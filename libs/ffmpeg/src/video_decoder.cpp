#include "video_decoder.hpp"

#include "vk_device.hpp"

extern "C" {
#include <libavcodec/avcodec.h>
#include <libavformat/avformat.h>
#include <libavutil/hwcontext.h>
#include <libavutil/hwcontext_vulkan.h>
#include <libavutil/imgutils.h>
#include <libavutil/pixdesc.h>
#include <libswscale/swscale.h>
}

#include <cstdio>
#include <cstring>

namespace waywallen::ffvk {

namespace {

struct FmtCtxDeleter {
    void operator()(AVFormatContext* p) const noexcept {
        if (p) avformat_close_input(&p);
    }
};
struct CodecCtxDeleter {
    void operator()(AVCodecContext* p) const noexcept {
        if (p) avcodec_free_context(&p);
    }
};
struct FrameDeleter {
    void operator()(AVFrame* p) const noexcept {
        if (p) av_frame_free(&p);
    }
};
struct PacketDeleter {
    void operator()(AVPacket* p) const noexcept {
        if (p) av_packet_free(&p);
    }
};
struct SwsDeleter {
    void operator()(SwsContext* p) const noexcept {
        if (p) sws_freeContext(p);
    }
};
struct BufRefDeleter {
    void operator()(AVBufferRef* p) const noexcept {
        if (p) av_buffer_unref(&p);
    }
};

using FmtCtxPtr   = std::unique_ptr<AVFormatContext, FmtCtxDeleter>;
using CodecCtxPtr = std::unique_ptr<AVCodecContext, CodecCtxDeleter>;
using FramePtr    = std::unique_ptr<AVFrame, FrameDeleter>;
using PacketPtr   = std::unique_ptr<AVPacket, PacketDeleter>;
using SwsPtr      = std::unique_ptr<SwsContext, SwsDeleter>;
using BufRefPtr   = std::unique_ptr<AVBufferRef, BufRefDeleter>;

/* Defined further down — forward-declared so the helpers above the
 * definitions can use them. */
bool fail(DecodeError* err, std::string m);
std::string av_err_str(int rc);

/* Translate FFmpeg's colorspace/range enums into our ColorSpace /
 * ColorRange ints (which the public Nv12Frame / VkFrameView carry).
 * Unknowns default to BT.709 limited — the most common case. */
uint32_t map_colorspace(int cs) {
    switch (cs) {
    case AVCOL_SPC_BT709:        return 0;
    case AVCOL_SPC_BT470BG:      // PAL / BT.601 625
    case AVCOL_SPC_SMPTE170M:    return 1;
    case AVCOL_SPC_BT2020_NCL:   return 2;
    case AVCOL_SPC_BT2020_CL:    return 2;
    default:                     return 0;
    }
}
uint32_t map_range(int r) {
    return (r == AVCOL_RANGE_JPEG) ? 1u : 0u;
}

/* `get_format` callback: prefer AV_PIX_FMT_VULKAN whenever the codec
 * offers it; fall back to whatever FFmpeg picks by default otherwise.
 * In shared-device mode this also allocates the codec's hw_frames_ctx
 * with DISABLE_MULTIPLANE so AVVkFrames come back as 2-image NV12
 * (img[0]=Y, img[1]=UV) — that's the format the GPU YUV→RGB shader
 * expects. */
AVPixelFormat get_format_prefer_vulkan(AVCodecContext* cctx,
                                       const AVPixelFormat* fmts) {
    bool offers_vulkan = false;
    for (const AVPixelFormat* p = fmts; *p != AV_PIX_FMT_NONE; ++p) {
        if (*p == AV_PIX_FMT_VULKAN) { offers_vulkan = true; break; }
    }
    if (!offers_vulkan) return avcodec_default_get_format(cctx, fmts);

    /* Allocate hw_frames_ctx so we can opt into DISABLE_MULTIPLANE; if
     * we leave it null FFmpeg auto-picks a format which on AMD/RADV is
     * a multiplanar VK_FORMAT_G8_B8R8_2PLANE_420_UNORM that requires a
     * VkSamplerYcbcrConversion-aware shader. Forcing 2-image NV12 lets
     * us use the existing R8 + R8G8 sampler bindings. */
    if (!cctx->hw_device_ctx) return AV_PIX_FMT_VULKAN;  /* FFmpeg-managed mode */
    AVBufferRef* fr = av_hwframe_ctx_alloc(cctx->hw_device_ctx);
    if (!fr) return avcodec_default_get_format(cctx, fmts);
    auto* fc  = reinterpret_cast<AVHWFramesContext*>(fr->data);
    fc->format    = AV_PIX_FMT_VULKAN;
    fc->sw_format = AV_PIX_FMT_NV12;
    fc->width     = cctx->coded_width  > 0 ? cctx->coded_width  : cctx->width;
    fc->height    = cctx->coded_height > 0 ? cctx->coded_height : cctx->height;
    auto* vfc = reinterpret_cast<AVVulkanFramesContext*>(fc->hwctx);
    vfc->flags = static_cast<AVVkFrameFlags>(vfc->flags | AV_VK_FRAME_FLAG_DISABLE_MULTIPLANE);
    if (av_hwframe_ctx_init(fr) < 0) {
        av_buffer_unref(&fr);
        /* Couldn't init disjoint NV12 — fall back to sw decode. */
        return avcodec_default_get_format(cctx, fmts);
    }
    cctx->hw_frames_ctx = fr;
    return AV_PIX_FMT_VULKAN;
}

/* Build an AV_HWDEVICE_TYPE_VULKAN context wrapping the caller's
 * Producer-owned VkInstance/VkDevice. Returns a populated AVBufferRef
 * on success, or null + populated *err on any failure. */
AVBufferRef* make_shared_vulkan_hwdevice(const Producer& vk, DecodeError* err) {
    AVBufferRef* hwd = av_hwdevice_ctx_alloc(AV_HWDEVICE_TYPE_VULKAN);
    if (!hwd) {
        fail(err, "av_hwdevice_ctx_alloc(VULKAN) failed");
        return nullptr;
    }
    auto* dctx = reinterpret_cast<AVHWDeviceContext*>(hwd->data);
    auto* vctx = reinterpret_cast<AVVulkanDeviceContext*>(dctx->hwctx);

    vctx->get_proc_addr = vkGetInstanceProcAddr;
    vctx->inst          = vk.instance();
    vctx->phys_dev      = vk.physical_device();
    vctx->act_dev       = vk.device();

    const auto& iexts = vk.enabled_instance_extensions();
    const auto& dexts = vk.enabled_device_extensions();
    vctx->enabled_inst_extensions    = iexts.empty() ? nullptr : iexts.data();
    vctx->nb_enabled_inst_extensions = static_cast<int>(iexts.size());
    vctx->enabled_dev_extensions     = dexts.empty() ? nullptr : dexts.data();
    vctx->nb_enabled_dev_extensions  = static_cast<int>(dexts.size());

    const auto& qfs = vk.queue_families();
    vctx->nb_qf = 0;
    for (const auto& q : qfs) {
        if (vctx->nb_qf >= static_cast<int>(sizeof(vctx->qf) / sizeof(vctx->qf[0])))
            break;
        AVVulkanDeviceQueueFamily entry {};
        entry.idx        = static_cast<int>(q.index);
        entry.num        = 1;
        entry.flags      = static_cast<VkQueueFlagBits>(q.flags);
        entry.video_caps = static_cast<VkVideoCodecOperationFlagBitsKHR>(q.video_caps);
        vctx->qf[vctx->nb_qf++] = entry;
    }

    if (int rc = av_hwdevice_ctx_init(hwd); rc < 0) {
        fail(err, "av_hwdevice_ctx_init(shared VULKAN): " + av_err_str(rc));
        av_buffer_unref(&hwd);
        return nullptr;
    }
    return hwd;
}

bool fail(DecodeError* err, std::string m) {
    if (err) err->message = std::move(m);
    return false;
}

std::string av_err_str(int rc) {
    char buf[AV_ERROR_MAX_STRING_SIZE] = {};
    av_strerror(rc, buf, sizeof(buf));
    return std::string(buf);
}

} // namespace

struct VideoDecoder::State {
    FmtCtxPtr     fmt;
    CodecCtxPtr   cctx;
    PacketPtr     pkt;
    FramePtr      src_frame;
    /* Sw landing frame for vulkan→sw downloads via
     * av_hwframe_transfer_data. Allocated lazily on first hw frame. */
    FramePtr      sw_frame;
    SwsPtr        sws;
    /* AV_HWDEVICE_TYPE_VULKAN context owned by the codec when present.
     * Best-effort: a NULL `hwd` here just means we run sw decode. */
    BufRefPtr     hwd;
    AVPixelFormat sws_src_fmt { AV_PIX_FMT_NONE };
    int           sws_src_w   { 0 };
    int           sws_src_h   { 0 };
    int           video_idx   { -1 };
    AVRational    stream_tb   { 0, 1 };
    bool          flushing    { false };
};

namespace {

bool ensure_sws(VideoDecoder::State& st, int src_w, int src_h, AVPixelFormat src_fmt,
                uint32_t target_w, uint32_t target_h) {
    if (st.sws && st.sws_src_w == src_w && st.sws_src_h == src_h
        && st.sws_src_fmt == src_fmt) {
        return true;
    }
    /* Always emit NV12 — that's what YuvToRgba consumes. */
    st.sws.reset(sws_getContext(src_w, src_h, src_fmt,
                                static_cast<int>(target_w),
                                static_cast<int>(target_h),
                                AV_PIX_FMT_NV12,
                                SWS_BICUBIC, nullptr, nullptr, nullptr));
    if (!st.sws) return false;
    st.sws_src_w = src_w;
    st.sws_src_h = src_h;
    st.sws_src_fmt = src_fmt;
    return true;
}

bool seek_to_start(VideoDecoder::State& st) {
    int rc = av_seek_frame(st.fmt.get(), -1, 0, AVSEEK_FLAG_BACKWARD);
    if (rc < 0) return false;
    avcodec_flush_buffers(st.cctx.get());
    st.flushing = false;
    return true;
}

} // namespace

VideoDecoder::~VideoDecoder() = default;

std::unique_ptr<VideoDecoder>
VideoDecoder::open(const std::string& path,
                   uint32_t            target_w,
                   uint32_t            target_h,
                   bool                loop,
                   DecodeError*        err) {
    return build_internal(path, target_w, target_h, loop,
                          /*pre_built_hwdev=*/nullptr, err);
}

std::unique_ptr<VideoDecoder>
VideoDecoder::open_with_vk(const std::string&  path,
                           uint32_t             target_w,
                           uint32_t             target_h,
                           bool                 loop,
                           const Producer&      vk,
                           DecodeError*         err) {
    DecodeError local_err;
    AVBufferRef* hwd = make_shared_vulkan_hwdevice(vk, &local_err);
    if (!hwd) {
        std::fprintf(stderr,
                     "VideoDecoder: shared-device vulkan setup failed: %s — "
                     "falling back to FFmpeg-managed hwdevice.\n",
                     local_err.message.c_str());
        return build_internal(path, target_w, target_h, loop,
                              /*pre_built_hwdev=*/nullptr, err);
    }
    return build_internal(path, target_w, target_h, loop, hwd, err);
}

std::unique_ptr<VideoDecoder>
VideoDecoder::build_internal(const std::string& path,
                             uint32_t target_w, uint32_t target_h,
                             bool loop, void* pre_built_hwdev_v,
                             DecodeError* err) {
    AVBufferRef* pre_built_hwdev = static_cast<AVBufferRef*>(pre_built_hwdev_v);
    if (target_w == 0 || target_h == 0) {
        fail(err, "target dimensions must be non-zero");
        if (pre_built_hwdev) av_buffer_unref(&pre_built_hwdev);
        return nullptr;
    }
    /* NV12 chroma is half-resolution → both dims must be even. */
    if (target_w & 1u) ++target_w;
    if (target_h & 1u) ++target_h;

    auto self = std::unique_ptr<VideoDecoder>(new VideoDecoder());
    self->target_w_ = target_w;
    self->target_h_ = target_h;
    self->loop_     = loop;
    self->st_       = std::make_unique<VideoDecoder::State>();

    AVFormatContext* raw_fmt = nullptr;
    if (int rc = avformat_open_input(&raw_fmt, path.c_str(), nullptr, nullptr);
        rc < 0) {
        fail(err, "avformat_open_input: " + av_err_str(rc));
        return nullptr;
    }
    self->st_->fmt.reset(raw_fmt);

    if (int rc = avformat_find_stream_info(self->st_->fmt.get(), nullptr); rc < 0) {
        fail(err, "avformat_find_stream_info: " + av_err_str(rc));
        return nullptr;
    }

    int idx = av_find_best_stream(self->st_->fmt.get(),
                                  AVMEDIA_TYPE_VIDEO, -1, -1, nullptr, 0);
    if (idx < 0) { fail(err, "no video stream in file"); return nullptr; }
    self->st_->video_idx = idx;
    AVStream*           st  = self->st_->fmt->streams[idx];
    AVCodecParameters*  par = st->codecpar;
    self->st_->stream_tb = st->time_base;

    const AVCodec* dec = avcodec_find_decoder(par->codec_id);
    if (!dec) {
        fail(err, std::string("no decoder for codec ") + avcodec_get_name(par->codec_id));
        return nullptr;
    }
    self->st_->cctx.reset(avcodec_alloc_context3(dec));
    if (!self->st_->cctx) { fail(err, "avcodec_alloc_context3 failed"); return nullptr; }
    if (int rc = avcodec_parameters_to_context(self->st_->cctx.get(), par); rc < 0) {
        fail(err, "avcodec_parameters_to_context: " + av_err_str(rc));
        return nullptr;
    }

    /* Iter 4: best-effort vulkan hwdevice. If FFmpeg was built without
     * vulkan support or the platform lacks the right driver, the create
     * call fails and we silently fall back to sw decode (keeping the
     * Iter 2 path live). When it succeeds, the get_format callback
     * picks AV_PIX_FMT_VULKAN whenever the codec offers it. The codec
     * takes a ref on the hwdevice ctx, so we keep our own ref alive
     * for the codec's lifetime via state.hwd. */
    {
        AVBufferRef* hwd = pre_built_hwdev;
        if (!hwd) {
            int rc = av_hwdevice_ctx_create(&hwd, AV_HWDEVICE_TYPE_VULKAN,
                                            nullptr, nullptr, 0);
            if (rc < 0) hwd = nullptr;
        }
        if (hwd) {
            /* Take ownership; the codec gets its own ref. */
            self->st_->hwd.reset(hwd);
            self->st_->cctx->hw_device_ctx = av_buffer_ref(hwd);
            self->st_->cctx->get_format    = get_format_prefer_vulkan;
            std::fprintf(stderr,
                         "VideoDecoder: AV_HWDEVICE_TYPE_VULKAN attached "
                         "(%s device); codec %s will use vulkan decode "
                         "when supported.\n",
                         pre_built_hwdev ? "shared" : "FFmpeg-owned",
                         avcodec_get_name(par->codec_id));
        } else {
            std::fprintf(stderr,
                         "VideoDecoder: vulkan hwdevice unavailable; "
                         "running sw decode for codec %s.\n",
                         avcodec_get_name(par->codec_id));
        }
    }

    if (int rc = avcodec_open2(self->st_->cctx.get(), dec, nullptr); rc < 0) {
        fail(err, "avcodec_open2: " + av_err_str(rc));
        return nullptr;
    }

    /* If get_format succeeded in producing a vulkan hw_frames_ctx we
     * route through the zero-copy `next_vk_frame` path; otherwise
     * (codec rejected vulkan / disjoint NV12 init failed) we fall back
     * to the sw `next_frame` path with av_hwframe_transfer_data. */
    self->using_vk_frames_ = pre_built_hwdev != nullptr
                          && self->st_->cctx->hw_frames_ctx != nullptr;

    self->st_->pkt.reset(av_packet_alloc());
    self->st_->src_frame.reset(av_frame_alloc());
    if (!self->st_->pkt || !self->st_->src_frame) {
        fail(err, "av_packet_alloc / av_frame_alloc failed");
        return nullptr;
    }
    return self;
}

FrameStatus VideoDecoder::next_vk_frame(VkFrameView& out, DecodeError* err) {
    if (!using_vk_frames_) {
        fail(err, "next_vk_frame called on non-shared-device decoder");
        return FrameStatus::error;
    }
    State& st = *st_;

    /* Release the previously-yielded AVVkFrame back to the pool. The
     * caller's GPU work that referenced it has been queue-submitted by
     * now (the contract of next_vk_frame), so it's safe to unref —
     * the AVVkFrame survives in the pool's hwframe context. */
    av_frame_unref(st.src_frame.get());

    while (true) {
        int rc = avcodec_receive_frame(st.cctx.get(), st.src_frame.get());
        if (rc == 0) {
            if (st.src_frame->format != AV_PIX_FMT_VULKAN) {
                fail(err, "next_vk_frame: decoder produced non-vulkan frame");
                return FrameStatus::error;
            }
            auto* vkf = reinterpret_cast<AVVkFrame*>(st.src_frame->data[0]);
            out.img          = vkf->img;
            out.layout       = vkf->layout;
            out.sem          = vkf->sem;
            out.sem_value    = vkf->sem_value;
            out.queue_family = vkf->queue_family;
            out.plane_count  = (vkf->img[1] != VK_NULL_HANDLE) ? 2u : 1u;
            out.width        = static_cast<uint32_t>(st.src_frame->width);
            out.height       = static_cast<uint32_t>(st.src_frame->height);
            out.colorspace   = map_colorspace(st.src_frame->colorspace);
            out.color_range  = map_range(st.src_frame->color_range);
            const int64_t pts = (st.src_frame->best_effort_timestamp != AV_NOPTS_VALUE)
                ? st.src_frame->best_effort_timestamp
                : st.src_frame->pts;
            out.pts_seconds = (pts == AV_NOPTS_VALUE)
                ? -1.0
                : static_cast<double>(pts) * av_q2d(st.stream_tb);
            return FrameStatus::ok;
        }
        if (rc == AVERROR_EOF) {
            if (loop_) {
                if (!seek_to_start(st)) {
                    fail(err, "loop seek-to-zero failed");
                    return FrameStatus::error;
                }
                continue;
            }
            return FrameStatus::eof;
        }
        if (rc != AVERROR(EAGAIN)) {
            fail(err, "avcodec_receive_frame: " + av_err_str(rc));
            return FrameStatus::error;
        }
        if (st.flushing) continue;

        rc = av_read_frame(st.fmt.get(), st.pkt.get());
        if (rc == AVERROR_EOF) {
            avcodec_send_packet(st.cctx.get(), nullptr);
            st.flushing = true;
            continue;
        }
        if (rc < 0) {
            fail(err, "av_read_frame: " + av_err_str(rc));
            return FrameStatus::error;
        }
        if (st.pkt->stream_index != st.video_idx) {
            av_packet_unref(st.pkt.get());
            continue;
        }
        rc = avcodec_send_packet(st.cctx.get(), st.pkt.get());
        av_packet_unref(st.pkt.get());
        if (rc < 0 && rc != AVERROR(EAGAIN)) {
            fail(err, "avcodec_send_packet: " + av_err_str(rc));
            return FrameStatus::error;
        }
    }
}

FrameStatus VideoDecoder::next_frame(Nv12Frame& out, DecodeError* err) {
    State& st = *st_;

    /* Resize output buffer to NV12 size on first call (and on extent
     * change, but the extent is fixed for VideoDecoder lifetime). */
    const size_t want = size_t(target_w_) * target_h_ * 3 / 2;
    if (out.width != target_w_ || out.height != target_h_ || out.data.size() != want) {
        out.width  = target_w_;
        out.height = target_h_;
        out.data.assign(want, 0u);
    }

    while (true) {
        int rc = avcodec_receive_frame(st.cctx.get(), st.src_frame.get());
        if (rc == 0) {
            /* If the decoder produced a vulkan-typed frame (Iter 4 hw
             * path), download it to a sw frame first. The download lands
             * in whatever YUV format the AVHWFramesContext exposes —
             * typically NV12 — and swscale handles whatever it is. */
            AVFrame* feed = st.src_frame.get();
            if (feed->format == AV_PIX_FMT_VULKAN) {
                if (!st.sw_frame) st.sw_frame.reset(av_frame_alloc());
                if (!st.sw_frame) {
                    fail(err, "av_frame_alloc(sw_frame) failed");
                    return FrameStatus::error;
                }
                av_frame_unref(st.sw_frame.get());
                int trc = av_hwframe_transfer_data(st.sw_frame.get(), feed, 0);
                if (trc < 0) {
                    fail(err, "av_hwframe_transfer_data: " + av_err_str(trc));
                    av_frame_unref(st.src_frame.get());
                    return FrameStatus::error;
                }
                /* Preserve PTS across the transfer (transfer_data copies
                 * pixel data only). */
                st.sw_frame->pts                    = feed->pts;
                st.sw_frame->best_effort_timestamp  = feed->best_effort_timestamp;
                feed = st.sw_frame.get();
            }

            const auto src_fmt = static_cast<AVPixelFormat>(feed->format);
            const int  src_w   = feed->width;
            const int  src_h   = feed->height;
            if (src_w <= 0 || src_h <= 0 || src_fmt == AV_PIX_FMT_NONE) {
                fail(err, "decoded frame has invalid dimensions/format");
                return FrameStatus::error;
            }
            if (!ensure_sws(st, src_w, src_h, src_fmt, target_w_, target_h_)) {
                fail(err, std::string("sws_getContext failed (src=") +
                          av_get_pix_fmt_name(src_fmt) + ")");
                return FrameStatus::error;
            }
            uint8_t* y_dst  = out.data.data();
            uint8_t* uv_dst = out.data.data() + size_t(target_w_) * target_h_;
            uint8_t* dst_planes[4]  = { y_dst, uv_dst, nullptr, nullptr };
            int      dst_strides[4] = { static_cast<int>(target_w_),
                                        static_cast<int>(target_w_),  /* NV12 UV pitch == width */
                                        0, 0 };
            int scaled = sws_scale(st.sws.get(),
                                   feed->data, feed->linesize,
                                   0, src_h, dst_planes, dst_strides);
            if (scaled <= 0) {
                fail(err, "sws_scale produced no rows");
                return FrameStatus::error;
            }
            const int64_t pts = (feed->best_effort_timestamp != AV_NOPTS_VALUE)
                ? feed->best_effort_timestamp
                : feed->pts;
            out.pts_seconds = (pts == AV_NOPTS_VALUE)
                ? -1.0
                : static_cast<double>(pts) * av_q2d(st.stream_tb);
            out.colorspace  = map_colorspace(feed->colorspace);
            out.color_range = map_range(feed->color_range);
            av_frame_unref(st.src_frame.get());
            if (st.sw_frame) av_frame_unref(st.sw_frame.get());
            return FrameStatus::ok;
        }
        if (rc == AVERROR_EOF) {
            if (loop_) {
                if (!seek_to_start(st)) {
                    fail(err, "loop seek-to-zero failed");
                    return FrameStatus::error;
                }
                continue;
            }
            return FrameStatus::eof;
        }
        if (rc != AVERROR(EAGAIN)) {
            fail(err, "avcodec_receive_frame: " + av_err_str(rc));
            return FrameStatus::error;
        }

        if (st.flushing) continue;

        rc = av_read_frame(st.fmt.get(), st.pkt.get());
        if (rc == AVERROR_EOF) {
            avcodec_send_packet(st.cctx.get(), nullptr);
            st.flushing = true;
            continue;
        }
        if (rc < 0) {
            fail(err, "av_read_frame: " + av_err_str(rc));
            return FrameStatus::error;
        }
        if (st.pkt->stream_index != st.video_idx) {
            av_packet_unref(st.pkt.get());
            continue;
        }
        rc = avcodec_send_packet(st.cctx.get(), st.pkt.get());
        av_packet_unref(st.pkt.get());
        if (rc < 0 && rc != AVERROR(EAGAIN)) {
            fail(err, "avcodec_send_packet: " + av_err_str(rc));
            return FrameStatus::error;
        }
    }
}

} // namespace waywallen::ffvk
