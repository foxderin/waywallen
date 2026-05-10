// waywallen-mpv-renderer — libmpv + GLES/EGL video renderer subprocess.
//
// All DMA-BUF allocation, modifier negotiation, drm_syncobj management,
// and bind_buffers/frame_ready emission lives in <waywallen-bridge/pool.h>.
// This subprocess owns:
//   - EGL display + GLES3 context (so libmpv can render through it)
//   - DRM render-node fd (handed to the bridge pool at init; pool then
//     owns the gbm_device + every dmabuf + EGLImage + export FBO)
//   - mpv intermediate framebuffer (RGBA8 native — decouples libmpv's
//     pipeline from any driver restriction on the export FBO).

#include <waywallen-bridge/bridge.h>
#include <waywallen-bridge/extent_resolve.h>
#include <waywallen-bridge/pool.h>
#include <waywallen-bridge/probe_egl.h>

#include <mpv/client.h>
#include <mpv/render.h>
#include <mpv/render_gl.h>

#include <EGL/egl.h>
#include <EGL/eglext.h>
#include <GLES3/gl3.h>
#include <GLES2/gl2ext.h>

#include <fcntl.h>
#include <errno.h>

#include <atomic>
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
#include <vector>

#include <sys/prctl.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/sysmacros.h>
#include <unistd.h>

