#!/usr/bin/env bash
# Build the FFmpeg libraries FramePlayerX bundles: LGPL, decode-only, no
# network protocols (all I/O goes through Rust), with dav1d for software AV1,
# zlib for compressed Matroska tracks (mkvmerge compresses PGS subtitles),
# the V4L2 memory-to-memory decoders for Qualcomm hardware decode, and one
# filter, bwdif, to deinterlace.
#
# Usage: tools/build-ffmpeg.sh [aarch64|x86_64]...   (default: both)
# Output: third_party/ffmpeg/<arch>/{include,lib}
#
# aarch64 is cross-compiled with zig against glibc 2.28, like the app.
# x86_64 is built with the host compiler for development and tests only.
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
FFMPEG_VER=7.1.1
DAV1D_VER=1.5.3
ZLIB_VER=1.3.1
src="$root/third_party/src"
mkdir -p "$src"

# The upstream release tarballs (ffmpeg.org, VideoLAN), which Ubuntu's
# archive serves unchanged, and zlib's from its GitHub release (zlib.net
# no longer serves the same file). Nothing is extracted, cached or
# downloaded, unless it matches.
FFMPEG_SHA256=733984395e0dbbe5c046abda2dc49a5544e7e0e1e2366bba849222ae9e3a03b1
DAV1D_SHA256=732010aa5ef461fa93355ed2c6c5fedb48ddc4b74e697eaabe8907eaeb943011
ZLIB_SHA256=9a93b2b7dfdac77ceba5a558a580e74667dd6fede4585b91eefb60f03b72df23

fetch() { # url dest sha256
  if [ -f "$2" ] && echo "$3  $2" | sha256sum -c --status; then return; fi
  rm -f "$2"
  curl -fsSL --retry 4 "$1" -o "$2.part" || { echo "download failed: $1" >&2; exit 1; }
  echo "$3  $2.part" | sha256sum -c --status || {
    rm -f "$2.part"; echo "checksum mismatch: $1" >&2; exit 1; }
  mv -f "$2.part" "$2"
}
fetch "https://archive.ubuntu.com/ubuntu/pool/universe/f/ffmpeg/ffmpeg_${FFMPEG_VER}.orig.tar.xz" "$src/ffmpeg-$FFMPEG_VER.tar.xz" "$FFMPEG_SHA256"
fetch "https://archive.ubuntu.com/ubuntu/pool/main/d/dav1d/dav1d_${DAV1D_VER}.orig.tar.xz" "$src/dav1d-$DAV1D_VER.tar.xz" "$DAV1D_SHA256"
fetch "https://github.com/madler/zlib/releases/download/v${ZLIB_VER}/zlib-${ZLIB_VER}.tar.gz" "$src/zlib-$ZLIB_VER.tar.gz" "$ZLIB_SHA256"

# zig from PATH, else the pip package (tools/requirements.txt).
if command -v zig >/dev/null; then zig="$(command -v zig)"; else zig="python3 -m ziglang"; fi

zig_wrappers() { # dir
  mkdir -p "$1"
  # zig rejects a glibc-versioned target for preprocess-only runs (-E), used
  # by meson to read compiler defines and by FFmpeg's configure for header
  # checks. Preprocessing does not link anything, so drop the version there;
  # compiles and links target glibc 2.28 (tools/package.sh verifies).
  local tool
  for tool in cc c++; do
    cat > "$1/${tool/c++/cxx}" <<WRAP
#!/bin/sh
target=aarch64-linux-gnu.2.28
for a in "\$@"; do [ "\$a" = -E ] && target=aarch64-linux-gnu; done
exec $zig $tool -target "\$target" "\$@"
WRAP
  done
  printf '#!/bin/sh\nexec %s ar "$@"\n' "$zig" > "$1/ar"
  printf '#!/bin/sh\nexec %s ranlib "$@"\n' "$zig" > "$1/ranlib"
  printf '#!/bin/sh\nexec %s nm "$@"\n' "$zig" > "$1/nm"
  chmod +x "$1"/*
  "$1/cc" --version >/dev/null || { echo "zig does not work: install it or the tools in tools/requirements.txt" >&2; exit 1; }
}

build_arch() {
  local arch="$1" out="$root/third_party/ffmpeg/$1" work="$root/third_party/build/$1"
  rm -rf "$work"; mkdir -p "$work" "$out"
  tar -xf "$src/dav1d-$DAV1D_VER.tar.xz" -C "$work"
  tar -xf "$src/ffmpeg-$FFMPEG_VER.tar.xz" -C "$work"
  tar -xf "$src/zlib-$ZLIB_VER.tar.gz" -C "$work"
  local dav1d_dir; dav1d_dir="$(ls -d "$work"/dav1d-*/)"
  local ff_dir; ff_dir="$(ls -d "$work"/ffmpeg-*/)"
  local zlib_dir; zlib_dir="$(ls -d "$work"/zlib-*/)"

  local cc=cc ar=ar ranlib=ranlib cross_args=() meson_cross=()
  if [ "$arch" = aarch64 ]; then
    zig_wrappers "$work/zig"
    cc="$work/zig/cc"; ar="$work/zig/ar"; ranlib="$work/zig/ranlib"
    cat > "$work/cross.ini" <<INI
