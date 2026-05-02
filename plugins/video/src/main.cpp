// waywallen-video-renderer — Iter 2 GPU YUV→RGB pipeline.
//
// Iter 0/1: sw decode → CPU swscale to RGBA → staging upload of RGBA.
// Iter 2 (this file): sw decode → CPU swscale to NV12 → GPU NV12→RGBA
// via a compute shader (`waywallen::ffvk::YuvToRgba`). NV12 upload is
// 1.5 bytes/pixel vs RGBA's 4, so PCIe bandwidth drops ~60%; the YUV→RGB
// math also moves off the CPU. Iter 4 swaps the sw-decode front end for
// FFmpeg's vulkan hwdevice, after which the pipeline is end-to-end GPU.
//
// IPC plumbing (Init handshake, reader thread, negotiate handoff) is
// unchanged from Iter 0.

#include <waywallen-bridge/bridge.h>
#include <waywallen-bridge/extent_resolve.h>
#include <waywallen-bridge/ipc_v1.h>
#include <waywallen-bridge/pool.h>
#include <waywallen-bridge/probe_vk.h>

#include <presenter.hpp>
#include <vk_device.hpp>
#include <video_decoder.hpp>
#include <yuv_to_rgba.hpp>

#include <atomic>
#include <cerrno>
#include <chrono>
#include <condition_variable>
#include <csignal>
#include <cstdint>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <mutex>
#include <string>
#include <thread>

#include <sys/prctl.h>
#include <sys/socket.h>
#include <unistd.h>

namespace {

constexpr uint32_t SLOT_COUNT = 3;

struct Options {
    std::string ipc_path;
    std::string video_path;
    std::string render_node;   // e.g. "/dev/dri/renderD128"; empty → auto-pick
    uint32_t    width  { 1280 };
    uint32_t    height { 720 };
    bool        loop_file { true };
    bool        selftest { false };
};

[[noreturn]] void die(const std::string& msg) {
    std::fprintf(stderr, "waywallen-video-renderer: %s\n", msg.c_str());
    std::exit(1);
}

// SPAWN_VERSION 3: video path arrives via `--path`; everything else
// (loop_file, hwdec, render_node, fps, volume) rides on Init.settings
// kv. Keep `--no-loop` / `--render-node` as standalone-debug escape
// hatches (set them before init; daemon doesn't emit them).
Options parse_args(int argc, char** argv) {
    Options o;
    for (int i = 1; i < argc; ++i) {
        std::string a = argv[i];
        auto next = [&]() -> std::string {
            if (i + 1 >= argc) return {};
            return argv[++i];
        };
        if (a == "--ipc")              o.ipc_path = next();
        else if (a == "--path")        o.video_path = next();
        else if (a == "--no-loop")     o.loop_file = false;
        else if (a == "--render-node") o.render_node = next();
        else if (a == "--selftest")    { o.selftest = true; o.video_path = next(); }
        // Tolerate other `--key value` extras by skipping the value.
        else if (a.size() >= 2 && a[0] == '-' && a[1] == '-' && i + 1 < argc) {
            std::string nxt = argv[i + 1];
            if (!(nxt.size() >= 2 && nxt[0] == '-' && nxt[1] == '-')) ++i;
        }
    }
    return o;
}

const char* kv_get(const ww_kv_list_t& kv, const char* key) {
    for (uint32_t i = 0; i < kv.count; ++i) {
        if (kv.data[i].key && std::strcmp(kv.data[i].key, key) == 0)
            return kv.data[i].value;
    }
    return nullptr;
}

struct HostState {
    int                     sock { -1 };
    ww_pool_t              *pool { nullptr };
    std::atomic<bool>       shutdown { false };
    std::atomic<bool>       negotiated { false };
    std::atomic<bool>       paused { false };

    std::mutex              neg_mu;
    std::condition_variable neg_cv;
    bool                    neg_pending { false };
    ww_pool_directive_t     neg_directive {};