namespace {

constexpr uint32_t SLOT_COUNT = 3;

struct Options {
    std::string ipc_path;
    std::string video_path;
    std::string render_node;   // e.g. "/dev/dri/renderD128"; empty → platform default
    uint32_t    width { 1920 };
    uint32_t    height { 1080 };
    bool        loop_file { true };
    bool        hwdec { true };
};

[[noreturn]] void die(const std::string& msg) {
    std::fprintf(stderr, "waywallen-mpv-renderer: %s\n", msg.c_str());
    std::exit(1);
}

// Step 3: spawn-time params (width/height/video path) come from the
// daemon's typed Init message; loop_file/hwdec come from
// Init.settings (manifest exposes them). Only `--ipc` and
// `--render-node` are still parsed here. `--render-node` stays on
// CLI because it picks the GPU before EGL init and is environment-
// level. Legacy daemon double-send args (`--width`, `--video`, ...)
// are silently ignored.
Options parse_args(int argc, char** argv) {
    Options o;
    for (int i = 1; i < argc; ++i) {
        std::string a = argv[i];
        auto next = [&]() -> std::string {
            if (i + 1 >= argc) return {};
            return argv[++i];
        };
        // SPAWN_VERSION 3: video path arrives via `--path`;
        // loop_file/hwdec/render_node come on Init.settings kv.
        // `--no-hwdec`/`--no-loop`/`--render-node` remain as
        // standalone-debug escape hatches (set before init).
        if (a == "--ipc") {
            o.ipc_path = next();
        } else if (a == "--path") {
            o.video_path = next();
        } else if (a == "--render-node") {
            o.render_node = next();
        } else if (a == "--no-hwdec") {
            o.hwdec = false;
        } else if (a == "--no-loop") {
            o.loop_file = false;
        } else if (a.size() >= 2 && a[0] == '-' && a[1] == '-' && i + 1 < argc) {
            // Tolerate other `--key value` extras by skipping the value.
            std::string nxt = argv[i + 1];
            if (!(nxt.size() >= 2 && nxt[0] == '-' && nxt[1] == '-')) ++i;
        }
    }
    return o;
}

// Lookup helper for ww_kv_list_t. Linear scan; the lists are tiny
// (manifest settings have <10 entries today).
static const char* kv_get(const ww_kv_list_t& kv, const char* key) {
    for (uint32_t i = 0; i < kv.count; ++i) {
        if (kv.data[i].key && std::strcmp(kv.data[i].key, key) == 0)
            return kv.data[i].value;
    }
    return nullptr;
}


// ---------------------------------------------------------------------------
// EGL / GLES — plugin-owned display + GLES3 context that bridge calls into
// during apply_directive. mpv intermediate FBO lives here too.
// ---------------------------------------------------------------------------

struct GlCtx {
    int                drm_fd { -1 };           // moved into bridge on pool create
    EGLDisplay         display { EGL_NO_DISPLAY };
    EGLContext         context { EGL_NO_CONTEXT };
    PFNEGLCREATESYNCKHRPROC          eglCreateSyncKHR { nullptr };
    PFNEGLDESTROYSYNCKHRPROC         eglDestroySyncKHR { nullptr };
    PFNEGLDUPNATIVEFENCEFDANDROIDPROC eglDupNativeFenceFDANDROID { nullptr };
    // mpv renders into this RGBA8 FBO. Per slot the bridge gives us
    // gl_export_fbo (DMA-BUF-backed); we glBlitFramebuffer from this
    // intermediate into export.
    GLuint             mpv_textures[SLOT_COUNT] { 0, 0, 0 };
    GLuint             mpv_fbos[SLOT_COUNT] { 0, 0, 0 };
};

bool egl_has_ext(const char* exts, const char* e) {
    return exts && std::strstr(exts, e) != nullptr;
}

void* must_egl_proc(const char* name) {
    void* p = reinterpret_cast<void*>(eglGetProcAddress(name));
    if (!p) die(std::string("eglGetProcAddress missing: ") + name);
    return p;
}

struct EglCandidate {
    EGLDeviceEXT dev;
    std::string  render_node;  // empty if device exposes neither RENDER_NODE nor DEVICE_FILE
};

// Build the candidate list for init_egl. Always goes through
// `eglQueryDevicesEXT` so the same code path drives both the
// `--render-node`-pinned case and the default. Behaviour:
//   * `opt.render_node` set → 1-element list, the device whose render
//     node (or card node) shares `st_rdev` with the requested path
//     (handles symlinks, `renderDN` ↔ `cardN` aliasing). Dies if no
//     enumerated device matches — explicit pinning is hard-fail.
//   * empty → every enumerated device in `eglQueryDevicesEXT` order.
//     init_egl will try them in turn and use the first that
//     successfully `eglInitialize`s. This unblocks multi-GPU hosts
//     where slot-0 happens to be a card our libEGL can't drive (e.g.
//     NVIDIA enumerated by Mesa with no NVIDIA ICD installed).
std::vector<EglCandidate> enumerate_egl_candidates(const Options& opt) {
    auto eglQueryDevicesEXT_ =
        reinterpret_cast<PFNEGLQUERYDEVICESEXTPROC>(
            eglGetProcAddress("eglQueryDevicesEXT"));
    auto eglQueryDeviceStringEXT_ =
        reinterpret_cast<PFNEGLQUERYDEVICESTRINGEXTPROC>(
            eglGetProcAddress("eglQueryDeviceStringEXT"));
    if (!eglQueryDevicesEXT_ || !eglQueryDeviceStringEXT_)
        die("EGL_EXT_device_enumeration / device_query missing");

    EGLDeviceEXT devs[16] = {};
    EGLint n_devs = 0;
    if (!eglQueryDevicesEXT_(16, devs, &n_devs) || n_devs <= 0)
        die("eglQueryDevicesEXT returned no devices");

    auto query_render_path = [&](EGLDeviceEXT d) -> std::string {
        // Prefer renderDN — render nodes are unprivileged-openable
        // on every driver, card nodes aren't.
        if (const char* p = eglQueryDeviceStringEXT_(
                d, EGL_DRM_RENDER_NODE_FILE_EXT)) return p;
        if (const char* p = eglQueryDeviceStringEXT_(
                d, EGL_DRM_DEVICE_FILE_EXT)) return p;
        return {};
    };

    if (!opt.render_node.empty()) {
        struct stat req_st = {};
        if (::stat(opt.render_node.c_str(), &req_st) != 0)
            die("--render-node: stat(" + opt.render_node + ") failed: "
                + std::strerror(errno));
        for (EGLint i = 0; i < n_devs; ++i) {
            const char* render = eglQueryDeviceStringEXT_(
                devs[i], EGL_DRM_RENDER_NODE_FILE_EXT);
            const char* card = eglQueryDeviceStringEXT_(
                devs[i], EGL_DRM_DEVICE_FILE_EXT);
            const char* paths[2] = { render, card };
            for (const char* p : paths) {
                if (!p) continue;
                struct stat st = {};
                if (::stat(p, &st) != 0) continue;
                if (st.st_rdev == req_st.st_rdev) {
                    std::string chosen_path = render ? render : card;
                    std::fprintf(stderr,
                        "waywallen-mpv-renderer: matched --render-node %s "
                        "to EGL device %d (%s)\n",
                        opt.render_node.c_str(), i, chosen_path.c_str());
                    return { { devs[i], chosen_path } };
                }
            }
        }
        die("--render-node: no EGL device exposes " + opt.render_node);
    }

    std::vector<EglCandidate> out;
    out.reserve(static_cast<size_t>(n_devs));
    for (EGLint i = 0; i < n_devs; ++i) {
        out.push_back({ devs[i], query_render_path(devs[i]) });
    }
    std::fprintf(stderr,
        "waywallen-mpv-renderer: enumerated %d EGL device(s); will try "
        "each until eglInitialize succeeds (pin with --render-node to skip)\n",
        n_devs);
    return out;
}

bool have_required_egl_exts(const char* exts) {
    return egl_has_ext(exts, "EGL_KHR_surfaceless_context")
        && egl_has_ext(exts, "EGL_EXT_image_dma_buf_import")
        && egl_has_ext(exts, "EGL_EXT_image_dma_buf_import_modifiers")
        && egl_has_ext(exts, "EGL_KHR_fence_sync")
        && egl_has_ext(exts, "EGL_ANDROID_native_fence_sync");
}

void init_egl(GlCtx& gl, const Options& opt) {
    auto eglGetPlatformDisplayEXT_ =
        reinterpret_cast<PFNEGLGETPLATFORMDISPLAYEXTPROC>(
            must_egl_proc("eglGetPlatformDisplayEXT"));

    auto candidates = enumerate_egl_candidates(opt);
    EGLint egl_major = 0;
    EGLint egl_minor = 0;

    for (size_t i = 0; i < candidates.size(); ++i) {
        const auto& c = candidates[i];
        const char* path_log = c.render_node.empty()
            ? "(no DRM path)" : c.render_node.c_str();
        std::fprintf(stderr,
            "waywallen-mpv-renderer: trying EGL device %zu/%zu (%s)\n",
            i, candidates.size(), path_log);

        EGLDisplay display = eglGetPlatformDisplayEXT_(
            EGL_PLATFORM_DEVICE_EXT, c.dev, nullptr);
        if (display == EGL_NO_DISPLAY) {
            std::fprintf(stderr,
                "waywallen-mpv-renderer: device %zu eglGetPlatformDisplayEXT "
                "failed; trying next\n", i);
            continue;
        }

        EGLint major = 0, minor = 0;
        if (!eglInitialize(display, &major, &minor)) {
            // Per spec the display isn't initialized on failure;
            // skip eglTerminate (it would be a no-op at best, error
            // at worst) and just abandon the handle.
            std::fprintf(stderr,
                "waywallen-mpv-renderer: device %zu eglInitialize failed; "
                "trying next\n", i);
            continue;
        }

        if (!have_required_egl_exts(eglQueryString(display, EGL_EXTENSIONS))) {
            std::fprintf(stderr,
                "waywallen-mpv-renderer: device %zu missing required EGL "
                "extensions (surfaceless / dma_buf_import / fence_sync); "
                "trying next\n", i);
            eglTerminate(display);
            continue;
        }

        if (c.render_node.empty()) {
            std::fprintf(stderr,
                "waywallen-mpv-renderer: device %zu exposes no DRM render "
                "node; trying next\n", i);
            eglTerminate(display);
            continue;
        }
        int fd = ::open(c.render_node.c_str(), O_RDWR | O_CLOEXEC);
        if (fd < 0) {
            std::fprintf(stderr,
                "waywallen-mpv-renderer: device %zu open(%s) failed: %s; "
                "trying next\n", i, c.render_node.c_str(),
                std::strerror(errno));
            eglTerminate(display);
            continue;
        }

        gl.display = display;
        gl.drm_fd  = fd;
        egl_major  = major;
        egl_minor  = minor;
        std::fprintf(stderr,
            "waywallen-mpv-renderer: opened DRM render node %s (fd=%d)\n",
            c.render_node.c_str(), gl.drm_fd);
        break;
    }
    if (gl.display == EGL_NO_DISPLAY)
        die("no EGL device could be initialized — see warnings above");

    if (!eglBindAPI(EGL_OPENGL_ES_API)) die("eglBindAPI(GLES) failed");

    EGLint config_attrs[] = {
        EGL_SURFACE_TYPE,    EGL_PBUFFER_BIT,
        EGL_RENDERABLE_TYPE, EGL_OPENGL_ES3_BIT,
        EGL_NONE,
    };
    EGLConfig config;
    EGLint    n_configs = 0;
    if (!eglChooseConfig(gl.display, config_attrs, &config, 1, &n_configs)
        || n_configs < 1)
        die("eglChooseConfig: no GLES3 pbuffer config");

    EGLint ctx_attrs[] = {
        EGL_CONTEXT_MAJOR_VERSION, 3,
        EGL_CONTEXT_MINOR_VERSION, 0,
        EGL_NONE,
    };
    gl.context = eglCreateContext(gl.display, config, EGL_NO_CONTEXT, ctx_attrs);
    if (gl.context == EGL_NO_CONTEXT) die("eglCreateContext failed");

    if (!eglMakeCurrent(gl.display, EGL_NO_SURFACE, EGL_NO_SURFACE, gl.context))
        die("eglMakeCurrent(surfaceless) failed");

    const GLubyte* gl_exts = glGetString(GL_EXTENSIONS);
    if (!gl_exts || !std::strstr(reinterpret_cast<const char*>(gl_exts),
                                 "GL_OES_EGL_image"))
        die("GL_OES_EGL_image missing");

    gl.eglCreateSyncKHR =
        reinterpret_cast<PFNEGLCREATESYNCKHRPROC>(must_egl_proc("eglCreateSyncKHR"));
    gl.eglDestroySyncKHR =
        reinterpret_cast<PFNEGLDESTROYSYNCKHRPROC>(must_egl_proc("eglDestroySyncKHR"));
    gl.eglDupNativeFenceFDANDROID =
        reinterpret_cast<PFNEGLDUPNATIVEFENCEFDANDROIDPROC>(
            must_egl_proc("eglDupNativeFenceFDANDROID"));

    ww_bridge_egl_dt_t dt {};
    ww_bridge_egl_dt_load(&dt, eglGetProcAddress);
    ww_bridge_egl_log_gpu_info("waywallen-mpv-renderer", &dt,
                               gl.display, egl_major, egl_minor);
}

// Build the per-slot mpv intermediate FBO. Lives outside the bridge
// pool because it's the source side of the blit (mpv renders into
// it; we glBlitFramebuffer into the bridge-owned export FBO). No
// DMA-BUF involvement here — guaranteed to succeed on every driver.
void init_mpv_fbos(GlCtx& gl, const Options& opt) {
    for (uint32_t i = 0; i < SLOT_COUNT; ++i) {
        glGenTextures(1, &gl.mpv_textures[i]);
        glBindTexture(GL_TEXTURE_2D, gl.mpv_textures[i]);
        glTexImage2D(GL_TEXTURE_2D, 0, GL_RGBA8,
                     static_cast<GLsizei>(opt.width),
                     static_cast<GLsizei>(opt.height),
                     0, GL_RGBA, GL_UNSIGNED_BYTE, nullptr);
        glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_LINEAR);
        glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_LINEAR);
        glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_WRAP_S, GL_CLAMP_TO_EDGE);
        glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_WRAP_T, GL_CLAMP_TO_EDGE);

        glGenFramebuffers(1, &gl.mpv_fbos[i]);
        glBindFramebuffer(GL_FRAMEBUFFER, gl.mpv_fbos[i]);
        glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0,
                               GL_TEXTURE_2D, gl.mpv_textures[i], 0);
        if (glCheckFramebufferStatus(GL_FRAMEBUFFER) != GL_FRAMEBUFFER_COMPLETE)
            die("mpv intermediate FBO incomplete");
    }
    glBindFramebuffer(GL_FRAMEBUFFER, 0);
}

