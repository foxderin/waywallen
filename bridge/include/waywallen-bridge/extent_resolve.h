#ifndef WAYWALLEN_BRIDGE_EXTENT_RESOLVE_H
#define WAYWALLEN_BRIDGE_EXTENT_RESOLVE_H

/* Shared inline helper that turns the daemon's (extent_w, extent_h,
 * extent_mode) hint into the actual render extent each renderer
 * should allocate. Every renderer must call this with its content's
 * intrinsic (native) size — image/video: decoded frame dims; mpv:
 * `dwidth`/`dheight` after FILE_LOADED; wescene: scene-declared
 * resolution from the .pkg manifest.
 *
 * Semantics, matching `ww_extent_mode_t` in <bridge.h>:
 *   AS_GIVEN:
 *     - both extent_w and extent_h > 0  → use exactly as-given.
 *     - extent_w > 0, extent_h == 0     → height = extent_w * native_h / native_w.
 *     - extent_w == 0, extent_h > 0     → width  = extent_h * native_w / native_h.
 *     - both 0                          → use native size unchanged.
 *   FIT_SHORTER:
 *     - target = max(extent_w, extent_h).
 *     - if native_w <= native_h         → out_w = target, out_h scaled.
 *     - else                            → out_h = target, out_w scaled.
 *
 * Native dims of 0 are treated defensively: the helper falls back to
 * `(extent_w, extent_h)` if either native is 0, and finally to
 * `(1, 1)` if everything is 0 — callers should still validate before
 * allocating GPU resources.
 */

#include <stdint.h>
#include "waywallen-bridge/bridge.h"

#ifdef __cplusplus
extern "C" {
#endif

static inline void ww_resolve_extent(
    uint32_t extent_w, uint32_t extent_h, uint32_t extent_mode,
    uint32_t native_w, uint32_t native_h,
    uint32_t *out_w, uint32_t *out_h)
{
    if (native_w == 0 || native_h == 0) {
        *out_w = extent_w ? extent_w : 1u;
        *out_h = extent_h ? extent_h : 1u;
        return;
    }

    if (extent_mode == WW_EXTENT_MODE_FIT_SHORTER) {
        uint32_t target = extent_w > extent_h ? extent_w : extent_h;
        if (target == 0) {
            *out_w = native_w;
            *out_h = native_h;
            return;
        }
        if (native_w <= native_h) {
            *out_w = target;
            *out_h = (uint32_t)((uint64_t)target * native_h / native_w);
        } else {
            *out_h = target;
            *out_w = (uint32_t)((uint64_t)target * native_w / native_h);
        }
        if (*out_w == 0) *out_w = 1u;
        if (*out_h == 0) *out_h = 1u;
        return;
    }

    /* AS_GIVEN */
    if (extent_w > 0 && extent_h > 0) {
        *out_w = extent_w;
        *out_h = extent_h;
        return;
    }
    if (extent_w > 0) {
        *out_w = extent_w;
        *out_h = (uint32_t)((uint64_t)extent_w * native_h / native_w);
        if (*out_h == 0) *out_h = 1u;
        return;
    }
    if (extent_h > 0) {
        *out_h = extent_h;
        *out_w = (uint32_t)((uint64_t)extent_h * native_w / native_h);
        if (*out_w == 0) *out_w = 1u;
        return;
    }
    *out_w = native_w;
    *out_h = native_h;
}

#ifdef __cplusplus
} /* extern "C" */
#endif

#endif /* WAYWALLEN_BRIDGE_EXTENT_RESOLVE_H */
