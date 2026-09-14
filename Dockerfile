# den-remux — a static Rust binary and a minimal ffmpeg, on Alpine.
#
# Low resource use is the design goal, and most of it is decided here. Debian's ffmpeg package brings a
# few hundred MB of codecs, filters and X libraries this service never touches. This ffmpeg is built
# from source with everything disabled, then only what a session uses switched back on: the Matroska
# and MP4 demuxers, the hls/mp4 muxers, the audio decoders a release can carry, the native AAC encoder,
# HTTP(S), and the VAAPI pieces of a GPU transcode. VAAPI is why this is Alpine rather than
# distroless/static: libva loads the GPU's driver at run time, which a fully static binary cannot, so
# ffmpeg links OpenSSL, zlib and libva dynamically and the runtime carries them plus Intel's driver.
# Idle it runs nothing either way; the driver is only loaded by a transcode.

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
RUN apk add --no-cache build-base nasm pkgconf openssl-dev zlib-dev libva-dev linux-headers
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
#              Opus, AAC, MP3/MP2, Vorbis, LPCM. And h264/hevc: a copy needs them for stream probing
#              (to learn the B-frame reorder delay — without them ffmpeg guesses every copied packet's
#              DTS), and a transcode decodes through them with the VAAPI hwaccels below.
#   encoders   aac (ffmpeg's native, stereo 192k); h264_vaapi for a transcode
#   parsers    for the codecs the demuxers hand over unparsed. av1: Matroska asks for AV1's headers parsed, and
#              the parser reads the pixel format and colours from the sequence header. A copy of AV1 needs no
#              decoder: its packets are never reordered, so there is no delay to learn by probing
#   filters    the audio graph -ac 2 builds (resample/downmix, format negotiation, the buffer
#              endpoints every graph has); scale_vaapi and tonemap_vaapi for a transcode
#   bsfs       dovi_rpu and filter_units: a copied Dolby Vision stream loses its RPU and enhancement layer
#              and its configuration record, leaving the HDR10 base layer a browser can play
#   protocols  file, and http(s) over tcp/tls (OpenSSL); --enable-version3 is what OpenSSL 3 requires
# ffmpeg's own libraries are linked in statically; OpenSSL, zlib and libva are the system's.
RUN ./configure \
      --prefix=/opt/ffmpeg \
      --enable-static --disable-shared --enable-small \
      --disable-everything --disable-autodetect --disable-doc --disable-debug \
      --disable-ffplay --disable-avdevice --disable-swscale \
      --enable-version3 --enable-openssl --enable-zlib --enable-vaapi \
      --enable-protocol=file,http,https,tcp,tls \
      --enable-demuxer=matroska,mov \
      --enable-muxer=hls,mp4 \
      --enable-decoder=h264,hevc,aac,ac3,eac3,dca,truehd,mlp,flac,opus,mp3,mp2,vorbis,pcm_s16le,pcm_s24le,pcm_s32le,pcm_f32le,pcm_s16be,pcm_s24be \
      --enable-hwaccel=h264_vaapi,hevc_vaapi \
      --enable-encoder=aac,h264_vaapi \
      --enable-parser=av1,h264,hevc,aac,ac3,dca,mlp,flac,opus,mpegaudio,vorbis \
      --enable-filter=aresample,aformat,anull,atrim,abuffer,abuffersink,null,trim,buffer,buffersink,format,scale_vaapi,tonemap_vaapi \
      --enable-bsf=dovi_rpu,filter_units \
      --enable-swresample \
    && make -j"$(nproc)" && make install

# ---- test: the #[ignore]d end-to-end tests against this image's own ffmpeg ---------------
# `docker build --target test .` (CI runs it; the default build skips it). The segment alignment depends
# on exactly how ffmpeg seeks, so these tests only count with the ffmpeg the image ships. ffprobe is built
# for them alone; the runtime image does not carry it. No GPU here: the transcode's flags are unit-tested.
FROM build AS test
RUN apk add --no-cache libssl3 zlib libva
COPY --from=ffmpeg /opt/ffmpeg/bin/ffmpeg /opt/ffmpeg/bin/ffprobe /usr/local/bin/
COPY testdata ./testdata
RUN cargo test --locked -- --include-ignored

# ---- runtime ---------------------------------------------------------------
# The libraries ffmpeg links, CA certificates (ffmpeg verifies the debrid's TLS against them), and on
# amd64 — the box, with its UHD 630 — Intel's VAAPI driver. nonroot is 65532, the uid every den addon
# image uses.
FROM alpine:3.22
RUN apk add --no-cache ca-certificates libssl3 zlib libva \
    && if [ "$(apk --print-arch)" = x86_64 ]; then apk add --no-cache intel-media-driver; fi \
    && adduser -D -H -u 65532 -s /sbin/nologin nonroot \
    && install -d -o 65532 -g 65532 /cache
COPY --from=ffmpeg /opt/ffmpeg/bin/ffmpeg /usr/local/bin/ffmpeg
COPY --from=build /src/target/release/den-remux /usr/local/bin/den-remux

# LIBVA_DRIVER_NAME: the UHD 630's driver, named, so libva does not probe for others.
ENV PORT=8095 \
    SCRATCH_DIR=/cache \
    FFMPEG_PATH=/usr/local/bin/ffmpeg \
    LIBVA_DRIVER_NAME=iHD
VOLUME ["/cache"]
EXPOSE 8095

# No HEALTHCHECK, deliberately: a periodic probe keeps an idle box awake. den-update checks /health.
USER 65532:65532
ENTRYPOINT ["den-remux"]
