#!/usr/bin/env bash
# Cut a release without rebuilding or retesting it.
#
#   scripts/release.sh 0.48.0
#
# Main CI has already tested, built, probed and scanned an immutable candidate image for its exact SHA. This
# changes only VERSION, pushes that commit to main, waits for its exact-main CI, and only then tags vX.Y.Z; the
# tag's docker-publish run verifies and promotes that candidate's digest. A failed main run creates no tag, so
# there is never a tag that built nothing — the failure den-update cannot tell from "nothing to deploy".
set -euo pipefail

version="${1:-}"
[ -n "$version" ] || { echo "usage: scripts/release.sh <version>   e.g. 0.48.0" >&2; exit 1; }
case "$version" in
    v*) echo "error: give the version without the leading v ($version -> ${version#v})" >&2; exit 1 ;;
esac
[[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] \
    || { echo "error: '$version' is not X.Y.Z" >&2; exit 1; }

cd "$(dirname "$0")/.."
# From the remote, so nothing here names a repo.
slug="$(git remote get-url origin | sed -e 's#.*[:/]\([^/]*/[^/]*\)$#\1#' -e 's/\.git$//')"
name="${slug#*/}"
[ -z "$(git status --porcelain)" ] || { echo "error: working tree has uncommitted changes" >&2; exit 1; }
git fetch --quiet origin
main="$(git symbolic-ref --quiet --short refs/remotes/origin/HEAD | sed 's#^origin/##' || echo main)"
# Several sessions work in these repos at once; releasing anything but the latest main would push a version
# commit on top of a stale tree, or be rejected halfway through.
[ "$(git rev-parse HEAD)" = "$(git rev-parse "origin/$main")" ] \
    || { echo "error: checkout is not the latest origin/$main — update it before releasing" >&2; exit 1; }
git rev-parse "v$version" >/dev/null 2>&1 && { echo "error: tag v$version already exists" >&2; exit 1; }
command -v gh >/dev/null 2>&1 || {
    echo "error: gh is required to prove exact-main CI before tagging" >&2; exit 1; }

# Cargo.toml's version stays fixed: changing it would invalidate the cached dependency layers on every release.
current="$(cat VERSION)"
if [ "$current" != "$version" ]; then
    echo "==> version $current -> $version"
    printf '%s\n' "$version" > VERSION
    git add VERSION
    git commit --quiet -m "$name $version"
    git push --quiet origin "HEAD:$main"
fi
sha="$(git rev-parse HEAD)"

echo "==> waiting for exact-main CI and its candidate image ($sha)"
status=""; conclusion=""
for _ in $(seq 1 120); do
    read -r status conclusion <<<"$(gh run list --repo "$slug" --workflow ci.yml --branch "$main" --commit "$sha" \
        --event push --limit 1 --json status,conclusion --jq '.[0] | "\(.status // "-") \(.conclusion // "-")"')"
    [ "$status" = completed ] && break
    sleep 15
done
if [ "$status" != completed ]; then
    echo "error: exact-main CI did not finish within 30 minutes; nothing was tagged" >&2
    exit 1
fi
if [ "$conclusion" != success ]; then
    echo "error: exact-main CI for $sha finished $conclusion; no release tag was created" >&2
    echo "       logs: gh run list --repo $slug --workflow ci.yml --commit $sha" >&2
    exit 1
fi

# Do not quietly release an older commit when another session merged while CI ran. Rerunning against the new head
# needs no second version commit: VERSION already carries the intended release and its own candidate will win.
git fetch --quiet origin
[ "$sha" = "$(git rev-parse "origin/$main")" ] || {
    echo "error: origin/$main moved while CI ran — update this checkout and rerun; nothing was tagged" >&2
    exit 1
}

git tag -a "v$version" -m "$name $version"
git push --quiet origin "v$version"

echo "==> promoting the tested candidate"
status=""; conclusion=""
for _ in $(seq 1 80); do
    # This tag's own run. Until it exists `.[0]` is null, which reads as not completed, so the loop waits for it.
    read -r status conclusion <<<"$(gh run list --repo "$slug" --workflow docker-publish.yml --branch "v$version" \
        --limit 1 --json status,conclusion --jq '.[0] | "\(.status // "-") \(.conclusion // "-")"')"
    [ "$status" = completed ] && break
    sleep 15
done
if [ "$status" != completed ]; then
    echo "note: promotion is still running after 20 minutes — gh run list --repo $slug"; exit 0
fi
[ "$conclusion" = success ] || {
    echo "error: v$version promotion failed ($conclusion); :latest was not moved" >&2
    echo "       logs: gh run view --repo $slug --log-failed" >&2
    exit 1
}
echo "v$version is signed and published. Deploy with:"
echo "  ssh root@pve 'incus exec den -- env TUF_ROOT=/var/lib/den/sigstore den-update $name'"
