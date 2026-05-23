/* waywallen-bridge — C library for renderer subprocesses to talk to
 * the waywallen daemon over its IPC Unix-domain socket.
 *
 * This header layers length-prefix framing + SCM_RIGHTS fd passing
 * on top of the auto-generated per-message encoders/decoders in
 * <waywallen-bridge/ipc_v1.h>.
 *
 * Wire frame (same layout as waywallen-display-v1):
 *
 *     [u16 LE opcode][u16 LE total_length][body...]
 *
 * where total_length includes the 4-byte header. Ancillary fds ride
 * along on the same sendmsg/recvmsg call.
 *
 * Error conventions: all functions return 0 on success and a negative
 * value on failure. The negative is either a negated errno, or one of
 * the WW_ERR_* codes defined in <waywallen-bridge/ipc_v1.h>.
 *
 * Thread safety: none. Each socket is single-writer, single-reader
 * from the caller's perspective.
 */
#ifndef WAYWALLEN_BRIDGE_H
#define WAYWALLEN_BRIDGE_H

#include <waywallen-bridge/ipc_v1.h>
#include <waywallen-bridge/drm_fourcc.h>
#include <waywallen-bridge/protocol_bits.h>

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

/* -----------------------------------------------------------------------
 * Connection
 * ----------------------------------------------------------------------- */

/* Connect to the daemon's IPC socket at `socket_path`.
 * Returns the socket fd (>=0) on success, or a negative errno on failure. */
int ww_bridge_connect(const char *socket_path);

/* Close a bridge socket. Equivalent to close(fd). */
void ww_bridge_close(int sock);


/* -----------------------------------------------------------------------
 * Logging
 *
 * The bridge logs internal events (slot allocation failures, bind
 * errors, the per-directive bind_buffers diagnostic, …) to stderr by
 * default. Renderers using rstd::log (or any other logging framework)
 * install a callback to redirect them — same pattern as
 * waywallen_display_set_log_callback.
 * ----------------------------------------------------------------------- */

typedef enum ww_bridge_log_level {
    WW_BRIDGE_LOG_DEBUG = 0,
    WW_BRIDGE_LOG_INFO  = 1,
    WW_BRIDGE_LOG_WARN  = 2,
    WW_BRIDGE_LOG_ERROR = 3,
} ww_bridge_log_level_t;

typedef void (*ww_bridge_log_callback_t)(ww_bridge_log_level_t level,
                                         const char *msg,
                                         void *user_data);

/* Install a global log callback. Pass NULL to fall back to stderr.
 * Not thread-safe with concurrent log emission — call once at startup. */
void ww_bridge_set_log_callback(ww_bridge_log_callback_t cb, void *user_data);


/* -----------------------------------------------------------------------
 * Low-level framing
 * ----------------------------------------------------------------------- */

/* Send a pre-encoded message body. `opcode` is the message opcode,
 * `body` is the encoded bytes (use ww_*_encode into a ww_buf_t to fill),
 * `fds`/`n_fds` are optional SCM_RIGHTS ancillary fds.
 *
 * Hard limits: body_len + 4 must fit in u16 (65531 max body), n_fds <= 64.
 *
 * Returns 0 on success. */
int ww_bridge_send_frame(int sock,
                         uint16_t opcode,
                         const uint8_t *body,
                         size_t body_len,
                         const int *fds,
                         size_t n_fds);

/* Receive a single framed message. On success:
 *   - *opcode_out      is the message opcode
 *   - *body_out        is a freshly-malloc()d buffer of length *body_len_out
 *                      (caller must free() it)
 *   - fds_out[0..*n_fds_out]  gets any SCM_RIGHTS fds that arrived (caller
 *                             owns them; call close() when done)
 *
 * `fds_cap` bounds how many fds we'll accept; exceeding it is an error.
 * Returns 0 on success, a negative errno on I/O, or WW_ERR_* on protocol
 * errors. */
