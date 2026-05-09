/* waywallen-bridge — pool dispatcher.
 *
 * Owns:
 *   - drm_fd + release timeline drm_syncobj (creation, export, destroy)
 *   - bind_generation + per-slot release_point bookkeeping
 *   - ready / release_syncobj / format_caps / bind_buffers /
 *     frame_ready / bind_failed wire emission
 *   - dispatch into backend ops (pool_egl_gbm.c or pool_vulkan.c)
 *
 * Backends own:
 *   - GPU device handle (GBM device / VkDevice borrowed)
 *   - per-slot resource allocation (gbm_bo / VkImage)
 *   - per-slot handle export to the plugin (GL FBO / VkImage)
 *   - modifier probe (against the producer GPU only)
 */
#include <waywallen-bridge/bridge.h>
#include <waywallen-bridge/pool.h>

#include "log_internal.h"
#include "pool_internal.h"
#include "sync_release.h"

#include <errno.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

/* -----------------------------------------------------------------------
 * Helpers
 * ----------------------------------------------------------------------- */

static void close_slot_fds(ww_pool_t *p) {
    for (uint32_t i = 0; i < p->n_slots && i < WW_POOL_MAX_SLOTS; ++i) {
        for (uint32_t pl = 0; pl < WW_POOL_MAX_PLANES; ++pl) {
            if (p->slots[i].fds[pl] >= 0) {
                close(p->slots[i].fds[pl]);
                p->slots[i].fds[pl] = -1;
            }
        }
    }
    p->n_slots = 0;
}

static void init_slot_fds_unset(ww_pool_t *p) {
    for (uint32_t i = 0; i < WW_POOL_MAX_SLOTS; ++i) {
        for (uint32_t pl = 0; pl < WW_POOL_MAX_PLANES; ++pl) {
            p->slots[i].fds[pl] = -1;
        }
    }
}

static void teardown_slots(ww_pool_t *p) {
    if (p->ops && p->ops->free_slot) {
        for (uint32_t i = 0; i < p->n_slots; ++i) {
            p->ops->free_slot(p, i);
        }
    }
    close_slot_fds(p);
    memset(p->slots, 0, sizeof(p->slots));
    init_slot_fds_unset(p);
    p->n_slots = 0;
    /* Slot identities changed — back-pressure points reference the
     * old buffers. Reset so the next render frame doesn't wait on a
     * syncobj point that pertains to a destroyed buffer. */
    for (uint32_t i = 0; i < WW_POOL_MAX_SLOTS; ++i) {
        p->last_release_point[i] = 0;
    }
}

static int send_ready_once(ww_pool_t *p, int sock) {
    if (p->ready_sent) return 0;
    int rc = ww_bridge_send_ready(sock,
                                  p->caps.drm_render_major,
                                  p->caps.drm_render_minor);
    if (rc != 0) return rc;
    p->ready_sent = true;
    return 0;
}

static int send_release_syncobj_once(ww_pool_t *p, int sock) {
    if (p->release_syncobj_sent) return 0;

    int fd = -1;
    int rc = ww_drm_syncobj_export_fd(p->drm_fd, p->release_syncobj_handle, &fd);
    if (rc != 0) {
        ww_bridge_logf(WW_BRIDGE_LOG_ERROR,
                       "ww_pool: HANDLE_TO_FD on release_syncobj failed: %d", rc);
        return rc;
    }
    rc = ww_bridge_send_release_syncobj(sock, fd);
    close(fd);
    if (rc != 0) {
        ww_bridge_logf(WW_BRIDGE_LOG_ERROR,
                       "ww_pool: send release_syncobj failed: %d", rc);
        return rc;
    }
    p->release_syncobj_sent = true;
    return 0;
}