void destroy_gl(GlCtx& gl) {
    if (gl.display != EGL_NO_DISPLAY) {
        for (uint32_t i = 0; i < SLOT_COUNT; ++i) {
            if (gl.mpv_fbos[i])     glDeleteFramebuffers(1, &gl.mpv_fbos[i]);
            if (gl.mpv_textures[i]) glDeleteTextures(1, &gl.mpv_textures[i]);
        }
        eglMakeCurrent(gl.display, EGL_NO_SURFACE, EGL_NO_SURFACE, EGL_NO_CONTEXT);
        if (gl.context != EGL_NO_CONTEXT)
            eglDestroyContext(gl.display, gl.context);
        eglTerminate(gl.display);
    }
    /* gl.drm_fd was moved into the bridge pool on create; do not close. */
}


// ---------------------------------------------------------------------------
// mpv
// ---------------------------------------------------------------------------

struct MpvState {
    mpv_handle*         mpv { nullptr };
    mpv_render_context* ctx { nullptr };
};

struct WakeState {
    std::mutex              mu;
    std::condition_variable cv;
    bool                    pending { false };
};

void on_mpv_render_update(void* ctx) {
    auto* w = static_cast<WakeState*>(ctx);
    {
        std::lock_guard<std::mutex> lk(w->mu);
        w->pending = true;
    }
    w->cv.notify_one();
}

