#!/usr/bin/env bash
# Build a minimal FFmpeg from source into the active conda env.
#
# Why source instead of conda-forge:
#   - control the codec / demuxer / filter set (smaller binary, fewer deps)
#   - inherit the same sysroot/glibc baseline as the rest of the project,
#     because configure picks up CC and CFLAGS / LDFLAGS from the activated
#     conda env (which the clang_linux-64 activation populates with --sysroot).
#
# Idempotent: skips the build if libavcodec.pc is already present in
# $CONDA_PREFIX/lib/pkgconfig/. Set FORCE=1 to rebuild.
#
# Tunables (env vars):
#   FFMPEG_VERSION   git tag to check out, default n8.1
#   FORCE            set to 1 to rebuild even if the pkg-config stamp exists

set -euo pipefail

[[ -n "${CONDA_PREFIX:-}" ]] || {
    printf '\033[1;31mERROR:\033[0m CONDA_PREFIX not set; activate the conda env first\n' >&2
    exit 1
}

FFMPEG_VERSION="${FFMPEG_VERSION:-n8.1}"
FFMPEG_SRC="$CONDA_PREFIX/.ffmpeg-src"
PKG_STAMP="$CONDA_PREFIX/lib/pkgconfig/libavcodec.pc"

step() { printf '\n\033[1;36m==> %s\033[0m\n' "$*"; }

if [[ -f "$PKG_STAMP" && -z "${FORCE:-}" ]]; then
    step "FFmpeg already installed in \$CONDA_PREFIX (set FORCE=1 to rebuild)"
    exit 0
fi

step "Building FFmpeg $FFMPEG_VERSION into $CONDA_PREFIX"

if [[ ! -d "$FFMPEG_SRC/.git" ]]; then
    rm -rf "$FFMPEG_SRC"
    git clone --depth 1 --branch "$FFMPEG_VERSION" \
        https://git.ffmpeg.org/ffmpeg.git "$FFMPEG_SRC"
fi

# Curated minimal feature set. Tweak as the renderer plugins grow new format
# requirements. The decoder list covers the common still / animated image
# and video container payloads; the audio decoders exist so media_probe can
# enumerate audio tracks in mp4/mkv files even though we never play them.
DECODERS=(
    # video
    h264 hevc av1 vp8 vp9 mpeg4 mjpeg
    # image (also used for image-sequence demuxing)
    png apng webp gif bmp tiff
    # audio (probe-only)
    aac mp3 opus vorbis flac pcm_s16le pcm_s16be
)
ENCODERS=()  # waywallen never encodes
DEMUXERS=(
    mov matroska image2 gif webp_pipe apng_pipe png_pipe jpeg_pipe
    aac mp3 ogg flac wav
)
PARSERS=(
    h264 hevc av1 vp8 vp9 mjpeg
    aac mpegaudio opus vorbis flac
    png webp
)
BSFS=(
    h264_mp4toannexb hevc_mp4toannexb
    vp9_metadata vp9_superframe
    av1_metadata
)
FILTERS=(
    scale format setpts fps null copy
    buffer buffersink abuffer abuffersink
)
PROTOCOLS=(file pipe)

CFG_ARGS=(
    --prefix="$CONDA_PREFIX"
    # FFmpeg's configure ignores $CC by default (it hardcodes cc=cc), which
    # silently picks up /usr/bin/cc — the host gcc — and bypasses the conda
    # sysroot. Pass them explicitly so the wrapper injects --sysroot.
    --cc="${CC:-clang}"
    --cxx="${CXX:-clang++}"
    --enable-shared
    --disable-static
    --disable-programs
    --disable-doc
    --disable-debug
    --enable-pic
    --disable-everything

    # External libs. Vulkan is enabled so libavcodec can offer the
    # VK_KHR_video_decode hwaccel (h264/hevc/av1) that the video plugin
    # consumes. Headers come from conda-forge vulkan-headers; the loader
    # (libvulkan.so.1) ships via vulkan-loader and is bundled into the
    # AppImage. The X / audio / v4l2 libs aren't in the conda env so
    # configure auto-disables them; we don't list them here to avoid
    # flag-name churn between FFmpeg releases.
    --enable-vulkan
    --disable-vaapi --disable-vdpau
    --disable-xlib --disable-libxcb
)
for x in "${DECODERS[@]}";  do CFG_ARGS+=( "--enable-decoder=$x" ); done
for x in "${ENCODERS[@]}";  do CFG_ARGS+=( "--enable-encoder=$x" ); done
for x in "${DEMUXERS[@]}";  do CFG_ARGS+=( "--enable-demuxer=$x" ); done
for x in "${PARSERS[@]}";   do CFG_ARGS+=( "--enable-parser=$x"  ); done
for x in "${BSFS[@]}";      do CFG_ARGS+=( "--enable-bsf=$x"     ); done
for x in "${FILTERS[@]}";   do CFG_ARGS+=( "--enable-filter=$x"  ); done
for x in "${PROTOCOLS[@]}"; do CFG_ARGS+=( "--enable-protocol=$x" ); done

# Forward the sysroot/optimization flags exported by the conda toolchain
# activation. configure splices these into every cc invocation it makes, so
# the resulting libs hit the sysroot 2.28 glibc baseline.
[[ -n "${CFLAGS:-}"  ]] && CFG_ARGS+=( --extra-cflags="$CFLAGS" )
[[ -n "${LDFLAGS:-}" ]] && CFG_ARGS+=( --extra-ldflags="$LDFLAGS" )

(
    cd "$FFMPEG_SRC"
    make distclean >/dev/null 2>&1 || true
    ./configure "${CFG_ARGS[@]}"
    make -j"$(nproc)"
    make install
)

step "FFmpeg installed; pkg-config stamp -> $PKG_STAMP"