static int emit_bind_buffers(ww_pool_t *p, int sock) {
    if (p->n_slots == 0 || p->n_slots > WW_POOL_MAX_SLOTS) return -EINVAL;
    p->bind_generation += 1;

    /* Every slot agrees on plane_count after alloc — the directive
     * pinned a single (fourcc, modifier) so the backend produces
     * identical layouts. Pick slot 0's plane_count as the wire's
     * `planes_per_buffer`. */
    uint32_t planes_per_buffer = p->slots[0].plane_count;
    if (planes_per_buffer == 0 || planes_per_buffer > WW_POOL_MAX_PLANES) {
        return -EINVAL;
    }
    for (uint32_t i = 1; i < p->n_slots; ++i) {
        if (p->slots[i].plane_count != planes_per_buffer) {
            ww_bridge_logf(WW_BRIDGE_LOG_ERROR,
                           "ww_pool: emit_bind_buffers: slot[%u].plane_count=%u "
                           "differs from slot[0].plane_count=%u — backend bug",
                           i, p->slots[i].plane_count, planes_per_buffer);
            return -EINVAL;
        }
    }

    uint32_t total = p->n_slots * planes_per_buffer;
    uint32_t strides[WW_POOL_MAX_SLOTS * WW_POOL_MAX_PLANES];
    uint32_t offsets[WW_POOL_MAX_SLOTS * WW_POOL_MAX_PLANES];
    uint64_t sizes[WW_POOL_MAX_SLOTS * WW_POOL_MAX_PLANES];
    int      fds[WW_POOL_MAX_SLOTS * WW_POOL_MAX_PLANES];
    for (uint32_t s = 0; s < p->n_slots; ++s) {
        for (uint32_t pl = 0; pl < planes_per_buffer; ++pl) {
            uint32_t flat = s * planes_per_buffer + pl;
            strides[flat] = p->slots[s].strides[pl];
            offsets[flat] = p->slots[s].plane_offsets[pl];
            sizes[flat]   = p->slots[s].sizes[pl];
            fds[flat]     = p->slots[s].fds[pl];
        }
    }

    /* Mirror the BUF_HOST_VISIBLE bit when memory source is LINEAR
     * or DMABUF_HEAP — both are GTT/sysmem-backed and PRIME-importable
     * by foreign GPUs. GPU_NATIVE may or may not be device-local;
     * leave the bit clear there (consumer treats absent as "device
     * local; same-GPU only"). */
    uint32_t bb_flags = 0;
    if (p->cur.mem_source == WW_MEM_SRC_GPU_LINEAR ||
        p->cur.mem_source == WW_MEM_SRC_DMABUF_HEAP) {
        bb_flags |= WW_BUF_HOST_VISIBLE;
    }

    ww_evt_bind_buffers_t bb = {0};
    bb.generation         = p->bind_generation;
    bb.flags              = bb_flags;
    bb.count              = p->n_slots;
    bb.fourcc             = p->cur.fourcc;
    bb.width              = p->cur.width;
    bb.height             = p->cur.height;
    bb.modifier           = p->slots[0].modifier;
    bb.planes_per_buffer  = planes_per_buffer;
    bb.stride.count       = total;
    bb.stride.data        = strides;
    bb.plane_offset.count = total;
    bb.plane_offset.data  = offsets;
    bb.size.count         = total;
    bb.size.data          = sizes;

    ww_bridge_logf(WW_BRIDGE_LOG_DEBUG,
                   "ww_pool: emit_bind_buffers gen=%llu count=%u planes=%u fourcc=0x%08x "
                   "%ux%u mod=0x%016llx flags=0x%x",
                   (unsigned long long)bb.generation, bb.count, planes_per_buffer,
                   bb.fourcc, bb.width, bb.height,
                   (unsigned long long)bb.modifier, bb.flags);
    for (uint32_t s = 0; s < p->n_slots; ++s) {
        for (uint32_t pl = 0; pl < planes_per_buffer; ++pl) {
            uint32_t flat = s * planes_per_buffer + pl;
            ww_bridge_logf(WW_BRIDGE_LOG_DEBUG,
                           "ww_pool:   buf[%u].plane[%u] fd=%d stride=%u offset=%u size=%llu",
                           s, pl, fds[flat], strides[flat], offsets[flat],
                           (unsigned long long)sizes[flat]);
        }
    }

    return ww_bridge_send_bind_buffers(sock, &bb, fds);
}