int ww_bridge_recv_frame(int sock,
                         uint16_t *opcode_out,
                         uint8_t **body_out,
                         size_t *body_len_out,
                         int *fds_out,
                         size_t fds_cap,
                         size_t *n_fds_out);


/* -----------------------------------------------------------------------
 * High-level event senders (subprocess -> daemon)
 * ----------------------------------------------------------------------- */

/* Emit `Ready`. Must be the first event after connecting. No fds.
 *
 * `drm_render_major` / `drm_render_minor` identify the DRM render-node
 * of the GPU the renderer's Vulkan/EGL/etc. instance picked, so the
 * daemon can decide whether each subscribed display is on the same GPU
 * (zero-copy) or a different GPU (must round-trip via HOST_VISIBLE).
 * Pass `(0, 0)` when the renderer cannot resolve its render node — the
 * daemon then conservatively assumes cross-GPU and forces HOST_VISIBLE
 * placement on every subsequent `configure_buffers`. */
int ww_bridge_send_ready(int sock,
                         uint32_t drm_render_major,
                         uint32_t drm_render_minor);

/* Emit `BindBuffers` carrying `m->count` DMA-BUF fds. `fds` must have
 * exactly `m->count` entries. */
int ww_bridge_send_bind_buffers(int sock,
                                const ww_evt_bind_buffers_t *m,
                                const int *fds);

/* Emit `FrameReady` with a single acquire sync_fd (dma_fence sync_file).
 * `m->release_point` names the timeline value the daemon will signal on
 * the producer-exported `release_syncobj` once every consumer has
 * finished sampling this frame.
 *
 * `sync_fd` semantics:
 *   - REQUIRED on the COMPAT_LINEAR / GPU_LINEAR path (the daemon
 *     negotiated a cross-vendor consumer; amdgpu and other importing
 *     drivers refuse to schedule a foreign dma-buf without an explicit
 *     dma_fence wait, manifesting as "Not enough memory for command
 *     submission" and a lost device).
 *   - OPTIONAL on OPTIMIZED same-GPU paths (driver-internal scheduling
 *     carries the producer-consumer dependency).
 *
 * The fd MUST be a SYNC_FD (dma_fence sync_file), produced via
 * `vkGetSemaphoreFdKHR(SYNC_FD)` on a binary semaphore created with
 * `VkExportSemaphoreCreateInfo.handleTypes = SYNC_FD`. OPAQUE_FD
 * timeline exports are NOT cross-vendor portable and MUST NOT be used
 * here. Pass `-1` when no fence is being signalled (same-GPU only). */
int ww_bridge_send_frame_ready(int sock,
                               const ww_evt_frame_ready_t *m,
                               int sync_fd);

/* Emit `ReleaseSyncobj` carrying the producer's timeline drm_syncobj fd.
 * Send exactly once per connection, after `Ready` and before any
 * `FrameReady`.
 *
 * The fd is a kernel `drm_syncobj` HANDLE_TO_FD export (timeline
 * semantics — points are u64 release_point values). It is wire-
 * compatible with `vkGetSemaphoreFdKHR(OPAQUE_FD)` on radv (which is
 * implemented as drm_syncobj), but the canonical producer for this fd
 * is the bridge itself via `ww_drm_syncobj_create` /
 * `ww_drm_syncobj_export_fd` — kernel ioctls work on every driver,
 * which Vulkan's OPAQUE_FD export does not (NVIDIA's OPAQUE_FD payload
 * is a private format incompatible with drm_syncobj).
 *
 * Consumer guidance: do NOT `vkImportSemaphoreFdKHR(OPAQUE_FD)` this fd
 * on a different-vendor GPU — it will be rejected with "Failed to
 * allocate semaphore device memory" or similar. Signal release via
 * `DRM_IOCTL_SYNCOBJ_TIMELINE_SIGNAL` (kernel ioctl, vendor-agnostic)
 * after the consumer's GPU work has retired. See cross_gpu.md.
 *
 * The caller retains ownership of `release_syncobj_fd` and is
 * responsible for closing it after this call returns (the kernel
 * dup'd it into SCM_RIGHTS). */