void* mpv_get_proc_address(void* /*ctx*/, const char* name) {
    return reinterpret_cast<void*>(eglGetProcAddress(name));
}

void mpv_init(MpvState& m, const Options& opt, WakeState& wake) {
    m.mpv = mpv_create();
    if (!m.mpv) die("mpv_create failed");

    mpv_set_option_string(m.mpv, "vo",                     "libmpv");
    mpv_set_option_string(m.mpv, "audio",                  "no");
    mpv_set_option_string(m.mpv, "terminal",               "no");
    mpv_set_option_string(m.mpv, "msg-level",              "all=warn");
    mpv_set_option_string(m.mpv, "loop-file",              opt.loop_file ? "inf" : "no");
    mpv_set_option_string(m.mpv, "hwdec",                  opt.hwdec ? "auto-safe" : "no");
    mpv_set_option_string(m.mpv, "keep-open",              "always");
    mpv_set_option_string(m.mpv, "input-default-bindings", "no");
    mpv_set_option_string(m.mpv, "input-vo-keyboard",      "no");

    if (int rc = mpv_initialize(m.mpv); rc < 0)
        die(std::string("mpv_initialize: ") + mpv_error_string(rc));

    mpv_opengl_init_params gl_params {};
    gl_params.get_proc_address     = mpv_get_proc_address;
    gl_params.get_proc_address_ctx = nullptr;

    mpv_render_param create_params[] = {
        { MPV_RENDER_PARAM_API_TYPE,
          const_cast<char*>(MPV_RENDER_API_TYPE_OPENGL) },
        { MPV_RENDER_PARAM_OPENGL_INIT_PARAMS, &gl_params },
        { MPV_RENDER_PARAM_INVALID, nullptr },
    };
    if (int rc = mpv_render_context_create(&m.ctx, m.mpv, create_params); rc < 0)
        die(std::string("mpv_render_context_create: ") + mpv_error_string(rc));

    mpv_render_context_set_update_callback(m.ctx, on_mpv_render_update, &wake);

    if (!opt.video_path.empty()) {
        const char* cmd[] = { "loadfile", opt.video_path.c_str(), nullptr };
        if (int rc = mpv_command(m.mpv, cmd); rc < 0) {
            std::fprintf(stderr,
                         "waywallen-mpv-renderer: loadfile %s failed: %s\n",
                         opt.video_path.c_str(), mpv_error_string(rc));
        }
    }
}