static int validate_directive(const ww_pool_t *p,
                              const ww_pool_directive_t *d) {
    if (!d) return -EINVAL;
    if (d->count == 0 || d->count > WW_POOL_MAX_SLOTS) return -EINVAL;
    /* `d->width`/`d->height` are no longer caller-controlled — the
     * pool sizes slots from `probe_width/probe_height` (the renderer's
     * actual render extent). Validation here would just second-guess
     * the renderer; reject only if the pool itself never advertised
     * dims (caller forgot to call `advertise_caps`). */
    if (p->probe_width == 0 || p->probe_height == 0) return -EINVAL;

    switch (d->category) {
    case WW_PATH_OPTIMIZED_SAME_DEVICE:
    case WW_PATH_OPTIMIZED_SAME_VENDOR:
    case WW_PATH_COMPAT_LINEAR:
        break;
    case WW_PATH_COMPAT_CPU_READBACK:
        return -ENOTSUP; /* Iter 3+ */
    default:
        return -EINVAL;
    }

    switch (d->mem_source) {
    case WW_MEM_SRC_GPU_NATIVE:
    case WW_MEM_SRC_GPU_LINEAR:
        break;
    case WW_MEM_SRC_DMABUF_HEAP:
        return -ENOTSUP; /* Iter 1 doesn't implement dma-buf-heap */
    default:
        return -EINVAL;
    }

    /* OPTIMIZED paths must reference an advertised (fourcc, modifier).
     * COMPAT_LINEAR doesn't have to be advertised — bridge may
     * re-allocate a brand-new LINEAR buffer regardless. */
    if (d->category == WW_PATH_OPTIMIZED_SAME_DEVICE ||
        d->category == WW_PATH_OPTIMIZED_SAME_VENDOR) {
        bool ok = false;
        for (size_t i = 0; i < p->caps.count; ++i) {
            if (p->caps.entries[i].fourcc == d->fourcc &&
                p->caps.entries[i].modifier == d->modifier) {
                ok = true;
                break;
            }
        }
        if (!ok) return -ENOTSUP;
    }

    /* Iter 1 only supports timeline drm_syncobj; daemon must pick
     * SYNC_SYNCOBJ_TIMELINE. Other modes are reserved for Iter 4. */
    if (d->sync_mode != WW_SYNC_SYNCOBJ_TIMELINE) {
        return -ENOTSUP;
    }

    return 0;
}

static void send_bind_failed_quiet(ww_pool_t *p, int sock,
                                   uint32_t fourcc, uint64_t modifier,
                                   uint32_t reason, const char *msg) {
    int rc = ww_bridge_send_bind_failed(sock, fourcc, modifier, reason, msg);
    if (rc != 0) {
        ww_bridge_logf(WW_BRIDGE_LOG_ERROR,
                       "ww_pool: send bind_failed failed: %d", rc);
    }
    (void)p;
}

/* -----------------------------------------------------------------------
 * Public API
 * ----------------------------------------------------------------------- */

int ww_bridge_pool_create(ww_pool_backend_t backend,
                          const void       *init_data,
                          ww_pool_t       **out_pool) {
    if (!init_data || !out_pool) return -EINVAL;
    *out_pool = NULL;

    ww_pool_t *p = (ww_pool_t *)calloc(1, sizeof(*p));
    if (!p) return -ENOMEM;
    p->backend = backend;
    p->drm_fd  = -1;
    init_slot_fds_unset(p);

    int rc;
    switch (backend) {
    case WW_POOL_BACKEND_EGL_GBM:
        rc = ww_pool_egl_gbm_create(p, init_data);
        break;
    case WW_POOL_BACKEND_VULKAN:
        rc = ww_pool_vulkan_create(p, init_data);
        break;
    default:
        rc = -EINVAL;
        break;
    }
    if (rc != 0) {
        free(p);
        return rc;
    }

    /* Backend init must have populated drm_fd. Create the timeline
     * drm_syncobj here so apply_directive can immediately publish a
     * release_point of 1 on the first frame. */
    if (p->drm_fd < 0) {
        if (p->ops && p->ops->destroy) p->ops->destroy(p);
        free(p);
        return -ENODEV;
    }
    rc = ww_drm_syncobj_create(p->drm_fd, &p->release_syncobj_handle);
    if (rc != 0) {
        if (p->ops && p->ops->destroy) p->ops->destroy(p);
        if (p->drm_fd >= 0) close(p->drm_fd);
        free(p);
        return rc;
    }

    *out_pool = p;
    return 0;
}

void ww_bridge_pool_destroy(ww_pool_t *pool) {
    if (!pool) return;
    teardown_slots(pool);
    if (pool->ops && pool->ops->destroy) {
        pool->ops->destroy(pool);
    }
    if (pool->release_syncobj_handle != 0 && pool->drm_fd >= 0) {
        ww_drm_syncobj_destroy(pool->drm_fd, pool->release_syncobj_handle);
    }
    if (pool->drm_fd >= 0) {
        close(pool->drm_fd);
    }
    if (pool->caps.entries) {
        free(pool->caps.entries);
    }
    free(pool);
}