int ww_bridge_send_release_syncobj(int sock, int release_syncobj_fd);

/* Emit `FormatCaps` — the producer's modifier-negotiation declaration.
 * Send exactly once per connection, after `Ready` and before any
 * `BindBuffers`. Caller fills the parallel-array fields directly on
 * `m`; this helper is a thin encode + framed-send wrapper.
 *
 * Validation invariant (mirrored on the daemon side):
 *   m->modifiers.count == m->usages.count == m->plane_counts.count ==
 *   sum(m->mod_counts.data[0..fourccs.count])
 * The helper does NOT enforce this — the renderer must construct the
 * arrays consistently or the daemon's unflatten_caps will reject. */
int ww_bridge_send_format_caps(int sock, const ww_evt_format_caps_t *m);

/* Caller-friendly inputs for `ww_bridge_send_format_caps_v2`. Holds
 * pointers to caller-owned arrays (no copies, no ownership transfer)
 * plus the scalar negotiation knobs. The helper assembles the
 * `ww_evt_format_caps_t` wire shape from these fields, packs the two
 * 16-byte UUIDs as 4×u32 LE, and dispatches to
 * `ww_bridge_send_format_caps`.
 *
 * Length invariants (mirrored on the daemon's `unflatten_caps`):
 *   modifiers_count == plane_counts_count ==
 *   sum(mod_counts[0..fourccs_count])
 *
 * `device_uuid` / `driver_uuid`: pass NULL to send 16 zero bytes
 * (renderers without `VK_KHR_external_memory_capabilities` /
 * EGL_DEVICE_UUID_EXT do this). When non-NULL, must point at 16
 * readable bytes. */
typedef struct ww_format_caps_caller {
    const uint32_t *fourccs;        uint32_t fourccs_count;
    const uint32_t *mod_counts;     uint32_t mod_counts_count;
    const uint64_t *modifiers;      uint32_t modifiers_count;
    const uint32_t *plane_counts;   uint32_t plane_counts_count;
    const uint8_t  *device_uuid;    /* NULL or 16 bytes */
    const uint8_t  *driver_uuid;    /* NULL or 16 bytes */
    uint32_t        drm_render_major;
    uint32_t        drm_render_minor;
    uint32_t        mem_hints;
    uint32_t        sync_caps;
    uint32_t        color_caps;
    uint32_t        extent_max_w;
    uint32_t        extent_max_h;
} ww_format_caps_caller_t;

/* High-level wrapper around `ww_bridge_send_format_caps` that takes
 * caller-owned C arrays and the negotiation scalars in one struct.
 * Use this when assembling format caps from a probe loop — both
 * renderer plugins go through this path. */
int ww_bridge_send_format_caps_v2(int sock,
                                  const ww_format_caps_caller_t *m);

/* Emit `BindFailed` — non-terminal report that the renderer could not
 * satisfy a `negotiate_buffers` request. Daemon blacklists the
 * (fourcc, modifier) pair on this renderer and re-runs the picker. */
int ww_bridge_send_bind_failed(int sock,
                               uint32_t fourcc,
                               uint64_t modifier,
                               uint32_t reason,
                               const char *message);

/* Emit an `Error` event with a text message. */
int ww_bridge_send_error(int sock, const char *msg);

/* Emit `ReportState` — kv-list of renderer-published state the daemon
 * merges into its per-renderer view. Same wire shape as the inbound
 * `setting_changed` event but the other direction.
 *
 * Recognised keys (v1):
 *   `clear_color` — `"r,g,b,a"`, four floats in 0..=1, comma-separated;
 *                   feeds the daemon's display `set_config.clear_*`.
 *
 * Caller owns the `ww_kv_list_t` storage; the bridge reads it and
 * encodes — no ownership transfer. */
int ww_bridge_send_report_state(int sock,
                                const ww_evt_report_state_t *m);

/* Convenience: publish a single `clear_color = "r,g,b,a"` kv pair.
 * Components are clamped to `[0, 1]` and formatted with `%.6f`. The
 * caller is expected to dedupe against the previous published value
 * (cheap to keep four floats around per-renderer). */