bool mpv_render_into_intermediate(MpvState& m, GlCtx& gl, uint32_t slot,
                                  const Options& opt) {
    mpv_opengl_fbo fbo_info {};
    fbo_info.fbo             = static_cast<int>(gl.mpv_fbos[slot]);
    fbo_info.w               = static_cast<int>(opt.width);
    fbo_info.h               = static_cast<int>(opt.height);
    fbo_info.internal_format = 0;

    int flip_y = 0;
    mpv_render_param params[] = {
        { MPV_RENDER_PARAM_OPENGL_FBO, &fbo_info },
        { MPV_RENDER_PARAM_FLIP_Y,     &flip_y },
        { MPV_RENDER_PARAM_INVALID,    nullptr },
    };
    return mpv_render_context_render(m.ctx, params) >= 0;
}

void mpv_drain_events(MpvState& m, std::atomic<bool>& shutdown) {
    while (true) {
        mpv_event* ev = mpv_wait_event(m.mpv, 0.0);
        if (!ev || ev->event_id == MPV_EVENT_NONE) break;
        if (ev->event_id == MPV_EVENT_SHUTDOWN)
            shutdown.store(true, std::memory_order_release);
    }
}


// ---------------------------------------------------------------------------
// IPC + bridge pool
// ---------------------------------------------------------------------------

struct HostState {
    int                   sock { -1 };
    ww_pool_t            *pool { nullptr };
    std::atomic<bool>     shutdown { false };
    std::atomic<bool>     negotiated { false };
    /* Pending NegotiateBuffers handed off from reader → main; reader
     * can't make GL calls (context is bound to main). */
    std::mutex            neg_mu;
    bool                  neg_pending { false };
    ww_pool_directive_t   neg_directive {};
};

void wake_up(WakeState& w) {
    {
        std::lock_guard<std::mutex> lk(w.mu);
        w.pending = true;
    }
    w.cv.notify_one();
}

int export_acquire_sync_fd(GlCtx& gl) {
    EGLSyncKHR sync = gl.eglCreateSyncKHR(gl.display, EGL_SYNC_NATIVE_FENCE_ANDROID, nullptr);
    if (sync == EGL_NO_SYNC_KHR) return -1;
    glFlush();
    int fd = gl.eglDupNativeFenceFDANDROID(gl.display, sync);
    gl.eglDestroySyncKHR(gl.display, sync);
    return (fd == EGL_NO_NATIVE_FENCE_FD_ANDROID) ? -1 : fd;
}

void apply_negotiate_request(HostState& host, GlCtx& gl,
                             const ww_pool_directive_t& d) {
    int rc = ww_bridge_pool_apply_directive(host.pool, host.sock, &d);
    if (rc != 0) {
        std::fprintf(stderr,
                     "waywallen-mpv-renderer: pool_apply_directive failed: %d\n", rc);
        if (rc > 0) host.shutdown.store(true, std::memory_order_release);
        return;
    }
    /* On success the bridge has emitted bind_buffers. Build the per-
     * slot intermediate FBOs lazily on first negotiate. */
    static std::atomic<bool> intermediate_built { false };
    if (!intermediate_built.load(std::memory_order_acquire)) {
        Options dummy;
        dummy.width  = d.width;
        dummy.height = d.height;
        init_mpv_fbos(gl, dummy);
        intermediate_built.store(true, std::memory_order_release);
    }
    host.negotiated.store(true, std::memory_order_release);
}

bool drain_pending_negotiate(HostState& host, GlCtx& gl) {
    bool have = false;
    ww_pool_directive_t d {};
    {
        std::lock_guard<std::mutex> lk(host.neg_mu);
        if (host.neg_pending) {
            have = true;
            d = host.neg_directive;
            host.neg_pending = false;
        }
    }
    if (have) apply_negotiate_request(host, gl, d);
    return have;
}


// ---------------------------------------------------------------------------
// Control reader thread
// ---------------------------------------------------------------------------

