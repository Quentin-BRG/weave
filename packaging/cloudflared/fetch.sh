#!/usr/bin/env bash
# Download one pinned cloudflared release asset, verify its SHA-256, and place
# the executable where the caller asked for it.
#
#     packaging/cloudflared/fetch.sh <asset> <destination>
#
#     asset        a file name from packaging/cloudflared/SHA256SUMS,
#                  e.g. cloudflared-linux-amd64 or cloudflared-darwin-arm64.tgz
#     destination  where to write the resulting executable
#
# `.tgz` assets (macOS) contain a single `cloudflared` member, which is what
# ends up at the destination. Everything else is the executable already.
#
# Runs on all three release runners: Linux, macOS and Git Bash on Windows.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=/dev/null
. "$here/pinned.env"

asset="${1:?usage: fetch.sh <asset> <destination>}"
destination="${2:?usage: fetch.sh <asset> <destination>}"

expected="$(grep -E "[[:space:]]\*?${asset}\$" "$here/SHA256SUMS" | awk '{print $1}' | head -n1)"
if [ -z "$expected" ]; then
  echo "fetch.sh: $asset is not a pinned cloudflared asset" >&2
  exit 1
fi

sha256_of() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | awk '{print $1}'
  else
    # macOS runners have no sha256sum.
    shasum -a 256 "$1" | awk '{print $1}'
  fi
}

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

url="$CLOUDFLARED_BASE_URL/$CLOUDFLARED_VERSION/$asset"
echo "fetch.sh: downloading $url"
curl --fail --location --silent --show-error --retry 3 --retry-delay 5 \
  --max-time 600 --output "$work/$asset" "$url"

actual="$(sha256_of "$work/$asset")"
if [ "$actual" != "$expected" ]; then
  echo "fetch.sh: checksum mismatch for $asset" >&2
  echo "  expected $expected" >&2
  echo "  actual   $actual" >&2
  exit 1
fi
echo "fetch.sh: sha256 ok ($expected)"

mkdir -p "$(dirname "$destination")"
case "$asset" in
  *.tgz)
    tar -xzf "$work/$asset" -C "$work" cloudflared
    mv "$work/cloudflared" "$destination"
    ;;
  *)
    mv "$work/$asset" "$destination"
    ;;
esac
chmod +x "$destination" 2>/dev/null || true

echo "fetch.sh: cloudflared $CLOUDFLARED_VERSION -> $destination"