int ww_bridge_send_report_state_clear_color(int sock,
                                            float r, float g, float b, float a);


/* -----------------------------------------------------------------------
 * Modifier negotiation
 *
 * Producer-side bookkeeping for the `format_caps` / `negotiate_buffers`
 * dance: a pinned (fourcc, modifier, plane_count) the slot pool is
 * currently allocated against, plus the full set of (modifier,
 * plane_count) tuples the producer can switch to via re-allocation.
 * ----------------------------------------------------------------------- */

/* One advertised (fourcc, modifier, plane_count) tuple. The daemon's
 * negotiator strict-equals plane_count when intersecting producer and
 * consumer caps, so producers must report truth — see
 * waywallen/src/negotiate.rs:432. */
typedef struct ww_format_entry {
    uint32_t fourcc;
    uint64_t modifier;
    uint32_t plane_count;
} ww_format_entry_t;

/* Producer-side negotiation snapshot. Owned by the caller; the
 * `advertised` array points at producer storage that outlives the
 * negotiation calls. The pinned (fourcc, modifier, plane_count) is
 * the one the slot pool is currently allocated against; on
 * `negotiate_buffers` the producer either re-allocates to a different
 * entry from `advertised` (and updates the pinned tuple) or replies
 * `bind_failed` to push the daemon to re-pick.
 *
 * Invariants:
 *   - The pinned (fourcc, modifier, plane_count) MUST appear in
 *     `advertised`.
 *   - Entries with the same `fourcc` MUST be contiguous in
 *     `advertised` (the format_caps flatten helper walks runs).
 *   - The pinned entry SHOULD be first within its fourcc's run, and
 *     the pinned fourcc SHOULD come before non-pinned fourccs — this
 *     lets the daemon's picker land on the pinned tuple in one round
 *     instead of bouncing through `bind_failed` retries. */
typedef struct ww_negotiation_state {
    uint32_t                  fourcc;
    uint64_t                  modifier;
    uint32_t                  plane_count;
    const ww_format_entry_t  *advertised;
    size_t                    advertised_count;
} ww_negotiation_state_t;

/* True (1) if a (fourcc, modifier) pair is anywhere in `advertised`.
 * False (0) otherwise. NULL `neg` returns 0. Replaces the linear-scan
 * "is this in our advertised set?" check producers do in their
 * NegotiateBuffers handlers. */
int ww_bridge_negotiation_contains(const ww_negotiation_state_t *neg,
                                   uint32_t                      fourcc,
                                   uint64_t                      modifier);

/* Populate a `ww_format_caps_caller_t` from the negotiation state plus
 * caller-provided scratch arrays. Walks `advertised` collapsing
 * contiguous same-fourcc runs into the wire format's
 * `(fourccs[], mod_counts[])` shape; relies on the
 * "same-fourcc-contiguous" invariant above.
 *
 * Scratch sizing (caller owns and outlives `out`); all sized to
 * `neg->advertised_count` for worst-case (one fourcc per entry):
 *   - `scratch_fourccs`      [advertised_count]
 *   - `scratch_mod_counts`   [advertised_count]
 *   - `scratch_modifiers`    [advertised_count]
 *   - `scratch_plane_counts` [advertised_count]
 *
 * Caller still fills the scalar negotiation knobs (sync_caps,
 * color_caps, mem_hints, extent_max, UUIDs, drm_render_*) on `out`
 * after this call. */
void ww_bridge_negotiation_fill_format_caps(
    const ww_negotiation_state_t *neg,
    uint32_t                     *scratch_fourccs,
    uint32_t                     *scratch_mod_counts,
    uint64_t                     *scratch_modifiers,
    uint32_t                     *scratch_plane_counts,
    ww_format_caps_caller_t      *out);


/* -----------------------------------------------------------------------
 * Renderer utilities
 *
 * Tiny helpers shared verbatim by every renderer subprocess. Kept in
 * the header so they're trivially inlineable across both C and C++
 * call sites.
 * ----------------------------------------------------------------------- */