void apply_control(HostState& s, MpvState& m, WakeState& wake,
                   ww_bridge_control_t& c) {
    switch (c.op) {
    case WW_EVT_IN_INIT:
        // Init is consumed at the top of main before the reader
        // thread starts. A late Init is either a buggy daemon
        // resending or a protocol violation; log and ignore.
        std::fprintf(stderr,
                     "waywallen-mpv-renderer: unexpected late Init; ignoring\n");
        break;
    case WW_EVT_IN_SETTING_CHANGED: {
        // v5 hot-reload: peel the typed view, apply known mpv knobs
        // via mpv_set_property (mpv option names use dashes, not the
        // underscore form the manifest uses), warn on the rest. fps
        // routes through the same mpv option as `--fps`/playback rate
        // limiting — the renderer's main loop is driven by mpv's own
        // clock so changing the fps cap takes effect on the next
        // decoded frame.
        ww_bridge_setting_changed_t as {};
        if (ww_bridge_setting_changed_from_control(&c, &as) != 0) break;
        for (uint32_t i = 0; i < as.settings.count; ++i) {
            const char* key = as.settings.data[i].key;
            const char* val = as.settings.data[i].value;
            if (!key || !val) continue;
            const char* mpv_opt = nullptr;
            if (std::strcmp(key, "loop_file") == 0)  mpv_opt = "loop-file";
            else if (std::strcmp(key, "hwdec") == 0) mpv_opt = "hwdec";
            else if (std::strcmp(key, "fps") == 0)   mpv_opt = "container-fps-override";
            else {
                std::fprintf(stderr,
                             "waywallen-mpv-renderer: ApplySettings: unknown key '%s'; ignoring\n",
                             key);
                continue;
            }
            // mpv_set_property with MPV_FORMAT_STRING wants a `char**`
            // pointing at the string pointer.
            char* mut_val = const_cast<char*>(val);
            int rc = mpv_set_property(m.mpv, mpv_opt, MPV_FORMAT_STRING, &mut_val);
            if (rc < 0) {
                std::fprintf(stderr,
                             "waywallen-mpv-renderer: mpv_set_property(%s=%s) rc=%d\n",
                             mpv_opt, val, rc);
            }
        }
        ww_bridge_setting_changed_free(&as);
        // Wake the main loop so a paused renderer picks up the
        // setting on the next iteration.
        wake_up(wake);
        break;
    }
    case WW_EVT_IN_PLAY: {
        int v = 0;
        mpv_set_property(m.mpv, "pause", MPV_FORMAT_FLAG, &v);
        break;
    }
    case WW_EVT_IN_PAUSE: {
        int v = 1;
        mpv_set_property(m.mpv, "pause", MPV_FORMAT_FLAG, &v);
        break;
    }
    case WW_EVT_IN_SET_FPS:
    case WW_EVT_IN_POINTER_MOTION:
    case WW_EVT_IN_POINTER_BUTTON:
    case WW_EVT_IN_POINTER_AXIS:
        break;
    case WW_EVT_IN_SHUTDOWN:
        s.shutdown.store(true, std::memory_order_release);
        break;
    case WW_EVT_IN_NEGOTIATE_BUFFERS: {
        const auto& nb = c.u.negotiate_buffers;
        ww_pool_directive_t d {};
        d.category   = nb.path;
        d.mem_source = nb.mem_source;
        d.fourcc     = nb.fourcc;
        d.modifier   = nb.modifier;
        d.plane_count = nb.plane_count;
        d.sync_mode  = nb.sync_mode;
        d.color      = nb.color;
        d.mem_hint   = nb.mem_hint;
        d.count      = nb.count > 0 ? nb.count : SLOT_COUNT;
        if (d.count > SLOT_COUNT) d.count = SLOT_COUNT; // bridge currently caps at 8
        {
            std::lock_guard<std::mutex> lk(s.neg_mu);
            s.neg_directive = d;
            s.neg_pending = true;
        }
        wake_up(wake);
        break;
    }
    default:
        std::fprintf(stderr,
                     "waywallen-mpv-renderer: unknown control op %d\n",
                     static_cast<int>(c.op));
        break;
    }
}

void reader_loop(HostState& s, MpvState& m, WakeState& wake) {
    while (!s.shutdown.load(std::memory_order_acquire)) {
        ww_bridge_control_t msg {};
        int                 rc = ww_bridge_recv_control(s.sock, &msg);
        if (rc != 0) {
            if (!s.shutdown.load(std::memory_order_acquire)) {
                std::fprintf(stderr,
                             "waywallen-mpv-renderer: recv_control failed: %d\n", rc);
            }
            s.shutdown.store(true, std::memory_order_release);
            wake_up(wake);
            return;
        }
        apply_control(s, m, wake, msg);
        ww_bridge_control_free(&msg);
    }
}

} // namespace


// ---------------------------------------------------------------------------
// main
// ---------------------------------------------------------------------------

