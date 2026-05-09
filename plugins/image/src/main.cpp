// waywallen-image-renderer — FFmpeg-decoded still image renderer subprocess.
//
// All DMA-BUF allocation + modifier negotiation + drm_syncobj sync lives
// in <waywallen-bridge/pool.h>. This plugin owns:
//   - Vulkan instance + physical device + device + queue (for upload)
//   - Staging buffer + command buffer (uploads RGBA into a bridge slot)
//   - libav decode pipeline

import rstd.cppstd;
import rstd.log;
import wavsen.video;

#include <rstd/macro.hpp>

#include <waywallen-bridge/bridge.h>
#include <waywallen-bridge/drm_fourcc.h>
#include <waywallen-bridge/ipc_v1.h>
#include <waywallen-bridge/pool.h>
#include <waywallen-bridge/probe_vk.h>

#include "av_image.hpp"

#include <errno.h>
#include <signal.h>
#include <string.h>

#include <sys/prctl.h>
#include <sys/socket.h>
#include <unistd.h>

namespace {

struct Options {
    std::string ipc_path;
    std::string image_path;
    /* Daemon-supplied size hint. After decode they are overwritten with
     * the resolved render extent (`ww_resolve_extent`). */
    uint32_t    width { 1920 };
    uint32_t    height { 1080 };
    /* Wire-level interpretation of `width`/`height`. `0` = AS_GIVEN. */
    uint32_t    extent_mode { 0 };
    bool        decode_only { false };
    bool        vulkan_probe { false };
    // Test hook
    bool        print_caps { false };
    std::string render_node;
};

[[noreturn]] void die(const std::string& msg) {
    rstd_error("waywallen-image-renderer: {}", msg);
    std::exit(1);
}

// SPAWN_VERSION 3: argv carries the canonical `--path` for the image
// resource plus `--ipc`. Per-plugin runtime settings (fps, etc.) come
// in via `Init.settings` kv. Standalone-debug flags (`--decode-only`,
// `--vulkan-probe`, `--print-caps`) are still parsed here.
Options parse_args(int argc, char** argv) {
    Options o;
    for (int i = 1; i < argc; ++i) {
        std::string a = argv[i];
        auto next = [&]() -> std::string {
            if (i + 1 >= argc) return {};
            return argv[++i];
        };
        if (a == "--ipc")               o.ipc_path = next();
        else if (a == "--path")         o.image_path = next();
        else if (a == "--decode-only")  o.decode_only = true;
        else if (a == "--vulkan-probe") o.vulkan_probe = true;
        else if (a == "--print-caps")   o.print_caps = true;
        else if (a == "--render-node")  o.render_node = next();
        // Tolerate other `--key value` extras (none defined for image
        // today) by skipping their value.
        else if (a.size() >= 2 && a[0] == '-' && a[1] == '-' && i + 1 < argc) {
            std::string nxt = argv[i + 1];
            if (!(nxt.size() >= 2 && nxt[0] == '-' && nxt[1] == '-')) ++i;
        }
    }
    return o;
}


struct HostState {
    int                    sock { -1 };
    ww_pool_t             *pool { nullptr };
    std::atomic<bool>      shutdown { false };
    std::atomic<bool>      negotiated { false };

    /* Reader → main negotiate handoff. */
    std::mutex             neg_mu;
    std::condition_variable neg_cv;
    bool                   neg_pending { false };
    ww_pool_directive_t    neg_directive {};