/* Monotonic nanosecond timestamp for `frame_ready.ts_ns` and any other
 * place a renderer needs a steady-clock reading. Falls back to 0 on
 * the (vanishingly rare) clock_gettime failure rather than crashing —
 * the daemon treats ts_ns as advisory. */
uint64_t ww_bridge_now_ns(void);

/* (Removed in Step 3 of the renderer-Init refactor: the
 * `ww_bridge_skip_unknown_kv_arg` helper is gone — every in-tree
 * renderer now consumes spawn parameters from the typed `Init`
 * message and parses only `--ipc` plus a small fixed set of
 * standalone-debug flags. The wescene renderer in OWE uses argparse
 * and never linked the helper, so its migration is independent.) */


/* -----------------------------------------------------------------------
 * Diagnostics
 * ----------------------------------------------------------------------- */

/* One labeled row of the GPU info block. Both fields are
 * caller-owned, NUL-terminated. `value == NULL` is rendered as
 * "(null)" — useful when an EGL/Vulkan/GL string accessor returns
 * NULL. `label == NULL` is treated as the empty string. */
typedef struct ww_gpu_info_field {
    const char *label;
    const char *value;
} ww_gpu_info_field_t;

/* Print a "GPU info" diagnostic block to stderr, formatted as
 *
 *     {prefix}: GPU info
 *       {label}: {value}
 *       ...
 *
 * The label column auto-aligns to the widest label across all
 * supplied fields. Caller does the GPU-API queries (eglQueryString,
 * glGetString, vkGetPhysicalDeviceProperties ...) and hands the
 * already-fetched strings to this helper, so the bridge stays free
 * of any EGL/GL/Vulkan dependency. */
void ww_bridge_log_gpu_info(const char *prefix,
                            const ww_gpu_info_field_t *fields,
                            size_t n_fields);


/* -----------------------------------------------------------------------
 * High-level control receive (daemon -> subprocess)
 * ----------------------------------------------------------------------- */

/* Tagged union of all incoming inbound events from the daemon. `op`
 * selects which union arm is populated. String / kv fields inside are
 * heap-allocated — call `ww_bridge_control_free` when done. */
typedef struct ww_bridge_control {
    ww_event_in_op_t op;
    union {
        ww_evt_in_init_t               init;
        ww_evt_in_setting_changed_t    setting_changed;
        ww_evt_in_play_t               play;
        ww_evt_in_pause_t              pause;
        ww_evt_in_pointer_motion_t     pointer_motion;
        ww_evt_in_pointer_button_t     pointer_button;
        ww_evt_in_pointer_axis_t       pointer_axis;
        ww_evt_in_set_fps_t            set_fps;
        ww_evt_in_shutdown_t           shutdown;
        ww_evt_in_negotiate_buffers_t  negotiate_buffers;
    } u;
} ww_bridge_control_t;

/* Receive the next control message. Blocks until a full frame is
 * available or the peer closes. Returns 0 on success. */
int ww_bridge_recv_control(int sock, ww_bridge_control_t *out);

/* Free any heap allocations inside a decoded control message. Safe to
 * call on a zero-initialized struct. */
void ww_bridge_control_free(ww_bridge_control_t *msg);


/* -----------------------------------------------------------------------
 * Init handshake (v4) — typed spawn payload + structured rejection
 *
 * Step 1 of the renderer-Init refactor adds these helpers; renderers
 * are NOT yet wired to call them (Step 3 swaps each renderer's
 * `main.cpp`). The daemon already double-sends — legacy `--key value`
 * argv plus a typed `init` request immediately after accept(). When
 * Step 3 lands, every renderer's `main` will:
 *
 *   int sock = ww_bridge_connect(socket_path);
 *   ww_bridge_init_t init = {0};
 *   int rc = ww_bridge_recv_init(sock, &init);
 *   if (rc < 0) {
 *       ww_bridge_send_init_nack(sock, init.spawn_version,
 *                                WW_BRIDGE_SUPPORTED_SPAWN_VERSION,
 *                                "rejected");
 *       exit(1);
 *   }
 *   // ... use init.{extent_w,extent_h,fps,resource_*,settings}
 *   //     to drive Vulkan/EGL/mpv init ...
 *   ww_bridge_init_free(&init);
 * ----------------------------------------------------------------------- */

