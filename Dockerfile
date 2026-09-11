# den-remux — a static Rust binary and a static, minimal ffmpeg, on distroless/static.
#
# Low resource use is the design goal, and most of it is decided here. Debian's ffmpeg package brings a
# few hundred MB of codecs, filters and X libraries this service never touches. This ffmpeg is built
# from source with everything disabled, then only what a session uses switched back on: the Matroska
# and MP4 demuxers, the hls/mp4 muxers, the audio decoders a release can carry, the native AAC encoder,
# and HTTP(S). Both binaries are static against musl (and OpenSSL, for ffmpeg), so the runtime image
# needs no libc and no package manager of its own. Builds for whatever arch buildx asks; the box is amd64.

# ---- build: Rust -----------------------------------------------------------
FROM rust:1-alpine3.22 AS build
RUN apk add --no-cache musl-dev
WORKDIR /src
# Cache deps: build against the manifests and a dummy main first, so a code-only change re-runs only the
# final (LTO'd) link of our crate.
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo 'fn main() {}' > src/main.rs && cargo build --release --locked && rm -rf src
COPY src ./src
RUN touch src/main.rs && cargo build --release --locked   # `strip = true` in the release profile

# ---- build: ffmpeg ---------------------------------------------------------
FROM alpine:3.22 AS ffmpeg
RUN apk add --no-cache build-base nasm pkgconf openssl-dev openssl-libs-static zlib-dev zlib-static linux-headers
# Checksummed: this source is compiled into the image and parses untrusted media, with nothing else
# checking it. Dependabot has no ecosystem for it; bump both lines together.
ARG FFMPEG_VERSION=9.0.1
ARG FFMPEG_SHA256=cf38e0e28c7e5605942c4a77755349b0145804a397af37eb1fb4c77cb237f635
RUN wget -q -O /tmp/ffmpeg.tar.xz "https://ffmpeg.org/releases/ffmpeg-${FFMPEG_VERSION}.tar.xz" \
    && echo "${FFMPEG_SHA256}  /tmp/ffmpeg.tar.xz" | sha256sum -c - \
    && tar -xJf /tmp/ffmpeg.tar.xz -C /tmp
WORKDIR /tmp/ffmpeg-${FFMPEG_VERSION}
# What each group is for:
#   demuxers   matroska, mov                  — the containers scout's releases come in
#   muxers     hls (+ mp4, which it drives)   — fMP4 HLS output
#   decoders   every audio codec a release carries: AC-3/E-AC-3, DTS (core of DTS-HD), TrueHD, FLAC,
#              Opus, AAC, MP3/MP2, Vorbis, LPCM. And h264/hevc, although video is only ever copied:
#              stream probing needs them to learn the B-frame reorder delay. Without them ffmpeg
#              guesses every copied packet's DTS ("Invalid DTS … replacing by guess" for every GOP).
#   encoder    aac                            — ffmpeg's native AAC, stereo 192k
#   parsers    for the codecs the demuxers hand over unparsed
#   filters    the audio graph -ac 2 builds: resample/downmix and format negotiation, plus the buffer
#              endpoints every graph has
#   protocols  file, and http(s) over tcp/tls (OpenSSL); --enable-version3 is what OpenSSL 3 requires
# A hardware transcode later would add --enable-vaapi, libva, and the h264_vaapi/hevc_vaapi encoders
# (README, "Hardware transcode").
RUN ./configure \
      --prefix=/opt/ffmpeg \
      --pkg-config-flags=--static --extra-ldflags=-static \
      --enable-static --disable-shared --enable-small \
      --disable-everything --disable-autodetect --disable-doc --disable-debug \
      --disable-ffplay --disable-ffprobe --disable-avdevice --disable-swscale \
      --enable-version3 --enable-openssl --enable-zlib \
      --enable-protocol=file,http,https,tcp,tls \
      --enable-demuxer=matroska,mov \
      --enable-muxer=hls,mp4 \
      --enable-decoder=h264,hevc,aac,ac3,eac3,dca,truehd,mlp,flac,opus,mp3,mp2,vorbis,pcm_s16le,pcm_s24le,pcm_s32le,pcm_f32le,pcm_s16be,pcm_s24be \
      --enable-encoder=aac \
      --enable-parser=h264,hevc,aac,ac3,dca,mlp,flac,opus,mpegaudio,vorbis \
      --enable-filter=aresample,aformat,anull,atrim,abuffer,abuffersink,null,trim,buffer,buffersink,format \
      --enable-swresample \
    && make -j"$(nproc)" && make install \
    && mkdir /cache

# ---- runtime ---------------------------------------------------------------
# distroless/static: CA certificates (ffmpeg verifies the debrid's TLS against them), tzdata and a
# passwd entry for nonroot (65532, the uid every den addon image uses) — and nothing else. No shell.
FROM gcr.io/distroless/static-debian12:nonroot
COPY --from=ffmpeg /opt/ffmpeg/bin/ffmpeg /usr/local/bin/ffmpeg
COPY --from=build /src/target/release/den-remux /usr/local/bin/den-remux
# Created owned by nonroot, so a fresh volume mounted over it is writable too.
COPY --from=ffmpeg --chown=65532:65532 /cache /cache

ENV PORT=8095 \
    SCRATCH_DIR=/cache \
    FFMPEG_PATH=/usr/local/bin/ffmpeg
VOLUME ["/cache"]
EXPOSE 8095

# No HEALTHCHECK, deliberately: a periodic probe keeps an idle box awake. den-update checks /health.
USER 65532:65532
ENTRYPOINT ["den-remux"]
