/* Internal logging shim. Routes through the user-installed
 * ww_bridge_set_log_callback() with stderr fallback. */

#ifndef WW_BRIDGE_LOG_INTERNAL_H
#define WW_BRIDGE_LOG_INTERNAL_H

#include <waywallen-bridge/bridge.h>

#ifdef __cplusplus
extern "C" {
#endif

__attribute__((format(printf, 2, 3), visibility("hidden")))
void ww_bridge_logf(ww_bridge_log_level_t level, const char *fmt, ...);

#ifdef __cplusplus
}
#endif

#endif /* WW_BRIDGE_LOG_INTERNAL_H */