int main(int argc, char** argv) {
    Options opt = parse_args(argc, argv);
    if (opt.ipc_path.empty()) die("--ipc <socket_path> is required");

    ::prctl(PR_SET_PDEATHSIG, SIGTERM);

    /* --- Connect first, then read Init ---
     *
     * Step 3: connect() moved to before EGL init and mpv_init. The
     * daemon's typed Init payload carries extent + video path +
     * settings (loop_file / hwdec) and drives the rest of
     * setup. */
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

    // SPAWN_VERSION 3: video path arrives via CLI argv `--path`
    // (already in opt.video_path). Init carries only extent +
    // settings kv.
    uint32_t init_extent_w    = init.extent_w;
    uint32_t init_extent_h    = init.extent_h;
    uint32_t init_extent_mode = init.extent_mode;
    /* Sane placeholder until libmpv reports `dwidth`/`dheight`. */
    opt.width  = init_extent_w  ? init_extent_w  : 1280u;
    opt.height = init_extent_h  ? init_extent_h  : 720u;

    // settings → typed knobs. CLI escape hatches (--no-hwdec /
    // --no-loop) already applied in parse_args; only override when
    // the user did NOT pass them. We honour CLI-supplied false by
    // detecting "still at default true" — same effect because the
    // dev flags are sticky.
    if (const char* v = kv_get(init.settings, "loop_file")) {
        // mpv understands "inf" / "no" / "yes" / a count. Treat any
        // non-"no" value as loop=true to preserve old semantics; the
        // mpv_set_option_string call below uses the raw value.
        opt.loop_file = !(std::strcmp(v, "no") == 0);
    }
    if (const char* v = kv_get(init.settings, "hwdec")) {
        opt.hwdec = !(std::strcmp(v, "no") == 0);
    }
    // render_node is identity-tagged in the manifest — a change forces
    // a respawn, so init_egl below picks up the right GPU on each spawn.
    // CLI `--render-node` wins over the Init-supplied value (dev escape
    // hatch for standalone debug runs).
    if (opt.render_node.empty()) {
        if (const char* v = kv_get(init.settings, "render_node");
            v && *v) {
            opt.render_node = v;
        }
    }
    ww_bridge_init_free(&init);

    if (opt.video_path.empty())
        die("--path <video-file> is required");

    GlCtx gl;
    init_egl(gl, opt);

    // Resolve DRM render-node major/minor from the EGL-bound DRM fd.
    uint32_t drm_render_major = 0, drm_render_minor = 0;
    {
        struct stat st;
        if (gl.drm_fd >= 0 && ::fstat(gl.drm_fd, &st) == 0) {
            drm_render_major = major(st.st_rdev);
            drm_render_minor = minor(st.st_rdev);
        }
    }

    WakeState wake;
    MpvState  mpv;
    mpv_init(mpv, opt, wake);

    /* Block until the first FILE_LOADED event so we can read the
     * stream's native `dwidth`/`dheight` and resolve the daemon's
     * extent hint against them. We do this before `advertise_caps`
     * so the bridge pool, FBOs and consumer all agree on a single
     * size from the start. A 5s timeout falls back to whatever
     * placeholder is currently in `opt` and logs a warning — better
     * to render at the wrong size than to hang the spawn. */
    {
        int64_t  native_w = 0, native_h = 0;
        bool     loaded   = false;
        auto deadline = std::chrono::steady_clock::now() + std::chrono::seconds(5);
        while (!loaded && std::chrono::steady_clock::now() < deadline) {
            mpv_event* ev = mpv_wait_event(mpv.mpv, 0.05);
            if (!ev) continue;
            if (ev->event_id == MPV_EVENT_NONE) continue;
            if (ev->event_id == MPV_EVENT_SHUTDOWN) {
                die("mpv shut down before FILE_LOADED");
            }
            if (ev->event_id == MPV_EVENT_FILE_LOADED) {
                mpv_get_property(mpv.mpv, "dwidth",  MPV_FORMAT_INT64, &native_w);
                mpv_get_property(mpv.mpv, "dheight", MPV_FORMAT_INT64, &native_h);
                loaded = true;
            }
        }
        if (!loaded) {
            std::fprintf(stderr,
                         "waywallen-mpv-renderer: timeout waiting for "
                         "FILE_LOADED; using daemon extent hint as-is "
                         "(%ux%u)\n", opt.width, opt.height);
        } else {
            ww_resolve_extent(init_extent_w, init_extent_h, init_extent_mode,
                              static_cast<uint32_t>(native_w > 0 ? native_w : 0),
                              static_cast<uint32_t>(native_h > 0 ? native_h : 0),
                              &opt.width, &opt.height);
        }
    }

    // Hand the EGL display + drm_fd off to the bridge pool. Bridge
    // takes ownership of drm_fd (we won't close it on destroy_gl).
    ww_pool_egl_gbm_init_t pool_init {};
    pool_init.egl_display       = gl.display;
    pool_init.drm_render_fd     = gl.drm_fd;
    pool_init.get_proc_address  = reinterpret_cast<void *(*)(const char *)>(eglGetProcAddress);
    pool_init.drm_render_major  = drm_render_major;
    pool_init.drm_render_minor  = drm_render_minor;
    if (int rc = ww_bridge_pool_create(WW_POOL_BACKEND_EGL_GBM,
                                       &pool_init, &host.pool);
        rc != 0)
        die("ww_bridge_pool_create failed: " + std::to_string(rc));
    /* drm_fd lifetime is now the pool's. */
    gl.drm_fd = -1;

    // Probe + advertise format_caps. Bridge sends ready, release_syncobj,
    // and format_caps in the right order.
    if (int rc = ww_bridge_pool_advertise_caps(host.pool, host.sock,
                                               opt.width, opt.height,
                                               WW_MEM_HINT_HOST_VISIBLE);
        rc != 0)
        die("ww_bridge_pool_advertise_caps failed: " + std::to_string(rc));

    // libmpv composites video over an opaque black surface; surface
    // it to the daemon so letterbox bars match.
    if (int rc = ww_bridge_send_report_state_clear_color(
            host.sock, 0.0f, 0.0f, 0.0f, 1.0f);
        rc != 0) {
        rstd_warn("waywallen-mpv-renderer: report_state(clear_color) failed ({})", rc);
    }

    std::thread reader([&]() { reader_loop(host, mpv, wake); });

    // Block until first NegotiateBuffers lands.
    while (!host.negotiated.load(std::memory_order_acquire)
           && !host.shutdown.load(std::memory_order_acquire)) {
        {
            std::unique_lock<std::mutex> lk(wake.mu);
            wake.cv.wait(lk, [&] {
                return wake.pending
                    || host.shutdown.load(std::memory_order_acquire);
            });
            wake.pending = false;
        }
        if (host.shutdown.load(std::memory_order_acquire)) break;
        drain_pending_negotiate(host, gl);
    }

    uint32_t slot = 0;
    while (!host.shutdown.load(std::memory_order_acquire)) {
        {
            std::unique_lock<std::mutex> lk(wake.mu);
            wake.cv.wait(lk, [&] {
                return wake.pending
                    || host.shutdown.load(std::memory_order_acquire);
            });
            wake.pending = false;
        }
        if (host.shutdown.load(std::memory_order_acquire)) break;

        drain_pending_negotiate(host, gl);

        mpv_drain_events(mpv, host.shutdown);
        if (host.shutdown.load(std::memory_order_acquire)) break;

        const uint64_t update = mpv_render_context_update(mpv.ctx);
        if (!(update & MPV_RENDER_UPDATE_FRAME)) continue;

        // Producer back-pressure — block until the prior use of this
        // slot has been signaled, with a 250ms cap. Failure is
        // logged but we proceed (running ahead is preferable to
        // stalling mpv's clock).
        if (int rc = ww_bridge_pool_wait_slot_release(host.pool, slot, 250);
            rc != 0 && rc != -ETIME) {
            std::fprintf(stderr,
                         "waywallen-mpv-renderer: wait_slot_release(%u) rc=%d\n",
                         slot, rc);
        }

        // Render mpv into the intermediate FBO, then blit into the
        // bridge-owned export FBO.
        if (!mpv_render_into_intermediate(mpv, gl, slot, opt)) continue;

        ww_pool_slot_t s {};
        if (int rc = ww_bridge_pool_acquire_slot(host.pool, slot, &s); rc != 0) {
            std::fprintf(stderr,
                         "waywallen-mpv-renderer: acquire_slot(%u) failed: %d\n",
                         slot, rc);
            host.shutdown.store(true, std::memory_order_release);
            break;
        }

        glBindFramebuffer(GL_READ_FRAMEBUFFER, gl.mpv_fbos[slot]);
        glBindFramebuffer(GL_DRAW_FRAMEBUFFER, s.gl_export_fbo);
        glBlitFramebuffer(
            0, 0, static_cast<GLint>(opt.width), static_cast<GLint>(opt.height),
            0, 0, static_cast<GLint>(opt.width), static_cast<GLint>(opt.height),
            GL_COLOR_BUFFER_BIT, GL_NEAREST);
        glBindFramebuffer(GL_FRAMEBUFFER, 0);

        int sync_fd = export_acquire_sync_fd(gl);
        if (sync_fd < 0) {
            std::fprintf(stderr,
                         "waywallen-mpv-renderer: export_acquire_sync_fd failed; shutting down\n");
            host.shutdown.store(true, std::memory_order_release);
            break;
        }

        if (int rc = ww_bridge_pool_submit_slot(host.pool, host.sock, slot, sync_fd);
            rc != 0) {
            std::fprintf(stderr,
                         "waywallen-mpv-renderer: pool_submit_slot rc=%d\n", rc);
            host.shutdown.store(true, std::memory_order_release);
            break;
        }

        slot = (slot + 1) % SLOT_COUNT;
    }

    // --- Shutdown ---------------------------------------------------------
    glFinish();

    if (mpv.ctx) mpv_render_context_free(mpv.ctx);
    if (mpv.mpv) mpv_terminate_destroy(mpv.mpv);

    if (reader.joinable()) {
        ::shutdown(host.sock, SHUT_RD);
        reader.join();
    }
    ww_bridge_close(host.sock);

    if (host.pool) ww_bridge_pool_destroy(host.pool);
    destroy_gl(gl);
    return 0;
}
