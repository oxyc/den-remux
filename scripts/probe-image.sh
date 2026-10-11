#!/usr/bin/env bash
# Start an image and prove it is the release it claims to be, before anything signs or promotes it.
#
#   scripts/probe-image.sh ghcr.io/oxyc/den-remux@sha256:<digest> 0.48.0
#
# Unconfigured, den-remux still starts, answers /health and checks its ffmpeg. The startup line must name the
# expected release and `ffmpeg=ok`, and /health must not report a broken ffmpeg or scratch directory.
set -euo pipefail

image="${1:?usage: probe-image.sh <image@digest> <version>}"
version="${2:?usage: probe-image.sh <image@digest> <version>}"

docker pull --quiet "$image" >/dev/null
id=$(docker run --rm -d -p 127.0.0.1:18095:8095 "$image")
trap 'docker rm -f "$id" >/dev/null 2>&1 || true' EXIT

body=""
for _ in $(seq 1 30); do
    body=$(curl -fsS http://127.0.0.1:18095/health 2>/dev/null) && break
    sleep 1
done
[ -n "$body" ] || { echo "::error::no /health answer within 30 s"; docker logs "$id" >&2; exit 1; }

line=$(docker logs "$id" 2>&1 | grep -m1 ' listening on ' || true)
echo "startup: $line"
echo "health: $body"
case "$line" in
    "den-remux $version listening on "*) ;;
    *) echo "::error::startup line does not name $version"; exit 1 ;;
esac
case "$line" in
    *" ffmpeg=ok "*) ;;
    *) echo "::error::the image's ffmpeg failed its startup check"; exit 1 ;;
esac
reason=$(jq -r '.reason // empty' <<<"$body")
case "$reason" in
    ffmpeg_unavailable | scratch_unwritable) echo "::error::/health reports $reason"; exit 1 ;;
esac
