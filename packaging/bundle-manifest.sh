#!/usr/bin/env bash
# Write the `weave-bundle.json` a package drops beside its bundled cloudflared.
#
#     packaging/bundle-manifest.sh <package-id> <destination>
#
# `src/install.rs` looks for this file to decide whether it is running from a
# real package. `weave doctor --install` then treats a missing or mismatched
# bundle as a broken installation rather than as a development build, and
# cross-checks the versions recorded here against what actually runs.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=/dev/null
. "$here/cloudflared/pinned.env"

package="${1:?usage: bundle-manifest.sh <package-id> <destination>}"
destination="${2:?usage: bundle-manifest.sh <package-id> <destination>}"

weave_version="$(sed -n 's/^version = "\(.*\)"/\1/p' "$here/../Cargo.toml" | head -n1)"
if [ -z "$weave_version" ]; then
  echo "bundle-manifest.sh: could not read the Weave version from Cargo.toml" >&2
  exit 1
fi

# SOURCE_DATE_EPOCH keeps the manifest reproducible when the caller sets it.
if [ -n "${SOURCE_DATE_EPOCH:-}" ]; then
  built_at="$(date -u -d "@$SOURCE_DATE_EPOCH" +%Y-%m-%dT%H:%M:%SZ 2>/dev/null \
    || date -u -r "$SOURCE_DATE_EPOCH" +%Y-%m-%dT%H:%M:%SZ)"
else
  built_at="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
fi

mkdir -p "$(dirname "$destination")"
cat >"$destination" <<JSON
{
  "weave_version": "$weave_version",
  "cloudflared_version": "$CLOUDFLARED_VERSION",
  "package": "$package",
  "built_at": "$built_at"
}
JSON

echo "bundle-manifest.sh: $destination (weave $weave_version, cloudflared $CLOUDFLARED_VERSION)"