    /* Cached RGBA buffer (kept alive across re-negotiations so we
     * can re-upload after a directive change). */
    const uint8_t*         rgba_data { nullptr };
    size_t                 rgba_size { 0 };
};

void signal_shutdown(HostState& s) {
    s.shutdown.store(true, std::memory_order_release);
    s.neg_cv.notify_all();
}

// Test hook: when WAYWALLEN_IMAGE_DUMP_DIR is set, write the RGBA8
// bytes the renderer is about to upload to the GPU to a file the
// orchestrator can compare against the consumer-side dump. The dump
// captures the *input* (post-decode, pre-staging) so it's always
// linear regardless of the picked DRM modifier — the consumer also
// dumps post-readback linear bytes, so byte-equality is meaningful.
//
// Filename: producer-{seq:06}-0x{fourcc:08x}-0x{modifier:016x}.bin
// Sidecar:  same name with .json — width/height/stride/fourcc/modifier.
static void maybe_dump_producer_frame(const HostState& host,
                                      const ww_pool_directive_t& d,
                                      const ww_pool_slot_t& s,
                                      uint64_t seq) {
    const char* dir = std::getenv("WAYWALLEN_IMAGE_DUMP_DIR");
    if (!dir || !*dir) return;
    if (!host.rgba_data || host.rgba_size == 0) return;

    char path[512];
    std::snprintf(path, sizeof(path),
                  "%s/producer-%06llu-0x%08x-0x%016llx.bin",
                  dir,
                  static_cast<unsigned long long>(seq),
                  d.fourcc,
                  static_cast<unsigned long long>(d.modifier));
    FILE* f = std::fopen(path, "wb");
    if (!f) {
        rstd_warn("waywallen-image-renderer: dump open {}: {}",
                  static_cast<const char*>(path),
                  static_cast<const char*>(::strerror(errno)));
        return;
    }
    size_t w = std::fwrite(host.rgba_data, 1, host.rgba_size, f);
    std::fclose(f);
    if (w != host.rgba_size) {
        rstd_warn("waywallen-image-renderer: dump short write {}/{} to {}",
                  w, host.rgba_size, static_cast<const char*>(path));
        return;
    }

    char sidecar[520];
    std::snprintf(sidecar, sizeof(sidecar),
                  "%s/producer-%06llu-0x%08x-0x%016llx.json",
                  dir,
                  static_cast<unsigned long long>(seq),
                  d.fourcc,
                  static_cast<unsigned long long>(d.modifier));
    FILE* sf = std::fopen(sidecar, "w");
    if (!sf) return;
    // Note: the dump is always tightly-packed RGBA8 (`width*height*4`
    // bytes) — that's the input format `decode_to_rgba` produces and
    // what `upload_into` accepts. The DMA-BUF stride/plane_offset are
    // the *destination* layout in the GPU buffer, which the consumer
    // reads back into the same tightly-packed shape; both sides' dumps
    // are therefore directly comparable.
    std::fprintf(sf,
                 "{\n"
                 "  \"kind\": \"producer\",\n"
                 "  \"seq\": %llu,\n"
                 "  \"fourcc\": \"0x%08x\",\n"
                 "  \"modifier\": \"0x%016llx\",\n"
                 "  \"width\": %u,\n"
                 "  \"height\": %u,\n"
                 "  \"stride\": %u,\n"
                 "  \"plane_offset\": %u,\n"
                 "  \"size\": %u,\n"
                 "  \"row_bytes\": %u,\n"
                 "  \"row_count\": %u,\n"
                 "  \"dump_layout\": \"tightly_packed_rgba8\"\n"
                 "}\n",
                 static_cast<unsigned long long>(seq),
                 d.fourcc,
                 static_cast<unsigned long long>(d.modifier),
                 s.width, s.height, s.stride, s.plane_offset, s.size,
                 s.width * 4u, s.height);
    std::fclose(sf);
}

bool upload_to_slot(HostState& host, wavsen::video::Producer& producer,
                    const ww_pool_directive_t& directive,
                    uint32_t slot_index) {
    ww_pool_slot_t s {};
    if (int rc = ww_bridge_pool_acquire_slot(host.pool, slot_index, &s);
        rc != 0) {
        rstd_error("waywallen-image-renderer: acquire_slot({}) failed: {}",
                   slot_index, rc);
        return false;
    }
    if (!s.vk_image) {
        rstd_error("waywallen-image-renderer: slot {} has no VkImage handle",
                   slot_index);
        return false;
    }

    static std::atomic<uint64_t> g_dump_seq { 0 };
    maybe_dump_producer_frame(host, directive, s,
                              g_dump_seq.fetch_add(1, std::memory_order_relaxed));

    auto upload_res = producer.upload_into(
        reinterpret_cast<VkImage>(s.vk_image),
        s.width, s.height,
        host.rgba_data, host.rgba_size);
    if (upload_res.is_err()) {
        rstd_error("waywallen-image-renderer: upload_into failed: {}",
                   std::move(upload_res).unwrap_err().message);
        return false;
    }
    int sync_fd = std::move(upload_res).unwrap();
    if (int rc = ww_bridge_pool_submit_slot(host.pool, host.sock, slot_index, sync_fd);
        rc != 0) {
        rstd_error("waywallen-image-renderer: submit_slot rc={}", rc);
        return false;
    }
    return true;
}

/* Apply a directive received from the daemon. After bridge brings the
 * slots up, upload our cached RGBA into slot 0 and submit one frame.
 * Static images: a single submit per (re-)negotiation is enough. */
void apply_negotiate_request(HostState& host, wavsen::video::Producer& producer,
                             const ww_pool_directive_t& d) {
    int rc = ww_bridge_pool_apply_directive(host.pool, host.sock, &d);
    if (rc != 0) {
        rstd_error("waywallen-image-renderer: pool_apply_directive failed: {}", rc);
        if (rc > 0) signal_shutdown(host);
        return;
    }
    if (!upload_to_slot(host, producer, d, 0)) {
        signal_shutdown(host);
        return;
    }
    host.negotiated.store(true, std::memory_order_release);
    rstd_info("waywallen-image-renderer: NegotiateBuffers honored "
              "(path={} mem_source={} modifier=0x{:016x}) — bind+frame emitted",
              d.category, d.mem_source,
              static_cast<unsigned long long>(d.modifier));
}

void apply_control(HostState& host, ww_bridge_control_t& c) {
    switch (c.op) {
    case WW_EVT_IN_INIT:
        // Init is consumed by ww_bridge_recv_init at the top of main
        // before the reader thread is even spawned. Anything that
        // arrives here is either a buggy daemon resending it or a
        // protocol violation; log and ignore to stay liberal.
        rstd_warn("waywallen-image-renderer: unexpected late Init; ignoring");
        break;
    case WW_EVT_IN_PLAY:
    case WW_EVT_IN_PAUSE:
    case WW_EVT_IN_SET_FPS:
    case WW_EVT_IN_POINTER_MOTION:
    case WW_EVT_IN_POINTER_BUTTON:
    case WW_EVT_IN_POINTER_AXIS:
        // image renderer doesn't subscribe to pointer events; daemon
        // already gates these (manifest sans `events`), but stay
        // permissive in case a misconfigured daemon forwards anyway.
        break;
    case WW_EVT_IN_SETTING_CHANGED: {
        // The image renderer's manifest declares no settings, so an
        // ApplySettings should arrive empty. If the daemon sends a
        // non-empty kv list (e.g. the user added a tunable key in
        // `settings.toml` that no schema declares), warn-log and
        // discard so we don't surprise the user with silent drops.
        ww_bridge_setting_changed_t as {};
        if (ww_bridge_setting_changed_from_control(&c, &as) == 0) {
            if (as.settings.count > 0) {
                rstd_warn("waywallen-image-renderer: ApplySettings with {} keys "
                          "but no hot-reloadable settings; ignoring",
                          as.settings.count);
            }
            ww_bridge_setting_changed_free(&as);
        }
        break;
    }
    case WW_EVT_IN_SHUTDOWN:
        signal_shutdown(host);
        break;
    case WW_EVT_IN_NEGOTIATE_BUFFERS: {
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
        /* Static image: one slot is enough. */
        d.count       = 1;
        {
            std::lock_guard<std::mutex> lk(host.neg_mu);
            host.neg_directive = d;
            host.neg_pending = true;
        }
        host.neg_cv.notify_all();
        break;
    }
    default:
        rstd_warn("waywallen-image-renderer: unknown control op {}",
                  static_cast<int>(c.op));
        break;
    }
}

void reader_loop(HostState& host) {
    while (!host.shutdown.load(std::memory_order_acquire)) {
        ww_bridge_control_t msg {};
        int rc = ww_bridge_recv_control(host.sock, &msg);
        if (rc != 0) {
            if (!host.shutdown.load(std::memory_order_acquire)) {
                rstd_error("waywallen-image-renderer: recv_control failed: {}", rc);
            }
            signal_shutdown(host);
            return;
        }
        apply_control(host, msg);
        ww_bridge_control_free(&msg);
    }
}

// ---------------------------------------------------------------------------
// --print-caps
// ---------------------------------------------------------------------------

// Emit a single JSON document on stdout that mirrors the
// `PeerCapsJson` shape consumed by `dmabuf_roundtrip_e2e`. Hand-rolled
// (no nlohmann dep) because the schema is tiny and stable. Keep the
// field names and ordering in sync with
// `displays/dump-test/src/main.rs::PeerCapsJson`.
//
// We don't have a public "query caps without a socket" entry point on
// the bridge pool; instead we build a Vulkan pool, hand it one end of
// a `socketpair(AF_UNIX)`, ask it to advertise, then drain the
// `format_caps` message on the other end and decode it.
static int print_caps_json(const Options& opt) {
    auto producer_res = wavsen::video::Producer::create(opt.width, opt.height);
    if (producer_res.is_err()) {
        rstd_error("waywallen-image-renderer: vk_producer: {}",
                   std::move(producer_res).unwrap_err().message);
        return 1;
    }
    auto producer = std::move(producer_res).unwrap();

    int sv[2] = { -1, -1 };
    if (::socketpair(AF_UNIX, SOCK_STREAM, 0, sv) != 0) {
        rstd_error("waywallen-image-renderer: socketpair: {}",
                   static_cast<const char*>(::strerror(errno)));
        return 1;
    }

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
    {
        ww_bridge_vk_dt_t dt {};
        ww_bridge_vk_dt_load(&dt, vkGetInstanceProcAddr, producer->instance());
        if (int rc = ww_bridge_vk_query_render_node(
                &dt, producer->physical_device(),
                &pool_init.drm_render_major, &pool_init.drm_render_minor);
            rc != 0) {
            rstd_warn("waywallen-image-renderer: drm render-node query failed ({}); "
                      "topology will be unknown to daemon", rc);
        }
    }
    pool_init.drm_render_fd         = producer->drm_render_fd();
    /* Image plugin uses vkCmdCopyBufferToImage (TRANSFER_DST feature)
     * to upload decoded pixels into the slot. */
    pool_init.image_usage_flags     = VK_IMAGE_USAGE_TRANSFER_DST_BIT;
    pool_init.format_feature_flags  = VK_FORMAT_FEATURE_TRANSFER_DST_BIT;

    ww_pool_t* pool = nullptr;
    if (int rc = ww_bridge_pool_create(WW_POOL_BACKEND_VULKAN, &pool_init, &pool);
        rc != 0) {
        rstd_error("waywallen-image-renderer: pool_create: {}", rc);
        ::close(sv[0]); ::close(sv[1]);
        return 1;
    }

    if (int rc = ww_bridge_pool_advertise_caps(pool, sv[0],
                                               opt.width, opt.height,
                                               WW_MEM_HINT_DEVICE_LOCAL
                                               | WW_MEM_HINT_HOST_VISIBLE);
        rc != 0) {
        rstd_error("waywallen-image-renderer: advertise_caps: {}", rc);
        ww_bridge_pool_destroy(pool);
        ::close(sv[0]); ::close(sv[1]);
        return 1;
    }

    /* Drain frames on sv[1] until we get the FormatCaps. The pool
     * writes (in order): Ready, ReleaseSyncobj (with a syncobj fd),
     * FormatCaps. */
    ww_evt_format_caps_t caps {};
    bool got_caps = false;
    for (int frame = 0; frame < 6 && !got_caps; ++frame) {
        uint16_t op = 0;
        uint8_t* body = nullptr;
        size_t body_len = 0;
        int fds[2] = { -1, -1 };
        size_t n_fds = 0;
        int rc = ww_bridge_recv_frame(sv[1], &op, &body, &body_len,
                                      fds, 2, &n_fds);
        if (rc != 0) {
            rstd_error("waywallen-image-renderer: recv_frame: {}", rc);
            break;
        }
        for (size_t i = 0; i < n_fds; ++i) {
            if (fds[i] >= 0) ::close(fds[i]);
        }
        if (op == WW_EVT_FORMAT_CAPS) {
            if (ww_evt_format_caps_decode(body, body_len, &caps) == 0) {
                got_caps = true;
            }
        }
        free(body);
    }

    ww_bridge_pool_destroy(pool);
    ::close(sv[0]); ::close(sv[1]);

    if (!got_caps) {
        rstd_error("waywallen-image-renderer: did not observe FormatCaps");
        return 1;
    }

    auto put_uuid = [](const ww_array_u32_t& a) -> std::string {
        // device_uuid / driver_uuid are 16 bytes packed as 4×u32 LE on
        // the wire. Unpack back to 16 bytes for the JSON output.
        uint8_t bytes[16] = {0};
        for (uint32_t i = 0; i < a.count && i < 4; ++i) {
            uint32_t v = a.data[i];
            bytes[i*4 + 0] = static_cast<uint8_t>( v        & 0xff);
            bytes[i*4 + 1] = static_cast<uint8_t>((v >>  8) & 0xff);
            bytes[i*4 + 2] = static_cast<uint8_t>((v >> 16) & 0xff);
            bytes[i*4 + 3] = static_cast<uint8_t>((v >> 24) & 0xff);
        }
        std::string s = "[";
        for (int i = 0; i < 16; ++i) {
            char buf[8];
            std::snprintf(buf, sizeof(buf), "%s%u", i ? "," : "", bytes[i]);
            s += buf;
        }
        s += "]";
        return s;
    };

    std::printf("{\n");
    std::printf("  \"by_fourcc\": {\n");
    size_t cursor = 0;
    for (uint32_t i = 0; i < caps.fourccs.count; ++i) {
        const uint32_t fc = caps.fourccs.data[i];
        const uint32_t n  = caps.mod_counts.data[i];
        std::printf("    \"0x%08x\": [", fc);
        for (uint32_t j = 0; j < n; ++j) {
            std::printf("%s\n      {\"modifier\": %llu, \"plane_count\": %u}",
                        j ? "," : "",
                        static_cast<unsigned long long>(caps.modifiers.data[cursor + j]),
                        caps.plane_counts.data[cursor + j]);
        }
        cursor += n;
        std::printf("\n    ]%s\n", (i + 1 < caps.fourccs.count) ? "," : "");
    }
    std::printf("  },\n");
    std::printf("  \"device_uuid\": %s,\n", put_uuid(caps.device_uuid).c_str());
    std::printf("  \"driver_uuid\": %s,\n", put_uuid(caps.driver_uuid).c_str());
    std::printf("  \"drm_render_major\": %u,\n", caps.drm_render_major);
    std::printf("  \"drm_render_minor\": %u,\n", caps.drm_render_minor);
    std::printf("  \"sync\": %u,\n", caps.sync_caps);
    std::printf("  \"color\": %u,\n", caps.color_caps);
    std::printf("  \"mem_hint\": %u,\n", caps.mem_hints);
    std::printf("  \"extent_max_w\": %u,\n", caps.extent_max_w);
    std::printf("  \"extent_max_h\": %u\n",  caps.extent_max_h);
    std::printf("}\n");
    std::fflush(stdout);
    ww_evt_format_caps_free(&caps);
    return 0;
}

} // namespace