[binaries]
c = '$work/zig/cc'
cpp = '$work/zig/cxx'
ar = '$work/zig/ar'
strip = 'true'
[host_machine]
system = 'linux'
cpu_family = 'aarch64'
cpu = 'aarch64'
endian = 'little'
INI
    meson_cross=(--cross-file "$work/cross.ini")
    cross_args=(--enable-cross-compile --arch=aarch64 --target-os=linux
                --cc="$cc" --cxx="$work/zig/cxx" --ar="$ar" --ranlib="$ranlib"
                --nm="$work/zig/nm" --strip=true --host-cc=cc)
  fi

  echo "== dav1d ($arch)"
  local x86asm=()
  [ "$arch" = x86_64 ] && ! command -v nasm >/dev/null && x86asm=(-Denable_asm=false)
  meson setup "$work/dav1d-build" "$dav1d_dir" "${meson_cross[@]}" \
    --prefix="$work/dav1d-prefix" --libdir=lib --buildtype=release \
    --default-library=static -Db_staticpic=true \
    -Denable_tools=false -Denable_tests=false -Denable_examples=false "${x86asm[@]}" \
    >"$work/dav1d-setup.log" 2>&1 || { tail -40 "$work/dav1d-setup.log"; tail -40 "$work/dav1d-build/meson-logs/meson-log.txt" 2>/dev/null; exit 1; }
  ninja -C "$work/dav1d-build" install >"$work/dav1d-build.log" 2>&1 || { tail -40 "$work/dav1d-build.log"; exit 1; }

  echo "== zlib ($arch)"
  (cd "$zlib_dir" && CC="$cc" AR="$ar" RANLIB="$ranlib" CFLAGS="-O2 -fPIC" \
    ./configure --static --prefix="$work/zlib-prefix" >"$work/zlib-configure.log" 2>&1 &&
    make -j"$(nproc)" install >"$work/zlib-build.log" 2>&1) ||
    { tail -40 "$work/zlib-configure.log" "$work/zlib-build.log"; exit 1; }

  echo "== ffmpeg ($arch)"
  local asm=()
  [ "$arch" = x86_64 ] && ! command -v nasm >/dev/null && asm=(--disable-x86asm)
  (cd "$ff_dir" && PKG_CONFIG_LIBDIR="$work/dav1d-prefix/lib/pkgconfig:$work/zlib-prefix/lib/pkgconfig" ./configure \
    --prefix="$out" "${cross_args[@]}" "${asm[@]}" \
    --pkg-config=pkg-config --pkg-config-flags=--static \
    --enable-shared --disable-static --enable-pic \
    --disable-programs --disable-doc --disable-debug \
    --disable-autodetect --disable-network --disable-avdevice --disable-postproc \
    --disable-everything \
    --enable-libdav1d --enable-zlib --enable-v4l2-m2m \
    --enable-swscale --enable-swresample \
    --enable-avfilter --enable-filter=buffer,buffersink,bwdif \
    --enable-protocol=file \
    --enable-demuxer=mov,matroska,mpegts,avi,flv,ogg,mp3,aac,wav,flac,srt,ass,webvtt,hls,image2,mjpeg \
    --enable-decoder=h264,hevc,vp8,vp9,av1,libdav1d,mpeg4,mpeg2video,vc1,wmv3,prores,mjpeg,png \
    --enable-decoder=h264_v4l2m2m,hevc_v4l2m2m,vp8_v4l2m2m,vp9_v4l2m2m,mpeg4_v4l2m2m \
    --enable-decoder=aac,aac_latm,ac3,eac3,mp3,mp3float,mp2,opus,vorbis,flac,alac,dca,truehd \
    --enable-decoder=pcm_s16le,pcm_s24le,pcm_s32le,pcm_f32le,pcm_s16be,pcm_s24be,pcm_bluray,pcm_dvd \
    --enable-decoder=srt,subrip,ass,ssa,webvtt,mov_text,pgssub,dvdsub,dvbsub \
    --enable-parser=h264,hevc,vp8,vp9,av1,aac,aac_latm,ac3,mpegaudio,opus,vorbis,flac,mpeg4video,mpegvideo,vc1,dca,mlp,mjpeg,png \
    --enable-bsf=h264_mp4toannexb,hevc_mp4toannexb,vp9_superframe_split,extract_extradata,null \
    --extra-ldflags="-Wl,-rpath,\\\$\$ORIGIN" \
    > "$work/configure.log" 2>&1) || { tail -30 "$work/configure.log"; tail -40 "$ff_dir/ffbuild/config.log"; exit 1; }
  # zig's glibc headers have no <sys/sysctl.h> (removed from glibc 2.32), but
  # configure's link test still finds the symbol; FFmpeg never needs it on Linux.
  sed -i 's/^#define HAVE_SYSCTL 1$/#define HAVE_SYSCTL 0/' "$ff_dir/config.h"
  grep -q '^#define CONFIG_ZLIB 1$' "$ff_dir/config.h" || { echo "FFmpeg configured without zlib" >&2; exit 1; }
  grep -q '^#define CONFIG_BWDIF_FILTER 1$' "$ff_dir/config_components.h" || { echo "FFmpeg configured without bwdif" >&2; exit 1; }
  make -C "$ff_dir" -j"$(nproc)" >"$work/make.log" 2>&1 || { tail -40 "$work/make.log"; exit 1; }
  make -C "$ff_dir" install >/dev/null
  rm -rf "$out/share" "$out/lib/pkgconfig"
  grep -E "^(libdav1d|v4l2_m2m|hevc_v4l2m2m_decoder)" "$ff_dir/ffbuild/config.mak" | head -5 || true
  ls -la "$out/lib"
}

archs=("$@"); [ ${#archs[@]} -eq 0 ] && archs=(x86_64 aarch64)
for a in "${archs[@]}"; do build_arch "$a"; done
echo "FFmpeg $FFMPEG_VER ready in third_party/ffmpeg/"
