#include "yuv_to_rgba.hpp"

#include "nv12_to_rgba.spv.h"  // generated at build time by glslangValidator

#include <cstdio>
#include <cstring>
#include <vector>

namespace waywallen::ffvk {

/* Push-constant struct mirroring `PC` in shaders/nv12_to_rgba.comp.
 * std140-friendly: padded to 16-byte boundaries. */
struct alignas(16) ShaderPushConstants {
    uint32_t dst_w;
    uint32_t dst_h;
    uint32_t _pad0[2];
    float    m_r[4];
    float    m_g[4];
    float    m_b[4];
    float    offset[4];
};
static_assert(sizeof(ShaderPushConstants) == 80, "PC size mismatch with shader");

ColorMatrix make_color_matrix(ColorSpace cs, ColorRange cr) {
    /* Coefficients are the standard ITU-R rec luma/chroma weights, with
     * the limited-range Y/C scaling baked into the matrix so the shader
     * only needs to subtract the offset and matmul. Reference:
     *   BT.709: Kr=0.2126, Kb=0.0722
     *   BT.601: Kr=0.299,  Kb=0.114
     *   BT.2020 (NCL): Kr=0.2627, Kb=0.0593 (treated identically here —
     *     no PQ / HLG support yet, so non-constant-luma BT.2020 is
     *     close enough for SDR fallback).
     */
    ColorMatrix m {};
    if (cr == ColorRange::Full) {
        /* Full range: y_scale = 1.0, c_scale = 1.0; offsets = (0, -.5, -.5). */
        if (cs == ColorSpace::Bt601) {
            m.m_r[0] = 1.0f; m.m_r[1] = 0.0f;     m.m_r[2] = 1.402f;
            m.m_g[0] = 1.0f; m.m_g[1] = -0.34414f; m.m_g[2] = -0.71414f;
            m.m_b[0] = 1.0f; m.m_b[1] = 1.772f;   m.m_b[2] = 0.0f;
        } else if (cs == ColorSpace::Bt2020) {
            m.m_r[0] = 1.0f; m.m_r[1] = 0.0f;     m.m_r[2] = 1.4746f;
            m.m_g[0] = 1.0f; m.m_g[1] = -0.16455f; m.m_g[2] = -0.57135f;
            m.m_b[0] = 1.0f; m.m_b[1] = 1.8814f;  m.m_b[2] = 0.0f;
        } else {
            /* BT.709 default. */
            m.m_r[0] = 1.0f; m.m_r[1] = 0.0f;     m.m_r[2] = 1.5748f;
            m.m_g[0] = 1.0f; m.m_g[1] = -0.18732f; m.m_g[2] = -0.46812f;
            m.m_b[0] = 1.0f; m.m_b[1] = 1.85563f; m.m_b[2] = 0.0f;
        }
        m.offset[0] = 0.0f;
        m.offset[1] = -128.0f / 255.0f;
        m.offset[2] = -128.0f / 255.0f;
    } else {
        /* Limited range. y_scale = 255/219; c_scale = 255/224.
         * Pre-bake into matrix coefficients. */
        constexpr float ys = 255.0f / 219.0f;
        constexpr float cs_ = 255.0f / 224.0f;
        if (cs == ColorSpace::Bt601) {
            m.m_r[0] = ys; m.m_r[1] = 0.0f;        m.m_r[2] = 1.402f   * cs_;
            m.m_g[0] = ys; m.m_g[1] = -0.34414f*cs_; m.m_g[2] = -0.71414f*cs_;
            m.m_b[0] = ys; m.m_b[1] = 1.772f * cs_;  m.m_b[2] = 0.0f;
        } else if (cs == ColorSpace::Bt2020) {
            m.m_r[0] = ys; m.m_r[1] = 0.0f;          m.m_r[2] = 1.4746f * cs_;
            m.m_g[0] = ys; m.m_g[1] = -0.16455f*cs_; m.m_g[2] = -0.57135f*cs_;
            m.m_b[0] = ys; m.m_b[1] = 1.8814f * cs_; m.m_b[2] = 0.0f;
        } else {
            m.m_r[0] = ys; m.m_r[1] = 0.0f;          m.m_r[2] = 1.5748f * cs_;
            m.m_g[0] = ys; m.m_g[1] = -0.18732f*cs_; m.m_g[2] = -0.46812f*cs_;
            m.m_b[0] = ys; m.m_b[1] = 1.85563f*cs_;  m.m_b[2] = 0.0f;
        }
        m.offset[0] = -16.0f  / 255.0f;
        m.offset[1] = -128.0f / 255.0f;
        m.offset[2] = -128.0f / 255.0f;
    }
    return m;
}

namespace {

bool fail(std::string* err, std::string m) {
    if (err) *err = std::move(m);
    return false;
}

const char* vk_result_str(VkResult r) {
    switch (r) {
    case VK_SUCCESS:                        return "VK_SUCCESS";
    case VK_ERROR_OUT_OF_HOST_MEMORY:       return "VK_ERROR_OUT_OF_HOST_MEMORY";
    case VK_ERROR_OUT_OF_DEVICE_MEMORY:     return "VK_ERROR_OUT_OF_DEVICE_MEMORY";
    case VK_ERROR_INITIALIZATION_FAILED:    return "VK_ERROR_INITIALIZATION_FAILED";
    case VK_ERROR_FORMAT_NOT_SUPPORTED:     return "VK_ERROR_FORMAT_NOT_SUPPORTED";
    case VK_ERROR_FEATURE_NOT_PRESENT:      return "VK_ERROR_FEATURE_NOT_PRESENT";
    default:                                return "VK_ERROR_?";
    }
}

uint32_t pick_memory_type(VkPhysicalDevice phys, uint32_t mask,
                          VkMemoryPropertyFlags want) {
    VkPhysicalDeviceMemoryProperties mp {};
    vkGetPhysicalDeviceMemoryProperties(phys, &mp);
    for (uint32_t i = 0; i < mp.memoryTypeCount; ++i) {
        if ((mask & (1u << i))
            && (mp.memoryTypes[i].propertyFlags & want) == want) {
            return i;
        }
    }
    return UINT32_MAX;
}

bool create_image_2d(VkDevice device, VkPhysicalDevice phys,
                     VkFormat fmt, uint32_t w, uint32_t h,
                     VkImageUsageFlags usage,
                     VkImage* out_img, VkDeviceMemory* out_mem,
                     std::string* err) {
    VkImageCreateInfo ici {};
    ici.sType         = VK_STRUCTURE_TYPE_IMAGE_CREATE_INFO;
    ici.imageType     = VK_IMAGE_TYPE_2D;
    ici.format        = fmt;
    ici.extent        = { w, h, 1 };
    ici.mipLevels     = 1;
    ici.arrayLayers   = 1;
    ici.samples       = VK_SAMPLE_COUNT_1_BIT;
    ici.tiling        = VK_IMAGE_TILING_OPTIMAL;
    ici.usage         = usage;
    ici.sharingMode   = VK_SHARING_MODE_EXCLUSIVE;
    ici.initialLayout = VK_IMAGE_LAYOUT_UNDEFINED;
    if (VkResult r = vkCreateImage(device, &ici, nullptr, out_img); r != VK_SUCCESS) {
        fail(err, std::string("vkCreateImage: ") + vk_result_str(r));
        return false;
    }
    VkMemoryRequirements mr {};
    vkGetImageMemoryRequirements(device, *out_img, &mr);
    uint32_t type = pick_memory_type(phys, mr.memoryTypeBits,
                                     VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT);
    if (type == UINT32_MAX) {
        fail(err, "no DEVICE_LOCAL memory type for plane image");
        return false;
    }
    VkMemoryAllocateInfo mai {};
    mai.sType           = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO;
    mai.allocationSize  = mr.size;
    mai.memoryTypeIndex = type;
    if (VkResult r = vkAllocateMemory(device, &mai, nullptr, out_mem); r != VK_SUCCESS) {
        fail(err, std::string("vkAllocateMemory(plane): ") + vk_result_str(r));
        return false;
    }
    if (VkResult r = vkBindImageMemory(device, *out_img, *out_mem, 0); r != VK_SUCCESS) {
        fail(err, std::string("vkBindImageMemory(plane): ") + vk_result_str(r));
        return false;
    }
    return true;
}

bool create_image_view(VkDevice device, VkImage img, VkFormat fmt,
                       VkImageView* out, std::string* err) {
    VkImageViewCreateInfo vci {};
    vci.sType            = VK_STRUCTURE_TYPE_IMAGE_VIEW_CREATE_INFO;
    vci.image            = img;
    vci.viewType         = VK_IMAGE_VIEW_TYPE_2D;
    vci.format           = fmt;
    vci.components       = { VK_COMPONENT_SWIZZLE_IDENTITY, VK_COMPONENT_SWIZZLE_IDENTITY,
                             VK_COMPONENT_SWIZZLE_IDENTITY, VK_COMPONENT_SWIZZLE_IDENTITY };
    vci.subresourceRange = { VK_IMAGE_ASPECT_COLOR_BIT, 0, 1, 0, 1 };
    if (VkResult r = vkCreateImageView(device, &vci, nullptr, out); r != VK_SUCCESS) {
        fail(err, std::string("vkCreateImageView: ") + vk_result_str(r));
        return false;
    }
    return true;
}

void barrier_image(VkCommandBuffer cmd, VkImage img,
                   VkAccessFlags src_a, VkAccessFlags dst_a,
                   VkImageLayout old_l, VkImageLayout new_l,
                   VkPipelineStageFlags src_s, VkPipelineStageFlags dst_s,
                   uint32_t src_qf = VK_QUEUE_FAMILY_IGNORED,
                   uint32_t dst_qf = VK_QUEUE_FAMILY_IGNORED) {
    VkImageMemoryBarrier b {};
    b.sType               = VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER;
    b.srcAccessMask       = src_a;
    b.dstAccessMask       = dst_a;
    b.oldLayout           = old_l;
    b.newLayout           = new_l;
    b.srcQueueFamilyIndex = src_qf;
    b.dstQueueFamilyIndex = dst_qf;
    b.image               = img;
    b.subresourceRange    = { VK_IMAGE_ASPECT_COLOR_BIT, 0, 1, 0, 1 };
    vkCmdPipelineBarrier(cmd, src_s, dst_s, 0, 0, nullptr, 0, nullptr, 1, &b);
}

} // namespace


YuvToRgba::~YuvToRgba() {
    if (device_ != VK_NULL_HANDLE) {
        vkDeviceWaitIdle(device_);
        if (last_dst_view_)    vkDestroyImageView(device_, last_dst_view_, nullptr);
        if (last_y_view_)      vkDestroyImageView(device_, last_y_view_, nullptr);
        if (last_uv_view_)     vkDestroyImageView(device_, last_uv_view_, nullptr);
        if (dpool_)            vkDestroyDescriptorPool(device_, dpool_, nullptr);
        if (signal_sem_)       vkDestroySemaphore(device_, signal_sem_, nullptr);
        if (done_fence_)       vkDestroyFence(device_, done_fence_, nullptr);
        if (cmd_pool_)         vkDestroyCommandPool(device_, cmd_pool_, nullptr);
        if (staging_map_)      vkUnmapMemory(device_, staging_mem_);
        if (staging_buf_)      vkDestroyBuffer(device_, staging_buf_, nullptr);
        if (staging_mem_)      vkFreeMemory(device_, staging_mem_, nullptr);
        if (y_view_)           vkDestroyImageView(device_, y_view_, nullptr);
        if (y_image_)          vkDestroyImage(device_, y_image_, nullptr);
        if (y_memory_)         vkFreeMemory(device_, y_memory_, nullptr);
        if (uv_view_)          vkDestroyImageView(device_, uv_view_, nullptr);
        if (uv_image_)         vkDestroyImage(device_, uv_image_, nullptr);
        if (uv_memory_)        vkFreeMemory(device_, uv_memory_, nullptr);
        if (sampler_)          vkDestroySampler(device_, sampler_, nullptr);
        if (pipeline_)         vkDestroyPipeline(device_, pipeline_, nullptr);
        if (pipeline_layout_)  vkDestroyPipelineLayout(device_, pipeline_layout_, nullptr);
        if (dsl_)              vkDestroyDescriptorSetLayout(device_, dsl_, nullptr);
        if (shader_)           vkDestroyShaderModule(device_, shader_, nullptr);
    }
}

std::unique_ptr<YuvToRgba>
YuvToRgba::create(VkInstance       instance,
                  VkPhysicalDevice phys,
                  VkDevice         device,
                  uint32_t         queue_family,
                  VkQueue          queue,
                  uint32_t         max_w,
                  uint32_t         max_h,
                  std::string*     err) {
    if (max_w == 0 || max_h == 0) {
        fail(err, "YuvToRgba: max_w/max_h must be non-zero");
        return nullptr;
    }
    // NV12 chroma is 4:2:0, so plane W/H must be even.
    if (max_w & 1u) ++max_w;
    if (max_h & 1u) ++max_h;
    auto self = std::unique_ptr<YuvToRgba>(new YuvToRgba());
    if (!self->init(instance, phys, device, queue_family, queue, max_w, max_h, err))
        return nullptr;
    return self;
}

bool YuvToRgba::init(VkInstance instance, VkPhysicalDevice phys, VkDevice device,
                     uint32_t queue_family, VkQueue queue,
                     uint32_t max_w, uint32_t max_h, std::string* err) {
    instance_     = instance;
    phys_         = phys;
    device_       = device;
    queue_        = queue;
    queue_family_ = queue_family;
    max_w_        = max_w;
    max_h_        = max_h;

    vkGetSemaphoreFdKHR_ =
        reinterpret_cast<PFN_vkGetSemaphoreFdKHR>(
            vkGetDeviceProcAddr(device_, "vkGetSemaphoreFdKHR"));
    if (!vkGetSemaphoreFdKHR_) {
        return fail(err, "vkGetSemaphoreFdKHR missing");
    }

    // ----- Sampler (linear, clamp-to-edge) -----
    {
        VkSamplerCreateInfo sci {};
        sci.sType        = VK_STRUCTURE_TYPE_SAMPLER_CREATE_INFO;
        sci.magFilter    = VK_FILTER_LINEAR;
        sci.minFilter    = VK_FILTER_LINEAR;
        sci.mipmapMode   = VK_SAMPLER_MIPMAP_MODE_NEAREST;
        sci.addressModeU = VK_SAMPLER_ADDRESS_MODE_CLAMP_TO_EDGE;
        sci.addressModeV = VK_SAMPLER_ADDRESS_MODE_CLAMP_TO_EDGE;
        sci.addressModeW = VK_SAMPLER_ADDRESS_MODE_CLAMP_TO_EDGE;
        sci.maxLod       = 0.0f;
        if (VkResult r = vkCreateSampler(device_, &sci, nullptr, &sampler_); r != VK_SUCCESS)
            return fail(err, std::string("vkCreateSampler: ") + vk_result_str(r));
    }

    // ----- Y image (R8_UNORM, max_w × max_h) -----
    if (!create_image_2d(device_, phys_, VK_FORMAT_R8_UNORM, max_w_, max_h_,
                         VK_IMAGE_USAGE_TRANSFER_DST_BIT | VK_IMAGE_USAGE_SAMPLED_BIT,
                         &y_image_, &y_memory_, err)) return false;
    if (!create_image_view(device_, y_image_, VK_FORMAT_R8_UNORM, &y_view_, err))
        return false;

    // ----- UV image (R8G8_UNORM, half resolution) -----
    if (!create_image_2d(device_, phys_, VK_FORMAT_R8G8_UNORM, max_w_ / 2, max_h_ / 2,
                         VK_IMAGE_USAGE_TRANSFER_DST_BIT | VK_IMAGE_USAGE_SAMPLED_BIT,
                         &uv_image_, &uv_memory_, err)) return false;
    if (!create_image_view(device_, uv_image_, VK_FORMAT_R8G8_UNORM, &uv_view_, err))
        return false;

    // ----- Staging buffer (HOST_VISIBLE|COHERENT, NV12-sized) -----
    {
        const VkDeviceSize nv12_size = VkDeviceSize(max_w_) * max_h_ * 3 / 2;
        staging_size_ = nv12_size;
        VkBufferCreateInfo bci {};
        bci.sType       = VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO;
        bci.size        = nv12_size;
        bci.usage       = VK_BUFFER_USAGE_TRANSFER_SRC_BIT;
        bci.sharingMode = VK_SHARING_MODE_EXCLUSIVE;
        if (VkResult r = vkCreateBuffer(device_, &bci, nullptr, &staging_buf_); r != VK_SUCCESS)
            return fail(err, std::string("vkCreateBuffer(stage): ") + vk_result_str(r));
        VkMemoryRequirements mr {};
        vkGetBufferMemoryRequirements(device_, staging_buf_, &mr);
        uint32_t type = pick_memory_type(phys_, mr.memoryTypeBits,
                                         VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT |
                                         VK_MEMORY_PROPERTY_HOST_COHERENT_BIT);
        if (type == UINT32_MAX)
            return fail(err, "no HOST_VISIBLE|COHERENT memory type for staging");
        VkMemoryAllocateInfo mai {};
        mai.sType           = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO;
        mai.allocationSize  = mr.size;
        mai.memoryTypeIndex = type;
        if (VkResult r = vkAllocateMemory(device_, &mai, nullptr, &staging_mem_); r != VK_SUCCESS)
            return fail(err, std::string("vkAllocateMemory(stage): ") + vk_result_str(r));
        if (VkResult r = vkBindBufferMemory(device_, staging_buf_, staging_mem_, 0); r != VK_SUCCESS)
            return fail(err, std::string("vkBindBufferMemory(stage): ") + vk_result_str(r));
        if (VkResult r = vkMapMemory(device_, staging_mem_, 0, VK_WHOLE_SIZE, 0, &staging_map_);
            r != VK_SUCCESS)
            return fail(err, std::string("vkMapMemory(stage): ") + vk_result_str(r));
    }

    // ----- Shader module -----
    {
        VkShaderModuleCreateInfo smi {};
        smi.sType    = VK_STRUCTURE_TYPE_SHADER_MODULE_CREATE_INFO;
        smi.codeSize = sizeof(nv12_to_rgba_spv);
        smi.pCode    = reinterpret_cast<const uint32_t*>(nv12_to_rgba_spv);
        if (VkResult r = vkCreateShaderModule(device_, &smi, nullptr, &shader_); r != VK_SUCCESS)
            return fail(err, std::string("vkCreateShaderModule: ") + vk_result_str(r));
    }

    // ----- Descriptor set layout (binding 0/1 = sampled, binding 2 = storage) -----
    {
        VkDescriptorSetLayoutBinding bs[3] {};
        bs[0].binding         = 0;
        bs[0].descriptorType  = VK_DESCRIPTOR_TYPE_COMBINED_IMAGE_SAMPLER;
        bs[0].descriptorCount = 1;
        bs[0].stageFlags      = VK_SHADER_STAGE_COMPUTE_BIT;
        bs[1].binding         = 1;
        bs[1].descriptorType  = VK_DESCRIPTOR_TYPE_COMBINED_IMAGE_SAMPLER;
        bs[1].descriptorCount = 1;
        bs[1].stageFlags      = VK_SHADER_STAGE_COMPUTE_BIT;
        bs[2].binding         = 2;
        bs[2].descriptorType  = VK_DESCRIPTOR_TYPE_STORAGE_IMAGE;
        bs[2].descriptorCount = 1;
        bs[2].stageFlags      = VK_SHADER_STAGE_COMPUTE_BIT;
        VkDescriptorSetLayoutCreateInfo dsli {};
        dsli.sType        = VK_STRUCTURE_TYPE_DESCRIPTOR_SET_LAYOUT_CREATE_INFO;
        dsli.bindingCount = 3;
        dsli.pBindings    = bs;
        if (VkResult r = vkCreateDescriptorSetLayout(device_, &dsli, nullptr, &dsl_);
            r != VK_SUCCESS)
            return fail(err, std::string("vkCreateDescriptorSetLayout: ") + vk_result_str(r));
    }

    // ----- Pipeline layout (push constants: dst dims + color matrix) -----
    {
        VkPushConstantRange pcr {};
        pcr.stageFlags = VK_SHADER_STAGE_COMPUTE_BIT;
        pcr.offset     = 0;
        pcr.size       = sizeof(ShaderPushConstants);
        VkPipelineLayoutCreateInfo pli {};
        pli.sType                  = VK_STRUCTURE_TYPE_PIPELINE_LAYOUT_CREATE_INFO;
        pli.setLayoutCount         = 1;
        pli.pSetLayouts            = &dsl_;
        pli.pushConstantRangeCount = 1;
        pli.pPushConstantRanges    = &pcr;
        if (VkResult r = vkCreatePipelineLayout(device_, &pli, nullptr, &pipeline_layout_);
            r != VK_SUCCESS)
            return fail(err, std::string("vkCreatePipelineLayout: ") + vk_result_str(r));
    }

    // ----- Compute pipeline -----
    {
        VkPipelineShaderStageCreateInfo ssi {};
        ssi.sType  = VK_STRUCTURE_TYPE_PIPELINE_SHADER_STAGE_CREATE_INFO;
        ssi.stage  = VK_SHADER_STAGE_COMPUTE_BIT;
        ssi.module = shader_;
        ssi.pName  = "main";
        VkComputePipelineCreateInfo cpi {};
        cpi.sType  = VK_STRUCTURE_TYPE_COMPUTE_PIPELINE_CREATE_INFO;
        cpi.stage  = ssi;
        cpi.layout = pipeline_layout_;
        if (VkResult r = vkCreateComputePipelines(device_, VK_NULL_HANDLE, 1, &cpi,
                                                   nullptr, &pipeline_);
            r != VK_SUCCESS)
            return fail(err, std::string("vkCreateComputePipelines: ") + vk_result_str(r));
    }

    // ----- Descriptor pool + set -----
    {
        VkDescriptorPoolSize ps[2] {};
        ps[0].type            = VK_DESCRIPTOR_TYPE_COMBINED_IMAGE_SAMPLER;
        ps[0].descriptorCount = 2;
        ps[1].type            = VK_DESCRIPTOR_TYPE_STORAGE_IMAGE;
        ps[1].descriptorCount = 1;
        VkDescriptorPoolCreateInfo dpi {};
        dpi.sType         = VK_STRUCTURE_TYPE_DESCRIPTOR_POOL_CREATE_INFO;
        dpi.maxSets       = 1;
        dpi.poolSizeCount = 2;
        dpi.pPoolSizes    = ps;
        if (VkResult r = vkCreateDescriptorPool(device_, &dpi, nullptr, &dpool_);
            r != VK_SUCCESS)
            return fail(err, std::string("vkCreateDescriptorPool: ") + vk_result_str(r));
        VkDescriptorSetAllocateInfo dsai {};
        dsai.sType              = VK_STRUCTURE_TYPE_DESCRIPTOR_SET_ALLOCATE_INFO;
        dsai.descriptorPool     = dpool_;
        dsai.descriptorSetCount = 1;
        dsai.pSetLayouts        = &dsl_;
        if (VkResult r = vkAllocateDescriptorSets(device_, &dsai, &dset_); r != VK_SUCCESS)
            return fail(err, std::string("vkAllocateDescriptorSets: ") + vk_result_str(r));
    }

    // ----- Cmd pool + buffer + per-submit fence + signal semaphore -----
    {
        VkCommandPoolCreateInfo cpi {};
        cpi.sType            = VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO;
        cpi.flags            = VK_COMMAND_POOL_CREATE_RESET_COMMAND_BUFFER_BIT;
        cpi.queueFamilyIndex = queue_family_;
        if (VkResult r = vkCreateCommandPool(device_, &cpi, nullptr, &cmd_pool_); r != VK_SUCCESS)
            return fail(err, std::string("vkCreateCommandPool: ") + vk_result_str(r));
        VkCommandBufferAllocateInfo cbi {};
        cbi.sType              = VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO;
        cbi.commandPool        = cmd_pool_;
        cbi.level              = VK_COMMAND_BUFFER_LEVEL_PRIMARY;
        cbi.commandBufferCount = 1;
        if (VkResult r = vkAllocateCommandBuffers(device_, &cbi, &cmd_); r != VK_SUCCESS)
            return fail(err, std::string("vkAllocateCommandBuffers: ") + vk_result_str(r));

        VkFenceCreateInfo fci {};
        fci.sType = VK_STRUCTURE_TYPE_FENCE_CREATE_INFO;
        if (VkResult r = vkCreateFence(device_, &fci, nullptr, &done_fence_); r != VK_SUCCESS)
            return fail(err, std::string("vkCreateFence: ") + vk_result_str(r));

        VkExportSemaphoreCreateInfo es {};
        es.sType       = VK_STRUCTURE_TYPE_EXPORT_SEMAPHORE_CREATE_INFO;
        es.handleTypes = VK_EXTERNAL_SEMAPHORE_HANDLE_TYPE_SYNC_FD_BIT;
        VkSemaphoreCreateInfo sci {};
        sci.sType = VK_STRUCTURE_TYPE_SEMAPHORE_CREATE_INFO;
        sci.pNext = &es;
        if (VkResult r = vkCreateSemaphore(device_, &sci, nullptr, &signal_sem_);
            r != VK_SUCCESS)
            return fail(err, std::string("vkCreateSemaphore(signal): ") + vk_result_str(r));
    }

    // Bindings 0/1 are stable across frames — write them once.
    {
        VkDescriptorImageInfo dii_y {};
        dii_y.sampler     = sampler_;
        dii_y.imageView   = y_view_;
        dii_y.imageLayout = VK_IMAGE_LAYOUT_SHADER_READ_ONLY_OPTIMAL;
        VkDescriptorImageInfo dii_uv {};
        dii_uv.sampler     = sampler_;
        dii_uv.imageView   = uv_view_;
        dii_uv.imageLayout = VK_IMAGE_LAYOUT_SHADER_READ_ONLY_OPTIMAL;
        VkWriteDescriptorSet ws[2] {};
        ws[0].sType           = VK_STRUCTURE_TYPE_WRITE_DESCRIPTOR_SET;
        ws[0].dstSet          = dset_;
        ws[0].dstBinding      = 0;
        ws[0].descriptorCount = 1;
        ws[0].descriptorType  = VK_DESCRIPTOR_TYPE_COMBINED_IMAGE_SAMPLER;
        ws[0].pImageInfo      = &dii_y;
        ws[1].sType           = VK_STRUCTURE_TYPE_WRITE_DESCRIPTOR_SET;
        ws[1].dstSet          = dset_;
        ws[1].dstBinding      = 1;
        ws[1].descriptorCount = 1;
        ws[1].descriptorType  = VK_DESCRIPTOR_TYPE_COMBINED_IMAGE_SAMPLER;
        ws[1].pImageInfo      = &dii_uv;
        vkUpdateDescriptorSets(device_, 2, ws, 0, nullptr);
    }

    return true;
}

int YuvToRgba::convert_nv12(VkImage             dst,
                            uint32_t            dst_w,
                            uint32_t            dst_h,
                            const uint8_t*      nv12,
                            size_t              nv12_size,
                            const ColorMatrix&  cm,
                            std::string*        err) {
    if (dst == VK_NULL_HANDLE) { fail(err, "convert_nv12: dst VkImage null"); return -1; }
    if (dst_w == 0 || dst_h == 0) { fail(err, "convert_nv12: dst_w/h zero"); return -1; }
    if ((dst_w & 1u) || (dst_h & 1u)) {
        fail(err, "convert_nv12: dst dims must be even (NV12 chroma)");
        return -1;
    }
    if (dst_w > max_w_ || dst_h > max_h_) {
        fail(err, "convert_nv12: dst exceeds configured max extent");
        return -1;
    }
    const size_t want = size_t(dst_w) * dst_h * 3 / 2;
    if (nv12_size != want) {
        fail(err, "convert_nv12: nv12_size mismatch (expected NV12 layout)");
        return -1;
    }

    /* Wait for prior submit — protects cmd_/staging_/dset_ from races. */
    if (fence_pending_) {
        if (VkResult r = vkWaitForFences(device_, 1, &done_fence_, VK_TRUE,
                                         /* 1s */ 1'000'000'000ull);
            r != VK_SUCCESS) {
            fail(err, std::string("vkWaitForFences: ") + vk_result_str(r));
            return -1;
        }
        if (VkResult r = vkResetFences(device_, 1, &done_fence_); r != VK_SUCCESS) {
            fail(err, std::string("vkResetFences: ") + vk_result_str(r));
            return -1;
        }
        fence_pending_ = false;
    }

    /* Copy NV12 bytes into staging. */
    std::memcpy(staging_map_, nv12, nv12_size);

    /* Create a transient view for the dst image — fresh each call because
     * the bridge cycles dst handles per slot. The view is destroyed at
     * the next call's fence wait via the deferred queue. */
    VkImageView dst_view = VK_NULL_HANDLE;
    {
        VkImageViewCreateInfo vci {};
        vci.sType            = VK_STRUCTURE_TYPE_IMAGE_VIEW_CREATE_INFO;
        vci.image            = dst;
        vci.viewType         = VK_IMAGE_VIEW_TYPE_2D;
        vci.format           = VK_FORMAT_R8G8B8A8_UNORM;
        vci.components       = { VK_COMPONENT_SWIZZLE_IDENTITY, VK_COMPONENT_SWIZZLE_IDENTITY,
                                 VK_COMPONENT_SWIZZLE_IDENTITY, VK_COMPONENT_SWIZZLE_IDENTITY };
        vci.subresourceRange = { VK_IMAGE_ASPECT_COLOR_BIT, 0, 1, 0, 1 };
        if (VkResult r = vkCreateImageView(device_, &vci, nullptr, &dst_view);
            r != VK_SUCCESS) {
            fail(err, std::string("vkCreateImageView(dst): ") + vk_result_str(r));
            return -1;
        }
    }

    /* Bind dst into descriptor binding 2. */
    {
        VkDescriptorImageInfo dii {};
        dii.imageView   = dst_view;
        dii.imageLayout = VK_IMAGE_LAYOUT_GENERAL;
        VkWriteDescriptorSet w {};
        w.sType           = VK_STRUCTURE_TYPE_WRITE_DESCRIPTOR_SET;
        w.dstSet          = dset_;
        w.dstBinding      = 2;
        w.descriptorCount = 1;
        w.descriptorType  = VK_DESCRIPTOR_TYPE_STORAGE_IMAGE;
        w.pImageInfo      = &dii;
        vkUpdateDescriptorSets(device_, 1, &w, 0, nullptr);
    }

    /* Reset + record. */
    if (VkResult r = vkResetCommandBuffer(cmd_, 0); r != VK_SUCCESS) {
        fail(err, std::string("vkResetCommandBuffer: ") + vk_result_str(r));
        vkDestroyImageView(device_, dst_view, nullptr);
        return -1;
    }
    VkCommandBufferBeginInfo bi {};
    bi.sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO;
    bi.flags = VK_COMMAND_BUFFER_USAGE_ONE_TIME_SUBMIT_BIT;
    if (VkResult r = vkBeginCommandBuffer(cmd_, &bi); r != VK_SUCCESS) {
        fail(err, std::string("vkBeginCommandBuffer: ") + vk_result_str(r));
        vkDestroyImageView(device_, dst_view, nullptr);
        return -1;
    }

    /* Y plane: UNDEFINED → TRANSFER_DST. */
    barrier_image(cmd_, y_image_, 0, VK_ACCESS_TRANSFER_WRITE_BIT,
                  VK_IMAGE_LAYOUT_UNDEFINED, VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL,
                  VK_PIPELINE_STAGE_TOP_OF_PIPE_BIT, VK_PIPELINE_STAGE_TRANSFER_BIT);
    /* UV plane: UNDEFINED → TRANSFER_DST. */
    barrier_image(cmd_, uv_image_, 0, VK_ACCESS_TRANSFER_WRITE_BIT,
                  VK_IMAGE_LAYOUT_UNDEFINED, VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL,
                  VK_PIPELINE_STAGE_TOP_OF_PIPE_BIT, VK_PIPELINE_STAGE_TRANSFER_BIT);

    /* Copy Y from staging[0..W*H]. */
    {
        VkBufferImageCopy bic {};
        bic.bufferOffset                    = 0;
        bic.bufferRowLength                 = 0;
        bic.bufferImageHeight               = 0;
        bic.imageSubresource.aspectMask     = VK_IMAGE_ASPECT_COLOR_BIT;
        bic.imageSubresource.layerCount     = 1;
        bic.imageOffset                     = { 0, 0, 0 };
        bic.imageExtent                     = { dst_w, dst_h, 1 };
        vkCmdCopyBufferToImage(cmd_, staging_buf_, y_image_,
                               VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL, 1, &bic);
    }
    /* Copy UV from staging[W*H..W*H + W*H/2]. */
    {
        VkBufferImageCopy bic {};
        bic.bufferOffset                    = VkDeviceSize(dst_w) * dst_h;
        bic.bufferRowLength                 = 0;
        bic.bufferImageHeight               = 0;
        bic.imageSubresource.aspectMask     = VK_IMAGE_ASPECT_COLOR_BIT;
        bic.imageSubresource.layerCount     = 1;
        bic.imageOffset                     = { 0, 0, 0 };
        bic.imageExtent                     = { dst_w / 2, dst_h / 2, 1 };
        vkCmdCopyBufferToImage(cmd_, staging_buf_, uv_image_,
                               VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL, 1, &bic);
    }

    /* Y/UV: TRANSFER_DST → SHADER_READ_ONLY. */
    barrier_image(cmd_, y_image_, VK_ACCESS_TRANSFER_WRITE_BIT, VK_ACCESS_SHADER_READ_BIT,
                  VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL, VK_IMAGE_LAYOUT_SHADER_READ_ONLY_OPTIMAL,
                  VK_PIPELINE_STAGE_TRANSFER_BIT, VK_PIPELINE_STAGE_COMPUTE_SHADER_BIT);
    barrier_image(cmd_, uv_image_, VK_ACCESS_TRANSFER_WRITE_BIT, VK_ACCESS_SHADER_READ_BIT,
                  VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL, VK_IMAGE_LAYOUT_SHADER_READ_ONLY_OPTIMAL,
                  VK_PIPELINE_STAGE_TRANSFER_BIT, VK_PIPELINE_STAGE_COMPUTE_SHADER_BIT);

    /* dst: UNDEFINED → GENERAL (storage write). */
    barrier_image(cmd_, dst, 0, VK_ACCESS_SHADER_WRITE_BIT,
                  VK_IMAGE_LAYOUT_UNDEFINED, VK_IMAGE_LAYOUT_GENERAL,
                  VK_PIPELINE_STAGE_TOP_OF_PIPE_BIT, VK_PIPELINE_STAGE_COMPUTE_SHADER_BIT);

    /* Bind + dispatch. */
    vkCmdBindPipeline(cmd_, VK_PIPELINE_BIND_POINT_COMPUTE, pipeline_);
    vkCmdBindDescriptorSets(cmd_, VK_PIPELINE_BIND_POINT_COMPUTE,
                            pipeline_layout_, 0, 1, &dset_, 0, nullptr);
    ShaderPushConstants pc {};
    pc.dst_w = dst_w; pc.dst_h = dst_h;
    for (int i = 0; i < 3; ++i) {
        pc.m_r[i]   = cm.m_r[i];
        pc.m_g[i]   = cm.m_g[i];
        pc.m_b[i]   = cm.m_b[i];
        pc.offset[i] = cm.offset[i];
    }
    vkCmdPushConstants(cmd_, pipeline_layout_, VK_SHADER_STAGE_COMPUTE_BIT,
                       0, sizeof(pc), &pc);
    const uint32_t gx = (dst_w + 7) / 8;
    const uint32_t gy = (dst_h + 7) / 8;
    vkCmdDispatch(cmd_, gx, gy, 1);

    /* dst: GENERAL → GENERAL (release to FOREIGN, matches bridge contract). */
    barrier_image(cmd_, dst,
                  VK_ACCESS_SHADER_WRITE_BIT, 0,
                  VK_IMAGE_LAYOUT_GENERAL, VK_IMAGE_LAYOUT_GENERAL,
                  VK_PIPELINE_STAGE_COMPUTE_SHADER_BIT, VK_PIPELINE_STAGE_BOTTOM_OF_PIPE_BIT,
                  queue_family_, VK_QUEUE_FAMILY_FOREIGN_EXT);

    if (VkResult r = vkEndCommandBuffer(cmd_); r != VK_SUCCESS) {
        fail(err, std::string("vkEndCommandBuffer: ") + vk_result_str(r));
        vkDestroyImageView(device_, dst_view, nullptr);
        return -1;
    }

    VkSubmitInfo si {};
    si.sType                = VK_STRUCTURE_TYPE_SUBMIT_INFO;
    si.commandBufferCount   = 1;
    si.pCommandBuffers      = &cmd_;
    si.signalSemaphoreCount = 1;
    si.pSignalSemaphores    = &signal_sem_;
    if (VkResult r = vkQueueSubmit(queue_, 1, &si, done_fence_); r != VK_SUCCESS) {
        fail(err, std::string("vkQueueSubmit: ") + vk_result_str(r));
        vkDestroyImageView(device_, dst_view, nullptr);
        return -1;
    }
    fence_pending_ = true;

    /* Export sync_fd. */
    VkSemaphoreGetFdInfoKHR sgfi {};
    sgfi.sType      = VK_STRUCTURE_TYPE_SEMAPHORE_GET_FD_INFO_KHR;
    sgfi.semaphore  = signal_sem_;
    sgfi.handleType = VK_EXTERNAL_SEMAPHORE_HANDLE_TYPE_SYNC_FD_BIT;
    int sync_fd = -1;
    if (VkResult r = vkGetSemaphoreFdKHR_(device_, &sgfi, &sync_fd); r != VK_SUCCESS) {
        fail(err, std::string("vkGetSemaphoreFdKHR: ") + vk_result_str(r));
        vkDestroyImageView(device_, dst_view, nullptr);
        return -1;
    }

    /* Schedule dst_view destruction at next fence wait. We avoid a
     * map/list by doing the simple thing: the GPU is using the view
     * until done_fence_ signals, so destroying it here is unsafe. We
     * leak it conceptually, but each frame we'd leak one — instead,
     * use a tiny single-slot deferred queue. Iter 2 simplification:
     * vkDeviceWaitIdle would stall the pipeline; instead we destroy
     * the previous frame's view *now* after recording but before
     * submit-completion is observable, exploiting the property that
     * dset_ already references it (so it's GPU-live until our fence
     * signals). Since we just waited on the fence at function entry,
     * the previous-call's `dst_view` is by definition no longer in
     * use; a single-slot queue works. */
    if (last_dst_view_ != VK_NULL_HANDLE) {
        vkDestroyImageView(device_, last_dst_view_, nullptr);
    }
    last_dst_view_ = dst_view;
    return sync_fd;
}

int YuvToRgba::convert_av_vk_frame(const VkFrameImports& im,
                                   VkImage             dst,
                                   uint32_t            dst_w,
                                   uint32_t            dst_h,
                                   const ColorMatrix&  cm,
                                   std::string*        err) {
    if (dst == VK_NULL_HANDLE) { fail(err, "convert_av_vk_frame: dst null"); return -1; }
    if (im.y_image == VK_NULL_HANDLE || im.uv_image == VK_NULL_HANDLE) {
        fail(err, "convert_av_vk_frame: AVVkFrame missing Y or UV plane "
                  "(DISABLE_MULTIPLANE not honoured?)");
        return -1;
    }
    if ((dst_w & 1u) || (dst_h & 1u)) {
        fail(err, "convert_av_vk_frame: dst dims must be even");
        return -1;
    }
    if (dst_w > max_w_ || dst_h > max_h_) {
        fail(err, "convert_av_vk_frame: dst exceeds configured max extent");
        return -1;
    }

    /* Wait for prior submit before reusing cmd_/dset_ — same protection
     * as convert_nv12; the in-flight last_*_view_ destruction below also
     * relies on this. */
    if (fence_pending_) {
        if (VkResult r = vkWaitForFences(device_, 1, &done_fence_, VK_TRUE,
                                         1'000'000'000ull);
            r != VK_SUCCESS) {
            fail(err, std::string("vkWaitForFences: ") + vk_result_str(r));
            return -1;
        }
        if (VkResult r = vkResetFences(device_, 1, &done_fence_); r != VK_SUCCESS) {
            fail(err, std::string("vkResetFences: ") + vk_result_str(r));
            return -1;
        }
        fence_pending_ = false;
    }

    /* Build per-call image views aliasing the AVVkFrame planes + dst. */
    VkImageView dst_view = VK_NULL_HANDLE;
    VkImageView y_view   = VK_NULL_HANDLE;
    VkImageView uv_view  = VK_NULL_HANDLE;

    auto cleanup_views = [&]() {
        if (dst_view) vkDestroyImageView(device_, dst_view, nullptr);
        if (y_view)   vkDestroyImageView(device_, y_view,   nullptr);
        if (uv_view)  vkDestroyImageView(device_, uv_view,  nullptr);
    };

    {
        VkImageViewCreateInfo vci {};
        vci.sType            = VK_STRUCTURE_TYPE_IMAGE_VIEW_CREATE_INFO;
        vci.viewType         = VK_IMAGE_VIEW_TYPE_2D;
        vci.subresourceRange = { VK_IMAGE_ASPECT_COLOR_BIT, 0, 1, 0, 1 };
        vci.components       = { VK_COMPONENT_SWIZZLE_IDENTITY, VK_COMPONENT_SWIZZLE_IDENTITY,
                                 VK_COMPONENT_SWIZZLE_IDENTITY, VK_COMPONENT_SWIZZLE_IDENTITY };

        vci.image  = im.y_image;
        vci.format = VK_FORMAT_R8_UNORM;
        if (VkResult r = vkCreateImageView(device_, &vci, nullptr, &y_view);
            r != VK_SUCCESS) {
            fail(err, std::string("vkCreateImageView(Y, AVVkFrame): ") + vk_result_str(r));
            cleanup_views(); return -1;
        }
        vci.image  = im.uv_image;
        vci.format = VK_FORMAT_R8G8_UNORM;
        if (VkResult r = vkCreateImageView(device_, &vci, nullptr, &uv_view);
            r != VK_SUCCESS) {
            fail(err, std::string("vkCreateImageView(UV, AVVkFrame): ") + vk_result_str(r));
            cleanup_views(); return -1;
        }
        vci.image  = dst;
        vci.format = VK_FORMAT_R8G8B8A8_UNORM;
        if (VkResult r = vkCreateImageView(device_, &vci, nullptr, &dst_view);
            r != VK_SUCCESS) {
            fail(err, std::string("vkCreateImageView(dst, AVVkFrame): ") + vk_result_str(r));
            cleanup_views(); return -1;
        }
    }

    /* Re-bind all three descriptor slots: Y/UV now alias FFmpeg's
     * images, dst is a fresh slot. */
    {
        VkDescriptorImageInfo dii_y  { sampler_, y_view,   VK_IMAGE_LAYOUT_SHADER_READ_ONLY_OPTIMAL };
        VkDescriptorImageInfo dii_uv { sampler_, uv_view,  VK_IMAGE_LAYOUT_SHADER_READ_ONLY_OPTIMAL };
        VkDescriptorImageInfo dii_d  { VK_NULL_HANDLE, dst_view, VK_IMAGE_LAYOUT_GENERAL };
        VkWriteDescriptorSet ws[3] {};
        for (int i = 0; i < 3; ++i) {
            ws[i].sType           = VK_STRUCTURE_TYPE_WRITE_DESCRIPTOR_SET;
            ws[i].dstSet          = dset_;
            ws[i].dstBinding      = static_cast<uint32_t>(i);
            ws[i].descriptorCount = 1;
        }
        ws[0].descriptorType = VK_DESCRIPTOR_TYPE_COMBINED_IMAGE_SAMPLER;
        ws[0].pImageInfo     = &dii_y;
        ws[1].descriptorType = VK_DESCRIPTOR_TYPE_COMBINED_IMAGE_SAMPLER;
        ws[1].pImageInfo     = &dii_uv;
        ws[2].descriptorType = VK_DESCRIPTOR_TYPE_STORAGE_IMAGE;
        ws[2].pImageInfo     = &dii_d;
        vkUpdateDescriptorSets(device_, 3, ws, 0, nullptr);
    }

    if (VkResult r = vkResetCommandBuffer(cmd_, 0); r != VK_SUCCESS) {
        fail(err, std::string("vkResetCommandBuffer: ") + vk_result_str(r));
        cleanup_views(); return -1;
    }
    VkCommandBufferBeginInfo bi {};
    bi.sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO;
    bi.flags = VK_COMMAND_BUFFER_USAGE_ONE_TIME_SUBMIT_BIT;
    if (VkResult r = vkBeginCommandBuffer(cmd_, &bi); r != VK_SUCCESS) {
        fail(err, std::string("vkBeginCommandBuffer: ") + vk_result_str(r));
        cleanup_views(); return -1;
    }

    /* Acquire Y/UV from FFmpeg's queue family → ours, transition to
     * SHADER_READ_ONLY. */
    const uint32_t y_src_qf  = *im.y_qf_in_out;
    const uint32_t uv_src_qf = *im.uv_qf_in_out;
    barrier_image(cmd_, im.y_image,
                  0, VK_ACCESS_SHADER_READ_BIT,
                  *im.y_layout_in_out, VK_IMAGE_LAYOUT_SHADER_READ_ONLY_OPTIMAL,
                  VK_PIPELINE_STAGE_TOP_OF_PIPE_BIT, VK_PIPELINE_STAGE_COMPUTE_SHADER_BIT,
                  y_src_qf, queue_family_);
    barrier_image(cmd_, im.uv_image,
                  0, VK_ACCESS_SHADER_READ_BIT,
                  *im.uv_layout_in_out, VK_IMAGE_LAYOUT_SHADER_READ_ONLY_OPTIMAL,
                  VK_PIPELINE_STAGE_TOP_OF_PIPE_BIT, VK_PIPELINE_STAGE_COMPUTE_SHADER_BIT,
                  uv_src_qf, queue_family_);

    /* dst: UNDEFINED → GENERAL. */
    barrier_image(cmd_, dst, 0, VK_ACCESS_SHADER_WRITE_BIT,
                  VK_IMAGE_LAYOUT_UNDEFINED, VK_IMAGE_LAYOUT_GENERAL,
                  VK_PIPELINE_STAGE_TOP_OF_PIPE_BIT, VK_PIPELINE_STAGE_COMPUTE_SHADER_BIT);

    vkCmdBindPipeline(cmd_, VK_PIPELINE_BIND_POINT_COMPUTE, pipeline_);
    vkCmdBindDescriptorSets(cmd_, VK_PIPELINE_BIND_POINT_COMPUTE,
                            pipeline_layout_, 0, 1, &dset_, 0, nullptr);
    ShaderPushConstants pc {};
    pc.dst_w = dst_w; pc.dst_h = dst_h;
    for (int i = 0; i < 3; ++i) {
        pc.m_r[i]   = cm.m_r[i];
        pc.m_g[i]   = cm.m_g[i];
        pc.m_b[i]   = cm.m_b[i];
        pc.offset[i] = cm.offset[i];
    }
    vkCmdPushConstants(cmd_, pipeline_layout_, VK_SHADER_STAGE_COMPUTE_BIT,
                       0, sizeof(pc), &pc);
    const uint32_t gx = (dst_w + 7) / 8;
    const uint32_t gy = (dst_h + 7) / 8;
    vkCmdDispatch(cmd_, gx, gy, 1);

    /* Release Y/UV back to FFmpeg's queue family in GENERAL layout
     * (FFmpeg's decoder expects that on the next decode submit), and
     * release dst to FOREIGN for the bridge consumer. */
    barrier_image(cmd_, im.y_image,
                  VK_ACCESS_SHADER_READ_BIT, 0,
                  VK_IMAGE_LAYOUT_SHADER_READ_ONLY_OPTIMAL, VK_IMAGE_LAYOUT_GENERAL,
                  VK_PIPELINE_STAGE_COMPUTE_SHADER_BIT, VK_PIPELINE_STAGE_BOTTOM_OF_PIPE_BIT,
                  queue_family_, y_src_qf);
    barrier_image(cmd_, im.uv_image,
                  VK_ACCESS_SHADER_READ_BIT, 0,
                  VK_IMAGE_LAYOUT_SHADER_READ_ONLY_OPTIMAL, VK_IMAGE_LAYOUT_GENERAL,
                  VK_PIPELINE_STAGE_COMPUTE_SHADER_BIT, VK_PIPELINE_STAGE_BOTTOM_OF_PIPE_BIT,
                  queue_family_, uv_src_qf);
    barrier_image(cmd_, dst,
                  VK_ACCESS_SHADER_WRITE_BIT, 0,
                  VK_IMAGE_LAYOUT_GENERAL, VK_IMAGE_LAYOUT_GENERAL,
                  VK_PIPELINE_STAGE_COMPUTE_SHADER_BIT, VK_PIPELINE_STAGE_BOTTOM_OF_PIPE_BIT,
                  queue_family_, VK_QUEUE_FAMILY_FOREIGN_EXT);

    if (VkResult r = vkEndCommandBuffer(cmd_); r != VK_SUCCESS) {
        fail(err, std::string("vkEndCommandBuffer: ") + vk_result_str(r));
        cleanup_views(); return -1;
    }

    /* Wait on AVVkFrame's timeline semaphores at their current values,
     * signal incremented values back. Plus our binary signal_sem_ for
     * the bridge SYNC_FD export. */
    const uint64_t y_wait_val   = *im.y_sem_val_in_out;
    const uint64_t uv_wait_val  = *im.uv_sem_val_in_out;
    const uint64_t y_signal_val  = y_wait_val  + 1;
    const uint64_t uv_signal_val = uv_wait_val + 1;

    VkSemaphore wait_sems[2]  = { im.y_sem, im.uv_sem };
    uint64_t    wait_vals[2]  = { y_wait_val, uv_wait_val };
    VkPipelineStageFlags wait_stages[2] = {
        VK_PIPELINE_STAGE_COMPUTE_SHADER_BIT,
        VK_PIPELINE_STAGE_COMPUTE_SHADER_BIT,
    };
    /* Signal: timelines + binary (binary value is ignored; we set 0). */
    VkSemaphore signal_sems[3] = { im.y_sem, im.uv_sem, signal_sem_ };
    uint64_t    signal_vals[3] = { y_signal_val, uv_signal_val, 0 };

    VkTimelineSemaphoreSubmitInfo tsi {};
    tsi.sType                     = VK_STRUCTURE_TYPE_TIMELINE_SEMAPHORE_SUBMIT_INFO;
    tsi.waitSemaphoreValueCount   = 2;
    tsi.pWaitSemaphoreValues      = wait_vals;
    tsi.signalSemaphoreValueCount = 3;
    tsi.pSignalSemaphoreValues    = signal_vals;

    VkSubmitInfo si {};
    si.sType                = VK_STRUCTURE_TYPE_SUBMIT_INFO;
    si.pNext                = &tsi;
    si.waitSemaphoreCount   = 2;
    si.pWaitSemaphores      = wait_sems;
    si.pWaitDstStageMask    = wait_stages;
    si.commandBufferCount   = 1;
    si.pCommandBuffers      = &cmd_;
    si.signalSemaphoreCount = 3;
    si.pSignalSemaphores    = signal_sems;
    if (VkResult r = vkQueueSubmit(queue_, 1, &si, done_fence_); r != VK_SUCCESS) {
        fail(err, std::string("vkQueueSubmit: ") + vk_result_str(r));
        cleanup_views(); return -1;
    }
    fence_pending_ = true;

    /* Update the AVVkFrame's tracked state — caller's contract. */
    *im.y_sem_val_in_out  = y_signal_val;
    *im.uv_sem_val_in_out = uv_signal_val;
    *im.y_layout_in_out   = VK_IMAGE_LAYOUT_GENERAL;
    *im.uv_layout_in_out  = VK_IMAGE_LAYOUT_GENERAL;
    *im.y_qf_in_out       = y_src_qf;   /* released back to FFmpeg's family */
    *im.uv_qf_in_out      = uv_src_qf;

    /* Export sync_fd for the bridge. */
    VkSemaphoreGetFdInfoKHR sgfi {};
    sgfi.sType      = VK_STRUCTURE_TYPE_SEMAPHORE_GET_FD_INFO_KHR;
    sgfi.semaphore  = signal_sem_;
    sgfi.handleType = VK_EXTERNAL_SEMAPHORE_HANDLE_TYPE_SYNC_FD_BIT;
    int sync_fd = -1;
    if (VkResult r = vkGetSemaphoreFdKHR_(device_, &sgfi, &sync_fd); r != VK_SUCCESS) {
        fail(err, std::string("vkGetSemaphoreFdKHR: ") + vk_result_str(r));
        cleanup_views(); return -1;
    }

    /* Cycle the per-call views into the deferred destroy slots. */
    if (last_dst_view_) vkDestroyImageView(device_, last_dst_view_, nullptr);
    if (last_y_view_)   vkDestroyImageView(device_, last_y_view_,   nullptr);
    if (last_uv_view_)  vkDestroyImageView(device_, last_uv_view_,  nullptr);
    last_dst_view_ = dst_view;
    last_y_view_   = y_view;
    last_uv_view_  = uv_view;
    return sync_fd;
}

} // namespace waywallen::ffvk