// ---------------------------------------------------------------------------
// main
// ---------------------------------------------------------------------------

int main(int argc, char** argv) {
    static rstd::log::EnvLogger _logger;
    rstd::log::set_logger(_logger);
    rstd::log::set_max_level(_logger.filter());

    ww_bridge_set_log_callback(
        [](ww_bridge_log_level_t level, const char* msg, void*) {
            constexpr rstd::log::Level kMap[4] = {
                rstd::log::Level::Debug,
                rstd::log::Level::Info,
                rstd::log::Level::Warn,
                rstd::log::Level::Error,
            };
            auto lvl = kMap[(unsigned)level <= 3u ? (unsigned)level : 3u];
            auto args = rstd::fmt::Arguments::make("{}", msg);
            rstd::log::Record rec {
                rstd::log::Metadata { lvl, {} }, args,
            };
            rstd::log::log(rec);
        },
        nullptr);

    Options opt = parse_args(argc, argv);

    if (opt.print_caps) {
        return print_caps_json(opt);
    }

    if (opt.vulkan_probe) {
        auto prod_res = wavsen::video::Producer::create(opt.width, opt.height);
        if (prod_res.is_err()) {
            rstd_error("waywallen-image-renderer: vk_producer: {}",
                       std::move(prod_res).unwrap_err().message);
            return 1;
        }
        auto prod = std::move(prod_res).unwrap();
        rstd_info("waywallen-image-renderer: vulkan_probe ok drm_render={}:{}",
                  prod->drm_render_major(), prod->drm_render_minor());
        return 0;
    }

    if (opt.decode_only) {
        if (opt.image_path.empty()) die("--decode-only requires --image");
        ww_image::DecodeError derr;
        ww_image::RgbaBuf buf =
            ww_image::decode_to_rgba(opt.image_path, opt.width, opt.height,
                                     /* extent_mode = */ 0, &derr);
        if (buf.data.empty()) {
            rstd_error("waywallen-image-renderer: decode failed: {}", derr.message);
            return 1;
        }
        uint64_t sum = 0;
        for (uint8_t b : buf.data) sum += b;
        rstd_info("waywallen-image-renderer: decoded {}x{} stride={} "
                  "bytes={} pixel_sum={}",
                  buf.width, buf.height, buf.stride,
                  buf.data.size(),
                  static_cast<unsigned long long>(sum));
        return 0;
    }

    if (opt.ipc_path.empty()) die("--ipc <socket_path> is required");

    ::prctl(PR_SET_PDEATHSIG, SIGTERM);

    /* --- Connect first, then read the Init message ---
     *
     * Step 3: connect() moved to before any decode / Vulkan init so
     * the daemon's typed Init payload (extent + image path) drives
     * the GPU pipeline rather than CLI argv. The legacy `--image`/
     * `--width`/`--height` argv is still emitted by the daemon
     * double-send but we ignore it here. */
    HostState host;
    host.sock = ww_bridge_connect(opt.ipc_path.c_str());
    if (host.sock < 0)
        die("ww_bridge_connect: " + std::string(::strerror(-host.sock)));

    ww_bridge_init_t init {};
    if (int rc = ww_bridge_recv_init(host.sock, &init); rc < 0) {
        // Surface the rejection structured-ly so the daemon's spawn()
        // gets a useful error string. `init.spawn_version` is filled
        // by recv_init even on -EPROTO (version mismatch).
        const char* reason = (rc == -EPROTO)
            ? "init: protocol error or unsupported spawn_version"
            : "init: recv failed";
        ww_bridge_send_init_nack(host.sock, init.spawn_version,
                                 WW_BRIDGE_SUPPORTED_SPAWN_VERSION,
                                 reason);
        ww_bridge_init_free(&init);
        die(std::string(reason) + " rc=" + std::to_string(rc));
    }

    // SPAWN_VERSION 3: image path arrives via CLI argv `--path`
    // (already parsed into opt.image_path). Init carries only extent.
    opt.width       = init.extent_w;
    opt.height      = init.extent_h;
    opt.extent_mode = init.extent_mode;
    if (opt.render_node.empty()) {
        for (size_t i = 0; i < init.settings.count; ++i) {
            const ww_kv_t& kv = init.settings.data[i];
            if (kv.key && std::strcmp(kv.key, "render_node") == 0
                && kv.value && *kv.value) {
                opt.render_node = kv.value;
                break;
            }
        }
    }
    ww_bridge_init_free(&init);

    /* --- Decode + Vulkan setup --- */
    if (opt.image_path.empty()) die("--path <image-file> is required");
    ww_image::DecodeError derr;
    ww_image::RgbaBuf rgba_buf = ww_image::decode_to_rgba(
        opt.image_path, opt.width, opt.height, opt.extent_mode, &derr);
    if (rgba_buf.data.empty()) die("decode " + opt.image_path + ": " + derr.message);

    /* `decode_to_rgba` resolved the daemon's hint against the image's
     * native size; from here on we work with the resolved render
     * extent. */
    opt.width  = rgba_buf.width;
    opt.height = rgba_buf.height;

    auto producer_res = opt.render_node.empty()
        ? wavsen::video::Producer::create(opt.width, opt.height)
        : wavsen::video::Producer::create_with_render_node(
              opt.width, opt.height, opt.render_node);
    if (producer_res.is_err()) {
        die("vk_producer: " + std::move(producer_res).unwrap_err().message);
    }
    auto producer = std::move(producer_res).unwrap();

    /* GPU info diagnostic (uses bridge probe_vk dispatch table). */
    ww_bridge_vk_dt_t vdt {};
    ww_bridge_vk_dt_load(&vdt, vkGetInstanceProcAddr, producer->instance());
    ww_bridge_vk_log_gpu_info("waywallen-image-renderer", &vdt,
                              producer->physical_device());

    host.rgba_data = rgba_buf.data.data();
    host.rgba_size = rgba_buf.data.size();

    /* --- Bridge pool: hand over Vulkan handles --- */
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
    {
        ww_bridge_vk_dt_t dt {};
        ww_bridge_vk_dt_load(&dt, vkGetInstanceProcAddr, producer->instance());
        if (int rc = ww_bridge_vk_query_render_node(
                &dt, producer->physical_device(),
                &pool_init.drm_render_major, &pool_init.drm_render_minor);
            rc != 0) {
            rstd_warn("waywallen-image-renderer: drm render-node query failed ({}); "
                      "topology will be unknown to daemon", rc);
        }
    }
    pool_init.drm_render_fd         = producer->drm_render_fd();
    pool_init.image_usage_flags     = VK_IMAGE_USAGE_TRANSFER_DST_BIT;
    pool_init.format_feature_flags  = VK_FORMAT_FEATURE_TRANSFER_DST_BIT;

    if (int rc = ww_bridge_pool_create(WW_POOL_BACKEND_VULKAN, &pool_init, &host.pool);
        rc != 0)
        die("ww_bridge_pool_create failed: " + std::to_string(rc));

    /* Bridge sends ready + release_syncobj + format_caps in one go. */
    if (int rc = ww_bridge_pool_advertise_caps(host.pool, host.sock,
                                               opt.width, opt.height,
                                               WW_MEM_HINT_DEVICE_LOCAL | WW_MEM_HINT_HOST_VISIBLE);
        rc != 0)
        die("ww_bridge_pool_advertise_caps failed: " + std::to_string(rc));
    rstd_info("waywallen-image-renderer: ready, advertised caps, "
              "waiting for NegotiateBuffers");

    std::thread reader([&]() { reader_loop(host); });

    /* Main loop: drain pending negotiate requests as they come. Static
     * image: one upload per directive is enough; afterwards we just
     * wait for shutdown. */
    while (!host.shutdown.load(std::memory_order_acquire)) {
        std::unique_lock<std::mutex> lk(host.neg_mu);
        host.neg_cv.wait(lk, [&] {
            return host.neg_pending
                || host.shutdown.load(std::memory_order_acquire);
        });
        if (host.shutdown.load(std::memory_order_acquire)) break;
        if (host.neg_pending) {
            ww_pool_directive_t d = host.neg_directive;
            host.neg_pending = false;
            lk.unlock();
            apply_negotiate_request(host, *producer, d);
        }
    }

    if (reader.joinable()) {
        ::shutdown(host.sock, SHUT_RD);
        reader.join();
    }
    if (host.pool) ww_bridge_pool_destroy(host.pool);
    ww_bridge_close(host.sock);
    return 0;
}
