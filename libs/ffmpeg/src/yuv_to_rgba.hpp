#pragma once

// GPU NV12 → RGBA8 converter built on a Vulkan compute pipeline.
//
// Lifecycle: one instance per renderer subprocess, sized for the largest
// extent the daemon may negotiate (Iter 2: matches the wallpaper extent
// passed in `Init`). Per-frame the producer:
//
//   1. obtains an NV12 byte buffer from libavcodec (sw decode path —
//      the decoder helper does swscale-to-NV12 if the codec didn't
//      already produce that format),
//   2. calls `convert_nv12(slot.vk_image, w, h, bytes, size)`,
//   3. hands the returned sync_fd to `ww_bridge_pool_submit_slot`.
//
// Internally the converter maintains:
//   - private NV12 sampling images (Y: R8_UNORM, UV: R8G8_UNORM at half
//     resolution) sized to the max extent, re-uploaded each frame.
//   - a staging buffer big enough for `max_w * max_h * 3 / 2` bytes.
//   - a single VkDescriptorSet — bindings 0/1 are stable (the Y/UV
//     image views), binding 2 (the storage-image dst) gets re-pointed
//     at every frame at the bridge slot's VkImage.
//   - a VkFence so repeated convert_nv12 calls are race-free against
//     cmd-buffer reuse.

#include <cstdint>
#include <memory>
#include <string>

#include <vulkan/vulkan.h>

namespace waywallen::ffvk {

// Coefficients for the YUV→RGB push constant. CPU side fills this from
// the source frame's colorspace + range; the shader applies it as
// `rgb = M * (ycbcr + offset)`.
struct ColorMatrix {
    float m_r[3];   // Y, Cb, Cr scalings producing R
    float m_g[3];
    float m_b[3];
    float offset[3]; // subtracted from (Y, Cb, Cr) before matmul
};

// Mirrors FFmpeg's `enum AVColorSpace` for the cases we actually
// branch on. Keeping our own enum avoids leaking <libavutil/pixfmt.h>
// into headers consumed by plugins.
enum class ColorSpace : uint32_t {
    Bt709 = 0,    // default
    Bt601 = 1,    // SMPTE 170M / BT.470BG
    Bt2020 = 2,   // BT.2020 non-constant luma (treated as BT.709 for HDR-out-of-scope reasons)
};

enum class ColorRange : uint32_t {
    Limited = 0,  // 16..235 / 16..240 (MPEG)
    Full    = 1,  // 0..255 (JPEG)
};

// Derive the ColorMatrix to push to the shader. Defaults to BT.709
// limited when either argument is the canonical "unknown" sentinel.
ColorMatrix make_color_matrix(ColorSpace cs, ColorRange cr);

class YuvToRgba {
public:
    ~YuvToRgba();
    YuvToRgba(const YuvToRgba&)            = delete;
    YuvToRgba& operator=(const YuvToRgba&) = delete;

    // Build the pipeline + private NV12 images sized for `max_w x max_h`.
    // The Y/UV plane images are reused across frames; only the dst storage
    // image binding changes per dispatch. `max_w` / `max_h` must both be
    // non-zero; if odd they are rounded up to the next even pixel because
    // NV12 chroma is half-resolution.
    static std::unique_ptr<YuvToRgba>
    create(VkInstance       instance,
           VkPhysicalDevice phys,
           VkDevice         device,
           uint32_t         queue_family,
           VkQueue          queue,
           uint32_t         max_w,
           uint32_t         max_h,
           std::string*     err);

    // Convert `nv12` (Y plane of W*H bytes followed by interleaved UV
    // plane of W*H/2 bytes) into `dst` (RGBA8 storage image). Returns an
    // exported sync_fd that signals when the dispatch is complete; bridge
    // takes ownership of the fd. Returns -1 with `*err` populated on
    // failure. `dst_w`/`dst_h` must be ≤ the (max_w, max_h) passed to
    // create() and must be even.
    int convert_nv12(VkImage             dst,
                     uint32_t            dst_w,
                     uint32_t            dst_h,
                     const uint8_t*      nv12,
                     size_t              nv12_size,
                     const ColorMatrix&  cm,
                     std::string*        err);