    std::atomic<bool>       loop_pending { false };
    std::atomic<bool>       loop_value { true };
};

void signal_shutdown(HostState& s) {
    s.shutdown.store(true, std::memory_order_release);
    s.neg_cv.notify_all();
}

void apply_control(HostState& host, ww_bridge_control_t& c) {
    switch (c.op) {
    case WW_REQ_INIT:
        std::fprintf(stderr,
                     "waywallen-video-renderer: unexpected late Init; ignoring\n");
        break;
    case WW_REQ_PLAY:
        host.paused.store(false, std::memory_order_release);
        host.neg_cv.notify_all();
        break;
    case WW_REQ_PAUSE:
        host.paused.store(true, std::memory_order_release);
        break;
    case WW_REQ_MOUSE:
    case WW_REQ_SET_FPS:
        break;
    case WW_REQ_APPLY_SETTINGS: {
        ww_bridge_apply_settings_t as {};
        if (ww_bridge_apply_settings_from_control(&c, &as) != 0) break;
        for (uint32_t i = 0; i < as.settings.count; ++i) {
            const char* key = as.settings.data[i].key;
            const char* val = as.settings.data[i].value;
            if (!key || !val) continue;
            if (std::strcmp(key, "loop_file") == 0) {
                bool enabled = !(std::strcmp(val, "no") == 0);
                host.loop_value.store(enabled, std::memory_order_release);
                host.loop_pending.store(true, std::memory_order_release);
            } else if (std::strcmp(key, "hwdec") == 0) {
                // Iter 2 is sw decode + GPU YUV→RGB; honoured in Iter 4.
            } else {
                std::fprintf(stderr,
                             "waywallen-video-renderer: ApplySettings: unknown key '%s'; ignoring\n",
                             key);
            }
        }
        ww_bridge_apply_settings_free(&as);
        host.neg_cv.notify_all();
        break;
    }
    case WW_REQ_SHUTDOWN:
        signal_shutdown(host);
        break;
    case WW_REQ_NEGOTIATE_BUFFERS: {
        const auto& nb = c.u.negotiate_buffers;
        ww_pool_directive_t d {};
        d.category    = nb.path;
        d.mem_source  = nb.mem_source;
        d.fourcc      = nb.fourcc;
        d.modifier    = nb.modifier;
        d.plane_count = nb.plane_count;
        d.sync_mode   = nb.sync_mode;
        d.color       = nb.color;
        d.mem_hint    = nb.mem_hint;
        d.count       = nb.count > 0 ? nb.count : SLOT_COUNT;
        if (d.count > SLOT_COUNT) d.count = SLOT_COUNT;
        {
            std::lock_guard<std::mutex> lk(host.neg_mu);
            host.neg_directive = d;
            host.neg_pending = true;
        }
        host.neg_cv.notify_all();
        break;
    }
    default:
        std::fprintf(stderr,
                     "waywallen-video-renderer: unknown control op %d\n",
                     static_cast<int>(c.op));
        break;
    }
}

// --selftest: open a video, decode one frame, run YuvToRgba against a
// throw-away VkImage we allocate ourselves. Validates that the GPU
// pipeline (device, shader compile/load, descriptor set, queue submit)
// works on this box before relying on the renderer in a real shell. No
// IPC, no daemon — strictly local. Returns 0 on success.
int run_selftest(const Options& opt) {
    if (opt.video_path.empty()) {
        std::fprintf(stderr,
                     "waywallen-video-renderer: --selftest needs a video path\n");
        return 1;
    }

    uint32_t even_w = opt.width  + (opt.width  & 1u);
    uint32_t even_h = opt.height + (opt.height & 1u);

    std::string verr;
    auto producer = waywallen::ffvk::Producer::create_with_render_node(
        even_w, even_h, opt.render_node, &verr);
    if (!producer) { std::fprintf(stderr, "selftest vk: %s\n", verr.c_str()); return 1; }
    auto yuv = waywallen::ffvk::YuvToRgba::create(
        producer->instance(), producer->physical_device(), producer->device(),
        producer->queue_family_index(), producer->queue(),
        even_w, even_h, &verr);
    if (!yuv) { std::fprintf(stderr, "selftest yuv: %s\n", verr.c_str()); return 1; }

    waywallen::ffvk::DecodeError derr;
    auto decoder = waywallen::ffvk::VideoDecoder::open_with_vk(
        opt.video_path, even_w, even_h, /*loop=*/false, *producer, &derr);
    if (!decoder) {
        std::fprintf(stderr, "selftest decode: %s\n", derr.message.c_str());
        return 1;
    }

    /* Allocate a private RGBA8 VkImage to convert into. */
    VkImage         dst_img = VK_NULL_HANDLE;
    VkDeviceMemory  dst_mem = VK_NULL_HANDLE;
    {
        VkImageCreateInfo ici {};
        ici.sType         = VK_STRUCTURE_TYPE_IMAGE_CREATE_INFO;
        ici.imageType     = VK_IMAGE_TYPE_2D;
        ici.format        = VK_FORMAT_R8G8B8A8_UNORM;
        ici.extent        = { even_w, even_h, 1 };
        ici.mipLevels     = 1;
        ici.arrayLayers   = 1;
        ici.samples       = VK_SAMPLE_COUNT_1_BIT;
        ici.tiling        = VK_IMAGE_TILING_OPTIMAL;
        ici.usage         = VK_IMAGE_USAGE_STORAGE_BIT
                          | VK_IMAGE_USAGE_TRANSFER_SRC_BIT;
        ici.sharingMode   = VK_SHARING_MODE_EXCLUSIVE;
        ici.initialLayout = VK_IMAGE_LAYOUT_UNDEFINED;
        if (vkCreateImage(producer->device(), &ici, nullptr, &dst_img) != VK_SUCCESS) {
            std::fprintf(stderr, "selftest vkCreateImage failed\n");
            return 1;
        }
        VkMemoryRequirements mr {};
        vkGetImageMemoryRequirements(producer->device(), dst_img, &mr);
        VkPhysicalDeviceMemoryProperties mp {};
        vkGetPhysicalDeviceMemoryProperties(producer->physical_device(), &mp);
        uint32_t type = UINT32_MAX;
        for (uint32_t i = 0; i < mp.memoryTypeCount; ++i) {
            if ((mr.memoryTypeBits & (1u << i))
                && (mp.memoryTypes[i].propertyFlags
                    & VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT)) {
                type = i; break;
            }
        }
        if (type == UINT32_MAX) {
            std::fprintf(stderr, "selftest no DEVICE_LOCAL memory\n");
            return 1;
        }
        VkMemoryAllocateInfo mai {};
        mai.sType           = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO;
        mai.allocationSize  = mr.size;
        mai.memoryTypeIndex = type;
        if (vkAllocateMemory(producer->device(), &mai, nullptr, &dst_mem) != VK_SUCCESS
            || vkBindImageMemory(producer->device(), dst_img, dst_mem, 0) != VK_SUCCESS) {
            std::fprintf(stderr, "selftest vkAllocateMemory/Bind failed\n");
            return 1;
        }
    }

    /* Decode one frame, convert it. */
    int sync_fd = -1;
    if (decoder->using_vk_frames()) {
        waywallen::ffvk::VkFrameView vkv {};
        auto fs = decoder->next_vk_frame(vkv, &derr);
        if (fs != waywallen::ffvk::FrameStatus::ok) {
            std::fprintf(stderr, "selftest next_vk_frame: %s\n", derr.message.c_str());
            return 1;
        }
        const auto cm = waywallen::ffvk::make_color_matrix(
            static_cast<waywallen::ffvk::ColorSpace>(vkv.colorspace),
            static_cast<waywallen::ffvk::ColorRange>(vkv.color_range));
        waywallen::ffvk::YuvToRgba::VkFrameImports im {};
        im.y_image          = vkv.img[0];
        im.uv_image         = vkv.plane_count > 1 ? vkv.img[1] : VK_NULL_HANDLE;
        im.y_sem            = vkv.sem[0];
        im.uv_sem           = vkv.plane_count > 1 ? vkv.sem[1] : vkv.sem[0];
        im.y_sem_val_in_out  = &vkv.sem_value[0];
        im.uv_sem_val_in_out = vkv.plane_count > 1 ? &vkv.sem_value[1] : &vkv.sem_value[0];
        im.y_layout_in_out   = &vkv.layout[0];
        im.uv_layout_in_out  = vkv.plane_count > 1 ? &vkv.layout[1] : &vkv.layout[0];
        im.y_qf_in_out       = &vkv.queue_family[0];
        im.uv_qf_in_out      = vkv.plane_count > 1 ? &vkv.queue_family[1] : &vkv.queue_family[0];
        im.src_w             = vkv.width;
        im.src_h             = vkv.height;
        im.bit_depth         = vkv.bit_depth;
        std::string yerr;
        sync_fd = yuv->convert_av_vk_frame(im, dst_img, even_w, even_h, cm, &yerr);
        if (sync_fd < 0) std::fprintf(stderr, "selftest convert: %s\n", yerr.c_str());
    } else {
        waywallen::ffvk::Nv12Frame frame;
        auto fs = decoder->next_frame(frame, &derr);
        if (fs != waywallen::ffvk::FrameStatus::ok) {
            std::fprintf(stderr, "selftest next_frame: %s\n", derr.message.c_str());
            return 1;
        }
        const auto cm = waywallen::ffvk::make_color_matrix(
            static_cast<waywallen::ffvk::ColorSpace>(frame.colorspace),
            static_cast<waywallen::ffvk::ColorRange>(frame.color_range));
        std::string yerr;
        sync_fd = yuv->convert_nv12(dst_img, even_w, even_h,
                                    frame.data.data(), frame.data.size(),
                                    cm, &yerr);
        if (sync_fd < 0) std::fprintf(stderr, "selftest convert: %s\n", yerr.c_str());
    }

    /* Wait for the conversion to complete via the sync_fd. We can poll
     * the fd with poll(2) since SYNC_FD is a kernel sync_file. */
    if (sync_fd >= 0) {
        ::close(sync_fd);
    }
    vkDeviceWaitIdle(producer->device());
    vkDestroyImage(producer->device(), dst_img, nullptr);
    vkFreeMemory(producer->device(), dst_mem, nullptr);

    if (sync_fd < 0) return 1;
    std::fprintf(stderr,
                 "waywallen-video-renderer: --selftest ok "
                 "(mode=%s, %ux%u)\n",
                 decoder->using_vk_frames() ? "shared-vk" : "sw",
                 even_w, even_h);
    return 0;
}

void reader_loop(HostState& host) {
    while (!host.shutdown.load(std::memory_order_acquire)) {
        ww_bridge_control_t msg {};
        int rc = ww_bridge_recv_control(host.sock, &msg);
        if (rc != 0) {
            if (!host.shutdown.load(std::memory_order_acquire)) {
                std::fprintf(stderr,
                             "waywallen-video-renderer: recv_control failed: %d\n", rc);
            }
            signal_shutdown(host);
            return;
        }
        apply_control(host, msg);
        ww_bridge_control_free(&msg);
    }
}

} // namespace