/* Spawn-payload version this build of the bridge handles. Bump when
 * the wire shape of `ww_evt_in_init_t` (or `ww_bridge_init_t`) changes;
 * `ww_bridge_recv_init` validates the value sent by the daemon
 * matches and returns -EPROTO otherwise. */
#define WW_BRIDGE_SUPPORTED_SPAWN_VERSION 4u

/* Interpretation of the daemon's `extent_w`/`extent_h` hints in
 * `ww_bridge_init_t`. See <waywallen-bridge/extent_resolve.h> for the
 * shared resolver every renderer should call after it knows its
 * content's intrinsic (native) size. */
typedef enum ww_extent_mode {
    /* `0` on either axis = "renderer fills this in from native"; both
     * 0 = fully native; both >0 = exact size requested. */
    WW_EXTENT_MODE_AS_GIVEN    = 0,
    /* The renderer chooses which native axis is shorter and fits it
     * to `max(extent_w, extent_h)`; the other axis scales to keep
     * the native aspect ratio. Used when the user picks a target
     * pixel size without specifying width vs height. */
    WW_EXTENT_MODE_FIT_SHORTER = 1,
} ww_extent_mode_t;

/* Caller-friendly view of the typed Init payload (SPAWN_VERSION 3).
 * The kv list is heap-owned (transferred from the underlying
 * `ww_evt_in_init_t` decode); call `ww_bridge_init_free` exactly once
 * after consumption.
 *
 * Resource path + plugin-specific extras (assets, workshop_id, …)
 * arrive on the renderer's CLI argv, NOT in this struct. fps,
 * test_pattern, volume, loop_file, hwdec, render_node, … all live
 * as keys in `settings` whenever the renderer's manifest declares
 * them; no scalar gets promoted to a typed wire field. */
typedef struct ww_bridge_init {
    uint32_t      spawn_version;
    uint32_t      extent_w;
    uint32_t      extent_h;
    uint32_t      extent_mode;       /* ww_extent_mode_t */
    ww_kv_list_t  settings;
    /* Raw JSON object forwarded from the DB row's
     * `user_property_overrides` column (project.json property key →
     * value). The renderer decodes once and routes through its
     * user-property pipeline. `NULL` / "" when no overrides exist.
     * Heap-owned; freed by `ww_bridge_init_free`. */
    char         *user_properties;
} ww_bridge_init_t;

/* Receive the daemon's typed `init` request and copy it into `out`.
 *
 * Behaviour:
 *   - Blocks until the next control frame arrives.
 *   - If the message is anything other than `WW_EVT_IN_INIT`, the body
 *     is freed and -EPROTO is returned.
 *   - If `spawn_version != WW_BRIDGE_SUPPORTED_SPAWN_VERSION`, the
 *     decoded value lands in `out->spawn_version` so the caller can
 *     forward it via `ww_bridge_send_init_nack`, and the function
 *     returns -EPROTO. The other heap fields are still populated and
 *     must be released via `ww_bridge_init_free`.
 *   - On success returns 0; ownership of every heap allocation
 *     transfers to the caller. */
int ww_bridge_recv_init(int sock, ww_bridge_init_t *out);

/* Release every heap allocation inside `out`. Safe to call on a
 * zero-initialized struct or after a successful free. Always returns
 * with `out` cleared. */
void ww_bridge_init_free(ww_bridge_init_t *out);

/* Emit an `init_nack` event back to the daemon (subprocess →
 * daemon). Used when `ww_bridge_recv_init` returns -EPROTO due to a
 * version mismatch or when the renderer cannot satisfy the typed
 * payload. The daemon kills the child and propagates `reason` to
 * the spawn caller.
 *
 * `reason` may be NULL (encoded as the empty string). Returns 0 on
 * success or a negative errno / WW_ERR_* on failure. */