    // Zero-copy variant: import the Y/UV VkImages directly from an
    // AVVkFrame (DISABLE_MULTIPLANE-allocated). Waits on the supplied
    // timeline semaphores at their current values, signals incremented
    // values back. The caller MUST update the AVVkFrame's
    // layout/sem_value/queue_family arrays in place after this call
    // returns success; we expose the new values via the in/out args.
    //
    // `y_image`, `uv_image`: VkImages from the frame (R8 / R8G8).
    // `y_sem`, `uv_sem`: timeline VkSemaphores per plane.
    // `y_sem_val_in_out`, `uv_sem_val_in_out`: on input the sem_value
    //   to wait on; on success rewritten to the value we signaled.
    // `y_layout_in_out`, `uv_layout_in_out`: same idea for layout.
    // `y_qf_in_out`, `uv_qf_in_out`: same for queue family.
    // `src_w`, `src_h`: source frame extent (the frame's pixel size).
    // `dst`, `dst_w`, `dst_h`: target storage image + extent.
    // Returns sync_fd (binary, signaled when dispatch completes) or -1.
    struct VkFrameImports {
        VkImage         y_image;
        VkImage         uv_image;
        VkSemaphore     y_sem;
        VkSemaphore     uv_sem;
        uint64_t*       y_sem_val_in_out;
        uint64_t*       uv_sem_val_in_out;
        VkImageLayout*  y_layout_in_out;
        VkImageLayout*  uv_layout_in_out;
        uint32_t*       y_qf_in_out;
        uint32_t*       uv_qf_in_out;
        uint32_t        src_w;
        uint32_t        src_h;
    };
    int convert_av_vk_frame(const VkFrameImports& imports,
                            VkImage             dst,
                            uint32_t            dst_w,
                            uint32_t            dst_h,
                            const ColorMatrix&  cm,
                            std::string*        err);

private:
    YuvToRgba() = default;

    bool init(VkInstance instance, VkPhysicalDevice phys, VkDevice device,
              uint32_t queue_family, VkQueue queue,
              uint32_t max_w, uint32_t max_h, std::string* err);

    // Owned handles. The instance/phys/device handles are caller-owned —
    // we just borrow.
    VkInstance       instance_      { VK_NULL_HANDLE };
    VkPhysicalDevice phys_          { VK_NULL_HANDLE };
    VkDevice         device_        { VK_NULL_HANDLE };
    VkQueue          queue_         { VK_NULL_HANDLE };
    uint32_t         queue_family_  { 0 };

    uint32_t         max_w_         { 0 };
    uint32_t         max_h_         { 0 };

    // Compute pipeline.
    VkShaderModule        shader_      { VK_NULL_HANDLE };
    VkDescriptorSetLayout dsl_         { VK_NULL_HANDLE };
    VkPipelineLayout      pipeline_layout_ { VK_NULL_HANDLE };
    VkPipeline            pipeline_    { VK_NULL_HANDLE };

    // Sampler (linear filter, clamp-to-edge) shared by Y and UV.
    VkSampler        sampler_       { VK_NULL_HANDLE };

    // Y plane image (R8_UNORM, max_w × max_h, OPTIMAL tiling).
    VkImage          y_image_       { VK_NULL_HANDLE };
    VkDeviceMemory   y_memory_      { VK_NULL_HANDLE };
    VkImageView      y_view_        { VK_NULL_HANDLE };

    // UV plane image (R8G8_UNORM, max_w/2 × max_h/2).
    VkImage          uv_image_      { VK_NULL_HANDLE };
    VkDeviceMemory   uv_memory_     { VK_NULL_HANDLE };
    VkImageView      uv_view_       { VK_NULL_HANDLE };

    // Staging buffer big enough for max_w * max_h * 3/2 bytes (NV12).
    VkBuffer         staging_buf_   { VK_NULL_HANDLE };
    VkDeviceMemory   staging_mem_   { VK_NULL_HANDLE };
    void*            staging_map_   { nullptr };
    VkDeviceSize     staging_size_  { 0 };

    // Cmd recording.
    VkCommandPool    cmd_pool_      { VK_NULL_HANDLE };
    VkCommandBuffer  cmd_           { VK_NULL_HANDLE };

    // Per-submit signal semaphore (exported as SYNC_FD) + completion fence.
    VkSemaphore      signal_sem_    { VK_NULL_HANDLE };
    VkFence          done_fence_    { VK_NULL_HANDLE };
    bool             fence_pending_ { false };

    // Single descriptor set; binding 2 gets rebound per frame to the new
    // dst storage image's view.
    VkDescriptorPool dpool_         { VK_NULL_HANDLE };
    VkDescriptorSet  dset_          { VK_NULL_HANDLE };

    // Single-slot deferred destruction queue for the prior frame's dst
    // VkImageView. Safe because convert_* waits on done_fence_ before
    // doing anything else, which proves the GPU is no longer using the
    // previous descriptor set's binding-2 reference. The shared-device
    // import path additionally cycles last_y_view_ / last_uv_view_
    // because Y/UV bindings now alias the AVVkFrame's images instead of
    // our private staging-uploaded ones.
    VkImageView      last_dst_view_ { VK_NULL_HANDLE };
    VkImageView      last_y_view_   { VK_NULL_HANDLE };
    VkImageView      last_uv_view_  { VK_NULL_HANDLE };

    PFN_vkGetSemaphoreFdKHR vkGetSemaphoreFdKHR_ { nullptr };
};

} // namespace waywallen::ffvk