int main(int argc, char** argv) {
    Options opt = parse_args(argc, argv);
    if (opt.selftest) return run_selftest(opt);
    if (opt.ipc_path.empty()) die("--ipc <socket_path> is required");

    ::prctl(PR_SET_PDEATHSIG, SIGTERM);

    HostState host;
    host.sock = ww_bridge_connect(opt.ipc_path.c_str());
    if (host.sock < 0)
        die("ww_bridge_connect: " + std::string(std::strerror(-host.sock)));

    ww_bridge_init_t init {};
    if (int rc = ww_bridge_recv_init(host.sock, &init); rc < 0) {
        const char* reason = (rc == -EPROTO)
            ? "init: protocol error or unsupported spawn_version"
            : "init: recv failed";
        ww_bridge_send_init_nack(host.sock, init.spawn_version,
                                 WW_BRIDGE_SUPPORTED_SPAWN_VERSION,
                                 reason);
        ww_bridge_init_free(&init);
        die(std::string(reason) + " rc=" + std::to_string(rc));
    }
    uint32_t init_extent_w    = init.extent_w;
    uint32_t init_extent_h    = init.extent_h;
    uint32_t init_extent_mode = init.extent_mode;
    // SPAWN_VERSION 3: video path arrives via CLI argv `--path`
    // (already in opt.video_path). Init carries only extent + the
    // resolved settings kv list.
    if (const char* v = kv_get(init.settings, "loop_file")) {
        opt.loop_file = !(std::strcmp(v, "no") == 0);
    }
    if (opt.render_node.empty()) {
        if (const char* v = kv_get(init.settings, "render_node");
            v && *v) {
            opt.render_node = v;
        }
    }
    ww_bridge_init_free(&init);
    if (opt.video_path.empty())
        die("--path <video-file> is required");

    /* Probe the file's native dimensions before allocating any GPU
     * state, then resolve the daemon's extent hint against them.
     * `Producer::create_with_render_node` needs the final size up
     * front, so this has to happen here in main, not inside
     * VideoDecoder. */
    uint32_t native_w = 0, native_h = 0;
    {
        waywallen::ffvk::DecodeError perr;
        if (!waywallen::ffvk::VideoDecoder::probe_native(
                opt.video_path, &native_w, &native_h, &perr)) {
            die("probe_native " + opt.video_path + ": " + perr.message);
        }
    }
    ww_resolve_extent(init_extent_w, init_extent_h, init_extent_mode,
                      native_w, native_h, &opt.width, &opt.height);

    /* NV12 chroma is 4:2:0 → both extents must be even. The decoder
     * rounds up internally too; do it here so all our state agrees. */
    uint32_t even_w = opt.width  + (opt.width  & 1u);
    uint32_t even_h = opt.height + (opt.height & 1u);

    /* --- Vulkan device first, so the decoder can share it --- */
    std::string verr;
    auto producer = waywallen::ffvk::Producer::create_with_render_node(
        even_w, even_h, opt.render_node, &verr);
    if (!producer) die("vk producer: " + verr);

    /* --- Decoder: prefer the shared-VkDevice path that yields AVVkFrames
     *   directly (zero host bounce). On any setup failure the open helper
     *   falls back internally to FFmpeg-managed hwdevice + transfer_data,
     *   which still works through the sw `next_frame` path. */
    waywallen::ffvk::DecodeError derr;
    auto decoder = waywallen::ffvk::VideoDecoder::open_with_vk(
        opt.video_path, even_w, even_h, opt.loop_file, *producer, &derr);
    if (!decoder) die("decode " + opt.video_path + ": " + derr.message);
    host.loop_value.store(opt.loop_file, std::memory_order_release);

    ww_bridge_vk_dt_t vdt {};
    ww_bridge_vk_dt_load(&vdt, vkGetInstanceProcAddr, producer->instance());
    ww_bridge_vk_log_gpu_info("waywallen-video-renderer", &vdt,
                              producer->physical_device());

    auto yuv = waywallen::ffvk::YuvToRgba::create(
        producer->instance(),
        producer->physical_device(),
        producer->device(),
        producer->queue_family_index(),
        producer->queue(),
        even_w, even_h, &verr);
    if (!yuv) die("yuv_to_rgba: " + verr);

    /* --- Bridge pool --- */
    ww_pool_vulkan_init_t pool_init {};
    pool_init.instance              = producer->instance();
    pool_init.physical_device       = producer->physical_device();
    pool_init.device                = producer->device();
    pool_init.queue                 = producer->queue();
    pool_init.queue_family_index    = producer->queue_family_index();
    pool_init.get_instance_proc_addr =
        reinterpret_cast<void *(*)(void *, const char *)>(vkGetInstanceProcAddr);
    pool_init.device_uuid           = producer->device_uuid();
    pool_init.driver_uuid           = producer->driver_uuid();
    pool_init.drm_render_major      = producer->drm_render_major();
    pool_init.drm_render_minor      = producer->drm_render_minor();
    pool_init.drm_render_fd         = producer->drm_render_fd();
    /* The bridge's slot VkImage will be the dst of our compute shader's
     * storage-image binding, so it needs STORAGE usage in addition to
     * the default TRANSFER_DST. */
    pool_init.image_usage_flags     = VK_IMAGE_USAGE_STORAGE_BIT
                                    | VK_IMAGE_USAGE_TRANSFER_DST_BIT;

    if (int rc = ww_bridge_pool_create(WW_POOL_BACKEND_VULKAN, &pool_init, &host.pool);
        rc != 0)
        die("ww_bridge_pool_create failed: " + std::to_string(rc));

    if (int rc = ww_bridge_pool_advertise_caps(host.pool, host.sock,
                                               opt.width, opt.height,
                                               WW_MEM_HINT_DEVICE_LOCAL
                                               | WW_MEM_HINT_HOST_VISIBLE);
        rc != 0)
        die("ww_bridge_pool_advertise_caps failed: " + std::to_string(rc));
    std::fprintf(stderr,
                 "waywallen-video-renderer: ready (%ux%u, loop=%d, GPU YUV→RGB), "
                 "waiting for NegotiateBuffers\n",
                 even_w, even_h, opt.loop_file ? 1 : 0);

    std::thread reader([&]() { reader_loop(host); });

    /* Block until first NegotiateBuffers. */
    {
        std::unique_lock<std::mutex> lk(host.neg_mu);
        host.neg_cv.wait(lk, [&] {
            return host.neg_pending
                || host.shutdown.load(std::memory_order_acquire);
        });
        if (host.neg_pending && !host.shutdown.load(std::memory_order_acquire)) {
            ww_pool_directive_t d = host.neg_directive;
            host.neg_pending = false;
            lk.unlock();
            int rc = ww_bridge_pool_apply_directive(host.pool, host.sock, &d);
            if (rc != 0) {
                std::fprintf(stderr,
                             "waywallen-video-renderer: pool_apply_directive (initial) rc=%d\n", rc);
                signal_shutdown(host);
            } else {
                host.negotiated.store(true, std::memory_order_release);
            }
        }
    }

    std::fprintf(stderr,
                 "waywallen-video-renderer: decoder mode = %s\n",
                 decoder->using_vk_frames() ? "shared-VkDevice (zero-copy)"
                                            : "sw → CPU NV12 → GPU upload");

    /* --- Main loop ----------------------------------------------------- */
    uint32_t  slot = 0;
    waywallen::ffvk::Presenter presenter;  // Iter 3: PTS-driven pacing.
    waywallen::ffvk::Nv12Frame frame;
    waywallen::ffvk::VkFrameView vkv {};

    while (!host.shutdown.load(std::memory_order_acquire)) {
        {
            std::unique_lock<std::mutex> lk(host.neg_mu);
            if (host.neg_pending) {
                ww_pool_directive_t d = host.neg_directive;
                host.neg_pending = false;
                lk.unlock();
                int rc = ww_bridge_pool_apply_directive(host.pool, host.sock, &d);
                if (rc != 0) {
                    std::fprintf(stderr,
                                 "waywallen-video-renderer: pool_apply_directive (re) rc=%d\n", rc);
                    if (rc > 0) { signal_shutdown(host); break; }
                }
                slot = 0;
            }
        }

        if (host.loop_pending.exchange(false, std::memory_order_acq_rel)) {
            decoder->set_loop(host.loop_value.load(std::memory_order_acquire));
            // Loop toggled — let the presenter re-baseline on next frame.
            presenter.reset();
        }

        if (host.paused.load(std::memory_order_acquire)) {
            std::unique_lock<std::mutex> lk(host.neg_mu);
            host.neg_cv.wait(lk, [&] {
                return host.shutdown.load(std::memory_order_acquire)
                    || host.neg_pending
                    || !host.paused.load(std::memory_order_acquire);
            });
            continue;
        }

        waywallen::ffvk::DecodeError de;
        double frame_pts = -1.0;
        waywallen::ffvk::FrameStatus fs = decoder->using_vk_frames()
            ? decoder->next_vk_frame(vkv, &de)
            : decoder->next_frame(frame, &de);
        if (fs == waywallen::ffvk::FrameStatus::error) {
            std::fprintf(stderr,
                         "waywallen-video-renderer: decode error: %s\n",
                         de.message.c_str());
            signal_shutdown(host);
            break;
        }
        if (fs == waywallen::ffvk::FrameStatus::eof) {
            std::fprintf(stderr,
                         "waywallen-video-renderer: clean EOF (loop=off); idling until shutdown\n");
            std::unique_lock<std::mutex> lk(host.neg_mu);
            host.neg_cv.wait(lk, [&] {
                return host.shutdown.load(std::memory_order_acquire)
                    || host.neg_pending
                    || host.loop_pending.load(std::memory_order_acquire);
            });
            continue;
        }
        frame_pts = decoder->using_vk_frames() ? vkv.pts_seconds : frame.pts_seconds;

        // PTS pacing: sleep until this frame is due. Drop if too late.
        if (!presenter.present_frame(frame_pts)) continue;

        if (int rc = ww_bridge_pool_wait_slot_release(host.pool, slot, 250);
            rc != 0 && rc != -ETIME) {
            std::fprintf(stderr,
                         "waywallen-video-renderer: wait_slot_release(%u) rc=%d\n",
                         slot, rc);
        }

        ww_pool_slot_t s {};
        if (int rc = ww_bridge_pool_acquire_slot(host.pool, slot, &s); rc != 0) {
            std::fprintf(stderr,
                         "waywallen-video-renderer: acquire_slot(%u) failed: %d\n",
                         slot, rc);
            signal_shutdown(host);
            break;
        }
        if (!s.vk_image) {
            std::fprintf(stderr,
                         "waywallen-video-renderer: slot %u has no VkImage handle\n",
                         slot);
            signal_shutdown(host);
            break;
        }

        std::string yerr;
        int sync_fd = -1;
        const uint32_t cs_id = decoder->using_vk_frames() ? vkv.colorspace : frame.colorspace;
        const uint32_t cr_id = decoder->using_vk_frames() ? vkv.color_range : frame.color_range;
        const auto color_matrix = waywallen::ffvk::make_color_matrix(
            static_cast<waywallen::ffvk::ColorSpace>(cs_id),
            static_cast<waywallen::ffvk::ColorRange>(cr_id));
        if (decoder->using_vk_frames()) {
            waywallen::ffvk::YuvToRgba::VkFrameImports im {};
            im.y_image          = vkv.img[0];
            im.uv_image         = vkv.plane_count > 1 ? vkv.img[1] : VK_NULL_HANDLE;
            im.y_sem            = vkv.sem[0];
            im.uv_sem           = vkv.plane_count > 1 ? vkv.sem[1] : vkv.sem[0];
            im.y_sem_val_in_out  = &vkv.sem_value[0];
            im.uv_sem_val_in_out = vkv.plane_count > 1 ? &vkv.sem_value[1]
                                                       : &vkv.sem_value[0];
            im.y_layout_in_out   = &vkv.layout[0];
            im.uv_layout_in_out  = vkv.plane_count > 1 ? &vkv.layout[1]
                                                       : &vkv.layout[0];
            im.y_qf_in_out       = &vkv.queue_family[0];
            im.uv_qf_in_out      = vkv.plane_count > 1 ? &vkv.queue_family[1]
                                                       : &vkv.queue_family[0];
            im.src_w            = vkv.width;
            im.src_h            = vkv.height;
            im.bit_depth        = vkv.bit_depth;
            sync_fd = yuv->convert_av_vk_frame(
                im, reinterpret_cast<VkImage>(s.vk_image),
                s.width, s.height, color_matrix, &yerr);
        } else {
            sync_fd = yuv->convert_nv12(
                reinterpret_cast<VkImage>(s.vk_image),
                s.width, s.height,
                frame.data.data(), frame.data.size(),
                color_matrix, &yerr);
        }
        if (sync_fd < 0) {
            std::fprintf(stderr,
                         "waywallen-video-renderer: yuv conversion failed: %s\n",
                         yerr.c_str());
            signal_shutdown(host);
            break;
        }
        if (int rc = ww_bridge_pool_submit_slot(host.pool, host.sock, slot, sync_fd);
            rc != 0) {
            std::fprintf(stderr,
                         "waywallen-video-renderer: submit_slot rc=%d\n", rc);
            signal_shutdown(host);
            break;
        }

        slot = (slot + 1) % SLOT_COUNT;
    }

    if (reader.joinable()) {
        ::shutdown(host.sock, SHUT_RD);
        reader.join();
    }
    if (host.pool) ww_bridge_pool_destroy(host.pool);
    ww_bridge_close(host.sock);
    return 0;
}