int ww_bridge_pool_advertise_caps(ww_pool_t *pool,
                                  int        sock,
                                  uint32_t   width,
                                  uint32_t   height,
                                  uint32_t   mem_hints) {
    if (!pool) return -EINVAL;
    pool->probe_width  = width;
    pool->probe_height = height;
    pool->caps.mem_hints = mem_hints;

    int rc = pool->ops->probe_caps(pool, width, height);
    if (rc != 0) return rc;
    pool->caps_advertised = true;

    rc = send_ready_once(pool, sock);
    if (rc != 0) return rc;
    rc = send_release_syncobj_once(pool, sock);
    if (rc != 0) return rc;

    /* Encode caps as a flat ww_evt_format_caps_t and send. */
    if (pool->caps.count == 0) return -ENOTSUP;

    /* Worst-case scratch sizing: one fourcc per entry. */
    size_t n = pool->caps.count;
    uint32_t *scratch_fourccs      = (uint32_t *)calloc(n, sizeof(uint32_t));
    uint32_t *scratch_mod_counts   = (uint32_t *)calloc(n, sizeof(uint32_t));
    uint64_t *scratch_modifiers    = (uint64_t *)calloc(n, sizeof(uint64_t));
    uint32_t *scratch_plane_counts = (uint32_t *)calloc(n, sizeof(uint32_t));
    if (!scratch_fourccs || !scratch_mod_counts || !scratch_modifiers ||
        !scratch_plane_counts) {
        free(scratch_fourccs); free(scratch_mod_counts);
        free(scratch_modifiers); free(scratch_plane_counts);
        return -ENOMEM;
    }

    ww_negotiation_state_t neg = {0};
    neg.advertised       = pool->caps.entries;
    neg.advertised_count = pool->caps.count;
    neg.fourcc           = pool->caps.entries[0].fourcc;
    neg.modifier         = pool->caps.entries[0].modifier;
    neg.plane_count      = pool->caps.entries[0].plane_count;

    ww_format_caps_caller_t out = {0};
    ww_bridge_negotiation_fill_format_caps(
        &neg,
        scratch_fourccs, scratch_mod_counts,
        scratch_modifiers, scratch_plane_counts,
        &out);

    out.device_uuid       = pool->caps.have_uuid ? pool->caps.device_uuid : NULL;
    out.driver_uuid       = pool->caps.have_uuid ? pool->caps.driver_uuid : NULL;
    out.drm_render_major  = pool->caps.drm_render_major;
    out.drm_render_minor  = pool->caps.drm_render_minor;
    out.mem_hints         = pool->caps.mem_hints;
    out.sync_caps         = pool->caps.sync_caps;
    out.color_caps        = pool->caps.color_caps;
    out.extent_max_w      = pool->caps.extent_max_w;
    out.extent_max_h      = pool->caps.extent_max_h;

    rc = ww_bridge_send_format_caps_v2(sock, &out);
    free(scratch_fourccs); free(scratch_mod_counts);
    free(scratch_modifiers); free(scratch_plane_counts);
    return rc;
}