int ww_bridge_send_init_nack(int sock,
                             uint32_t received_spawn_version,
                             uint32_t supported_spawn_version,
                             const char *reason);


/* -----------------------------------------------------------------------
 * setting_changed — runtime hot-reload of plugin settings
 *
 * The daemon fires `setting_changed` over a live renderer's IPC socket
 * whenever a non-identity plugin setting changes — e.g. `loop_file` /
 * `hwdec` for mpv, `volume` for wescene, `fps` for any plugin whose
 * manifest declares it. The whole payload is a kv list (same shape
 * as `init.settings`); no typed scalars are promoted.
 * ----------------------------------------------------------------------- */

/* Caller-friendly view of the setting_changed payload. Backing storage
 * lives in the underlying `ww_bridge_control_t::u.setting_changed`;
 * `_from_control` transfers ownership of the heap kv list into this
 * struct, so the caller MUST call `ww_bridge_setting_changed_free`
 * exactly once (NOT `ww_bridge_control_free`) when done. */
typedef struct ww_bridge_setting_changed {
    ww_kv_list_t settings;
} ww_bridge_setting_changed_t;

/* Peel the setting_changed typed view out of a generic control message.
 * On success, ownership of the heap kv list moves from `ctrl` into
 * `out`; `ctrl->u.setting_changed.settings` is zeroed so a follow-up
 * `ww_bridge_control_free(ctrl)` is a no-op for that arm.
 * Returns 0 on success, -EINVAL if `ctrl->op != WW_EVT_IN_SETTING_CHANGED`
 * or either pointer is NULL. */
int ww_bridge_setting_changed_from_control(ww_bridge_control_t *ctrl,
                                           ww_bridge_setting_changed_t *out);

/* Release every heap allocation inside `out`. Safe to call on a
 * zero-initialized struct or after a successful free. Always returns
 * with `out` cleared. */
void ww_bridge_setting_changed_free(ww_bridge_setting_changed_t *out);


/* -----------------------------------------------------------------------
 * Pointer events — optional, gated by manifest `events = ["pointer"]`
 *
 * The daemon forwards these only when the renderer's manifest declared
 * the "pointer" subscription. They are POD copies of the wire payload,
 * with no heap-owned fields, so no _free is required. The semantics of
 * (button, state, source, modifiers) mirror waywallen-display-v1's
 * pointer events.
 * ----------------------------------------------------------------------- */

typedef struct ww_bridge_pointer_motion {
    float    x;
    float    y;
    uint64_t timestamp_us;
    uint32_t modifiers;
} ww_bridge_pointer_motion_t;

typedef struct ww_bridge_pointer_button {
    float    x;
    float    y;
    uint32_t button;
    uint32_t state;
    uint64_t timestamp_us;
    uint32_t modifiers;
} ww_bridge_pointer_button_t;

typedef struct ww_bridge_pointer_axis {
    float    x;
    float    y;
    float    delta_x;
    float    delta_y;
    uint32_t source;
    uint64_t timestamp_us;
    uint32_t modifiers;
} ww_bridge_pointer_axis_t;

/* Peel a pointer-event view out of a generic control message. POD
 * copies — `ctrl` keeps no resources, so a trailing
 * `ww_bridge_control_free(ctrl)` is still safe (and a no-op for the
 * pointer arms). Returns -EINVAL when `ctrl->op` doesn't match. */
int ww_bridge_pointer_motion_from_control(ww_bridge_control_t *ctrl,
                                          ww_bridge_pointer_motion_t *out);
int ww_bridge_pointer_button_from_control(ww_bridge_control_t *ctrl,
                                          ww_bridge_pointer_button_t *out);
int ww_bridge_pointer_axis_from_control(ww_bridge_control_t *ctrl,
                                        ww_bridge_pointer_axis_t *out);


#ifdef __cplusplus
} /* extern "C" */
#endif

#endif /* WAYWALLEN_BRIDGE_H */
