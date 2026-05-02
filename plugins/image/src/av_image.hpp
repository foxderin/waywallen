#pragma once

#include <cstdint>
#include <string>
#include <vector>

namespace ww_image {

// Tightly-packed RGBA8 (R,G,B,A in memory order).
struct RgbaBuf {
    std::vector<uint8_t> data;
    uint32_t             width { 0 };
    uint32_t             height { 0 };
    uint32_t             stride { 0 }; // bytes per row; == width * 4 (no padding)
};

struct DecodeError {
    std::string message;
};

// Decode `path` (any container/codec FFmpeg understands), resolve the
// final render extent against the daemon's hint
// `(extent_w, extent_h, extent_mode)` using
// `<waywallen-bridge/extent_resolve.h>`, and scale the first frame to
// that extent in RGBA8. Scaling uses SWS_BICUBIC. The returned
// `width`/`height` reflect the **resolved** size — callers should
// take their render-target dims from the buffer, not from
// `extent_w`/`extent_h`. Populates `err->message` and returns an
// empty buffer on failure.
RgbaBuf decode_to_rgba(const std::string& path,
                       uint32_t           extent_w,
                       uint32_t           extent_h,
                       uint32_t           extent_mode,
                       DecodeError*       err);

} // namespace ww_image