int ww_bridge_pool_apply_directive(ww_pool_t                 *pool,
                                   int                        sock,
                                   const ww_pool_directive_t *directive) {
    if (!pool || !directive) return -EINVAL;
    if (!pool->caps_advertised) return -EINVAL;

    int rc = validate_directive(pool, directive);
    if (rc != 0) {
        send_bind_failed_quiet(pool, sock, directive->fourcc, directive->modifier,
                               2 /* feature_unsupported */,
                               "directive rejected by pool");
        return rc;
    }

    /* Tear down existing slots before re-allocating. */
    teardown_slots(pool);
    pool->cur          = *directive;
    /* The renderer is the authority on render-target extent — it
     * already resolved the daemon's policy hint against its content's
     * intrinsic size when it called `advertise_caps`, and its render
     * loop is producing frames at exactly `probe_width × probe_height`.
     * The daemon's `negotiate_buffers` only carries (fourcc, modifier,
     * sync, color, mem_hint) decisions; its `extent_w/h` field is
     * just an echo of what it sent in `Init` and isn't authoritative
     * here. Override with the renderer's choice so dmabuf slots
     * match the frames being put into them. */
    if (pool->probe_width  > 0) pool->cur.width  = pool->probe_width;
    if (pool->probe_height > 0) pool->cur.height = pool->probe_height;
    pool->has_directive = true;

    /* Dry-run: try slot 0 first. */
    rc = pool->ops->alloc_slot(pool, 0, &pool->slots[0]);
    if (rc != 0) {
        ww_bridge_logf(WW_BRIDGE_LOG_WARN,
                       "ww_pool: dry-run alloc_slot[0] failed (path=%u mem_src=%u "
                       "modifier=0x%016llx): %d",
                       directive->category, directive->mem_source,
                       (unsigned long long)directive->modifier, rc);
        send_bind_failed_quiet(pool, sock, directive->fourcc, directive->modifier,
                               0 /* import_failed */,
                               "alloc_slot dry-run failed");
        pool->n_slots = 0;
        return rc;
    }
    pool->n_slots = 1;

    /* Allocate the rest. Failure of any later slot rolls back to a
     * full bind_failed (the daemon can't safely use a partial pool). */
    for (uint32_t i = 1; i < directive->count; ++i) {
        rc = pool->ops->alloc_slot(pool, i, &pool->slots[i]);
        if (rc != 0) {
            ww_bridge_logf(WW_BRIDGE_LOG_ERROR,
                           "ww_pool: alloc_slot[%u] failed: %d", i, rc);
            send_bind_failed_quiet(pool, sock,
                                   directive->fourcc, directive->modifier,
                                   1 /* oom */,
                                   "alloc_slot failed mid-pool");
            teardown_slots(pool);
            return rc;
        }
        pool->n_slots = i + 1;
    }

    /* All slots allocated — emit bind_buffers. */
    rc = emit_bind_buffers(pool, sock);
    if (rc != 0) {
        ww_bridge_logf(WW_BRIDGE_LOG_ERROR,
                       "ww_pool: emit bind_buffers failed: %d", rc);
        return rc;
    }
    return 0;
}

int ww_bridge_pool_acquire_slot(ww_pool_t      *pool,
                                uint32_t        slot_index,
                                ww_pool_slot_t *out_slot) {
    if (!pool || !out_slot) return -EINVAL;
    if (!pool->has_directive) return -EINVAL;
    if (slot_index >= pool->n_slots) return -EINVAL;

    memset(out_slot, 0, sizeof(*out_slot));
    out_slot->index        = slot_index;
    out_slot->width        = pool->cur.width;
    out_slot->height       = pool->cur.height;
    /* Plugin-facing convenience: expose plane 0 only. Plugins that
     * need multi-plane layout (rare — render targets are normally
     * GPU-internal) read the bridge's caps directly. */
    out_slot->stride       = pool->slots[slot_index].strides[0];
    out_slot->plane_offset = pool->slots[slot_index].plane_offsets[0];
    out_slot->size         = (uint32_t)pool->slots[slot_index].sizes[0];
    /* Backend fills its handle fields. */
    return pool->ops->populate_slot_view(pool, slot_index, out_slot);
}

int ww_bridge_pool_submit_slot(ww_pool_t *pool,
                               int        sock,
                               uint32_t   slot_index,
                               int        acquire_sync_fd) {
    if (!pool) {
        if (acquire_sync_fd >= 0) close(acquire_sync_fd);
        return -EINVAL;
    }
    if (!pool->has_directive || slot_index >= pool->n_slots) {
        if (acquire_sync_fd >= 0) close(acquire_sync_fd);
        return -EINVAL;
    }

    pool->release_point += 1;
    uint64_t pt = pool->release_point;
    pool->last_release_point[slot_index] = pt;

    ww_evt_frame_ready_t fr = {0};
    fr.image_index   = slot_index;
    fr.seq           = pt;          /* seq doubles as monotonic per submit */
    fr.ts_ns         = ww_bridge_now_ns();
    fr.release_point = pt;

    int rc = ww_bridge_send_frame_ready(sock, &fr, acquire_sync_fd);
    if (acquire_sync_fd >= 0) close(acquire_sync_fd);
    return rc;
}

int ww_bridge_pool_wait_slot_release(ww_pool_t *pool,
                                     uint32_t   slot_index,
                                     uint32_t   timeout_ms) {
    if (!pool) return -EINVAL;
    if (!pool->has_directive || slot_index >= pool->n_slots) return -EINVAL;
    return ww_drm_syncobj_timeline_wait(pool->drm_fd,
                                        pool->release_syncobj_handle,
                                        pool->last_release_point[slot_index],
                                        timeout_ms);
}
