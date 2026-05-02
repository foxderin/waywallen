#pragma once

// Streaming video decoder for the waywallen video plugin.
//
// Produces NV12 frames (Y plane followed by interleaved UV plane,
// tightly packed) sized to a fixed target extent — that's what the
// `YuvToRgba` GPU pass consumes. NV12 is also what every common hw
// video decoder natively produces, which makes Iter 4's vulkan-decode
// path a drop-in: the AVVkFrame's images already match this layout, so
// `next_frame()` just changes its data-source.
//
// The decoder is sw-only as of Iter 2 — we ALWAYS swscale to NV12 at
// the target extent to keep callers oblivious to the source codec /
// pixel format. Iter 4 plugs in `hw_device_ctx = AVHWDeviceContext`
// (vulkan) and skips the swscale step when the frame is already
// vulkan-typed at our target size.

#include <cstdint>
#include <memory>
#include <string>
#include <vector>

#include <vulkan/vulkan.h>

namespace waywallen::ffvk {

class Producer;

struct Nv12Frame {
    // Layout: Y plane (`width * height` bytes) directly followed by
    // interleaved UV plane (`width * height / 2` bytes). Total size is
    // therefore `width * height * 3 / 2`.
    std::vector<uint8_t> data;
    uint32_t             width  { 0 };
    uint32_t             height { 0 };
    // Stream-time PTS in seconds; -1.0 if unavailable.
    double               pts_seconds { -1.0 };
    // Source colorspace / range — caller feeds these into the YuvToRgba
    // colour matrix builder. Defaults to BT.709 limited range when the
    // stream doesn't tag them.
    uint32_t             colorspace { 0 };  // matches our ColorSpace enum
    uint32_t             color_range { 0 }; // matches our ColorRange enum
};

struct DecodeError {
    std::string message;
};

enum class FrameStatus {
    ok,
    eof,    // clean end of stream; only seen with loop=false
    error,
};

// View onto the AVVkFrame yielded by `next_vk_frame` — one entry per
// plane (DISABLE_MULTIPLANE forces 2-image NV12, but we expose the same
// shape for either case so the caller can switch on `plane_count`).
//
// The pointers point INTO the underlying AVVkFrame; they're valid until
// the next call to `next_vk_frame` (which unrefs the holding AVFrame and
// returns the AVVkFrame to the pool). The caller is expected to update
// layout / sem_value / queue_family in place after recording barriers
// against these images, so the decoder's subsequent decode submit picks
// up the right state.
struct VkFrameView {
    VkImage*       img;           // length: plane_count
    VkImageLayout* layout;        // length: plane_count
    VkSemaphore*   sem;           // length: plane_count
    uint64_t*      sem_value;     // length: plane_count
    uint32_t*      queue_family;  // length: plane_count
    uint32_t       plane_count;
    uint32_t       width;
    uint32_t       height;
    double         pts_seconds;
    uint32_t       colorspace  { 0 };
    uint32_t       color_range { 0 };
    // 8 (NV12) or 16 (P010 / P016). Drives the VkFormat YuvToRgba uses
    // for the per-call Y/UV imageviews. Stream is tagged 10-bit when
    // the codec set bits_per_raw_sample >= 10 in get_format.
    uint32_t       bit_depth   { 8 };
};

class VideoDecoder {
public:
    // Read the native video resolution from the file's first video
    // stream without committing to a decoder. Used by callers that
    // need the intrinsic dimensions before allocating the GPU
    // producer (which itself needs sizing). Sets `*native_w` and
    // `*native_h` and returns true on success.
    static bool probe_native(const std::string& path,
                             uint32_t* native_w, uint32_t* native_h,
                             DecodeError* err);

    // `target_w`/`target_h` are the wallpaper extent. Both are rounded
    // up to even pixel boundaries (NV12 chroma is 4:2:0). Setting
    // `loop=true` causes EOF to seek back to the start automatically.
    static std::unique_ptr<VideoDecoder>
    open(const std::string& path,
         uint32_t            target_w,
         uint32_t            target_h,
         bool                loop,
         DecodeError*        err);

    // Shared-device variant: bring up an AV_HWDEVICE_TYPE_VULKAN
    // hwcontext on top of the Producer's VkInstance/VkDevice. When
    // successful, decoded frames stay GPU-resident and the caller uses
    // `next_vk_frame()` to get them. If anything in the shared-device
    // setup fails (older FFmpeg, missing extensions, codec doesn't
    // accept vulkan decode), the decoder falls back transparently to
    // the sw decode path and `using_vk_frames()` returns false.
    static std::unique_ptr<VideoDecoder>
    open_with_vk(const std::string&  path,
                 uint32_t             target_w,
                 uint32_t             target_h,
                 bool                 loop,
                 const Producer&      vk,
                 DecodeError*         err);

    ~VideoDecoder();
    VideoDecoder(const VideoDecoder&)            = delete;
    VideoDecoder& operator=(const VideoDecoder&) = delete;

    // Pull packets until exactly one frame is decoded, scaled to
    // (`target_w x target_h`) NV12, and emitted in `out`. `out.data` is
    // resized once to the NV12 size on first call and reused after.
    FrameStatus next_frame(Nv12Frame& out, DecodeError* err);

    // True iff the decoder is in shared-device vulkan mode and is
    // producing AVVkFrames the caller can sample directly. Stable for
    // the decoder's lifetime.
    bool using_vk_frames() const { return using_vk_frames_; }

    // Shared-device variant of next_frame. Only valid when
    // `using_vk_frames()` is true. The returned `VkFrameView` aliases
    // the AVVkFrame held by the decoder; do not call `next_*_frame`
    // again until you've recorded all the GPU work that uses it.
    FrameStatus next_vk_frame(VkFrameView& out, DecodeError* err);

    uint32_t width() const  { return target_w_; }
    uint32_t height() const { return target_h_; }
    void     set_loop(bool loop) { loop_ = loop; }

    // Forward declaration; impl details (libav* handles) live in the
    // .cpp so this header doesn't drag FFmpeg includes into plugins.
    struct State;

private:
    VideoDecoder() = default;

    /* `pre_built_hwdev` is type-erased as void* so we don't drag the
     * FFmpeg headers into this header; the impl casts it back to
     * AVBufferRef*. NULL means "use FFmpeg-managed hwdevice" (Iter-4
     * path); non-NULL means "attach this caller-built device" and the
     * helper takes ownership on success/failure. */
    static std::unique_ptr<VideoDecoder>
    build_internal(const std::string& path,
                   uint32_t target_w, uint32_t target_h,
                   bool loop, void* pre_built_hwdev,
                   DecodeError* err);

    std::unique_ptr<State> st_;
    uint32_t target_w_ { 0 };
    uint32_t target_h_ { 0 };
    bool     loop_     { false };
    bool     using_vk_frames_ { false };
};

} // namespace waywallen::ffvk
