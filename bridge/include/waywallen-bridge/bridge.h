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
 * finished sampling this frame. */
int ww_bridge_send_frame_ready(int sock,
                               const ww_evt_frame_ready_t *m,
                               int sync_fd);

/* Emit `ReleaseSyncobj` carrying the producer's exported timeline
 * drm_syncobj fd. Send exactly once per connection, after `Ready` and
 * before any `FrameReady`. The fd is the OPAQUE_FD export of a Vulkan
 * TIMELINE semaphore on the renderer's `VkDevice`. The caller retains
 * ownership of `release_syncobj_fd` and is responsible for closing it
 * after this call returns (the kernel dup'd it into SCM_RIGHTS). */
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
 *   modifiers_count == usages_count == plane_counts_count ==
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
    const uint32_t *usages;         uint32_t usages_count;
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
 *   - `scratch_usages`       [advertised_count]
 *
 * `usage` is replicated to every entry of `scratch_usages` (typical
 * value: `WW_USAGE_SAMPLED`). Caller still fills the scalar
 * negotiation knobs (sync_caps, color_caps, mem_hints, extent_max,
 * UUIDs, drm_render_*) on `out` after this call. */
void ww_bridge_negotiation_fill_format_caps(
    const ww_negotiation_state_t *neg,
    uint32_t                      usage,
    uint32_t                     *scratch_fourccs,
    uint32_t                     *scratch_mod_counts,
    uint64_t                     *scratch_modifiers,
    uint32_t                     *scratch_plane_counts,
    uint32_t                     *scratch_usages,
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

/* Tagged union of all incoming control requests. `op` selects which
 * union arm is populated. String fields inside are heap-allocated —
 * call `ww_bridge_control_free` when done. */
typedef struct ww_bridge_control {
    ww_request_op_t op;
    union {
        ww_req_init_t               init;
        ww_req_apply_settings_t     apply_settings;
        ww_req_play_t               play;
        ww_req_pause_t              pause;
        ww_req_mouse_t              mouse;
        ww_req_set_fps_t            set_fps;
        ww_req_shutdown_t           shutdown;
        ww_req_negotiate_buffers_t  negotiate_buffers;
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
 * the wire shape of `ww_req_init_t` (or `ww_bridge_init_t`) changes;
 * `ww_bridge_recv_init` validates the value sent by the daemon
 * matches and returns -EPROTO otherwise. */
#define WW_BRIDGE_SUPPORTED_SPAWN_VERSION 3u

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
 * `ww_req_init_t` decode); call `ww_bridge_init_free` exactly once
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
} ww_bridge_init_t;

/* Receive the daemon's typed `init` request and copy it into `out`.
 *
 * Behaviour:
 *   - Blocks until the next control frame arrives.
 *   - If the message is anything other than `WW_REQ_INIT`, the body
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
 * ApplySettings — runtime hot-reload of plugin settings (SPAWN_VERSION 3)
 *
 * The daemon fires `apply_settings` over a live renderer's IPC socket
 * whenever a non-identity plugin setting changes — e.g. `loop_file` /
 * `hwdec` for mpv, `volume` for wescene, `fps` for any plugin whose
 * manifest declares it. The whole payload is a kv list (same shape
 * as `init.settings`); no typed scalars are promoted.
 * ----------------------------------------------------------------------- */

/* Caller-friendly view of the apply_settings payload. Backing storage
 * lives in the underlying `ww_bridge_control_t::u.apply_settings`;
 * `_from_control` transfers ownership of the heap kv list into this
 * struct, so the caller MUST call `ww_bridge_apply_settings_free`
 * exactly once (NOT `ww_bridge_control_free`) when done. */
typedef struct ww_bridge_apply_settings {
    ww_kv_list_t settings;
} ww_bridge_apply_settings_t;

/* Peel the apply_settings typed view out of a generic control message.
 * On success, ownership of the heap kv list moves from `ctrl` into
 * `out`; `ctrl->u.apply_settings.settings` is zeroed so a follow-up
 * `ww_bridge_control_free(ctrl)` is a no-op for that arm.
 * Returns 0 on success, -EINVAL if `ctrl->op != WW_REQ_APPLY_SETTINGS`
 * or either pointer is NULL. */
int ww_bridge_apply_settings_from_control(ww_bridge_control_t *ctrl,
                                          ww_bridge_apply_settings_t *out);

/* Release every heap allocation inside `out`. Safe to call on a
 * zero-initialized struct or after a successful free. Always returns
 * with `out` cleared. */
void ww_bridge_apply_settings_free(ww_bridge_apply_settings_t *out);


#ifdef __cplusplus
} /* extern "C" */
#endif

#endif /* WAYWALLEN_BRIDGE_H */
