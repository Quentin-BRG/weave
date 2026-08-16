#!/usr/bin/env bash
# Build weave-linux-x64.deb.
#
#     packaging/linux/build-deb.sh <weave-binary> <output-dir>
#
# Installed layout:
#
#     /usr/bin/weave
#     /usr/lib/weave/cloudflared
#     /usr/lib/weave/weave-bundle.json
#     /usr/share/doc/weave/{copyright,changelog.Debian.gz,README.md}
#     /usr/share/doc/weave/third-party/cloudflared/{LICENSE,NOTICE}
#     /usr/share/icons/hicolor/<size>/apps/weave.{png,svg}
#
# `weave` finds its cloudflared at /usr/bin/../lib/weave/cloudflared, which is
# why the support directory is /usr/lib/weave and not somewhere prettier.
#
# No .desktop entry: Weave is a CLI and an application launcher entry for it
# would be noise. The icons are shipped so anything that does surface Weave —
# a software centre listing the package, a terminal profile — has the real
# mark to use.
set -euo pipefail

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
binary="${1:?usage: build-deb.sh <weave-binary> <output-dir>}"
outdir="${2:?usage: build-deb.sh <weave-binary> <output-dir>}"

version="$(sed -n 's/^version = "\(.*\)"/\1/p' "$repo/Cargo.toml" | head -n1)"
# Debian versions may not contain the '-' of a prerelease tag as we use it, and
# `~` sorts *before* the release, which is exactly what a release candidate is.
deb_version="${version//-/\~}"
maintainer="${WEAVE_DEB_MAINTAINER:-Quentin-BRG <Quentin-BRG@users.noreply.github.com>}"

stage="$(mktemp -d)"
trap 'rm -rf "$stage"' EXIT
# mktemp -d gives 0700, which would be recorded as the mode of `./` — the
# filesystem root — in the archive.
chmod 0755 "$stage"

install -d -m 0755 \
  "$stage/DEBIAN" \
  "$stage/usr/bin" \
  "$stage/usr/lib/weave" \
  "$stage/usr/share/doc/weave/third-party/cloudflared"

install -m 0755 "$binary" "$stage/usr/bin/weave"

"$repo/packaging/cloudflared/fetch.sh" cloudflared-linux-amd64 "$stage/usr/lib/weave/cloudflared"
chmod 0755 "$stage/usr/lib/weave/cloudflared"
"$repo/packaging/bundle-manifest.sh" linux-x64-deb "$stage/usr/lib/weave/weave-bundle.json"
chmod 0644 "$stage/usr/lib/weave/weave-bundle.json"

install -m 0644 "$repo/packaging/cloudflared/licenses/cloudflared/LICENSE" \
  "$stage/usr/share/doc/weave/third-party/cloudflared/LICENSE"
install -m 0644 "$repo/packaging/cloudflared/licenses/cloudflared/NOTICE" \
  "$stage/usr/share/doc/weave/third-party/cloudflared/NOTICE"
install -m 0644 "$repo/README.md" "$stage/usr/share/doc/weave/README.md"

# Icons. hicolor is the freedesktop theme every desktop reads.
for size in 16x16 22x22 24x24 32x32 48x48 64x64 128x128 256x256 512x512; do
  src="$repo/assets/icons/linux/hicolor/$size/apps/weave.png"
  [ -f "$src" ] || continue
  install -d -m 0755 "$stage/usr/share/icons/hicolor/$size/apps"
  install -m 0644 "$src" "$stage/usr/share/icons/hicolor/$size/apps/weave.png"
done
install -d -m 0755 "$stage/usr/share/icons/hicolor/scalable/apps"
install -m 0644 "$repo/assets/icons/linux/hicolor/scalable/apps/weave.svg" \
  "$stage/usr/share/icons/hicolor/scalable/apps/weave.svg"

# ---------------------------------------------------------------------------
# Metadata
# ---------------------------------------------------------------------------

cat >"$stage/usr/share/doc/weave/copyright" <<'COPYRIGHT'
Format: https://www.debian.org/doc/packaging-manuals/copyright-format/1.0/
Upstream-Name: weave
Source: https://github.com/Quentin-BRG/weave

Files: *
Copyright: Weave contributors
License: MPL-2.0
 This Source Code Form is subject to the terms of the Mozilla Public License,
 v. 2.0. If a copy of the MPL was not distributed with this file, You can
 obtain one at https://mozilla.org/MPL/2.0/.

Files: usr/lib/weave/cloudflared
Copyright: 2018-2026 Cloudflare, Inc.
License: Apache-2.0
 Licensed under the Apache License, Version 2.0. The full text is installed at
 /usr/share/doc/weave/third-party/cloudflared/LICENSE, alongside the NOTICE
 describing which release is bundled and how it is used.
COPYRIGHT
chmod 0644 "$stage/usr/share/doc/weave/copyright"

cat >"$stage/changelog.Debian" <<CHANGELOG
weave ($deb_version) stable; urgency=medium

  * Weave $version. See https://github.com/Quentin-BRG/weave/releases for the
    full release notes.

 -- $maintainer  $(date -u -R)
CHANGELOG
gzip -9n <"$stage/changelog.Debian" >"$stage/usr/share/doc/weave/changelog.Debian.gz"
rm "$stage/changelog.Debian"
chmod 0644 "$stage/usr/share/doc/weave/changelog.Debian.gz"

installed_size="$(du -sk --exclude=DEBIAN "$stage" | cut -f1)"

cat >"$stage/DEBIAN/control" <<CONTROL
Package: weave
Version: $deb_version
Architecture: amd64
Maintainer: $maintainer
Installed-Size: $installed_size
Depends: git (>= 1:2.25)
Section: devel
Priority: optional
Homepage: https://github.com/Quentin-BRG/weave
Description: Real-time collaboration layer above Git
 Weave keeps several local copies of one Git repository in sync in real time
 while Git keeps its ordinary role as the durable, publishable history. One
 authoritative host coordinates; participants edit their own working trees with
 their own editors and agents.
 .
 This package bundles Cloudflare's cloudflared, which "weave host" launches to
 publish a Quick Tunnel. Nothing else needs installing: no Rust toolchain, no
 separate cloudflared.
CONTROL

# Installation self-check. dpkg runs this as root, so it must be the
# installation diagnostic and never the repository one: `--install` needs no
# Git working tree and touches nothing belonging to a user.
cat >"$stage/DEBIAN/postinst" <<'POSTINST'
#!/bin/sh
set -e

if [ "$1" = "configure" ]; then
  if ! /usr/bin/weave doctor --install >/dev/null 2>&1; then
    echo "weave: the installation self-check failed." >&2
    /usr/bin/weave doctor --install >&2 || true
    echo "weave: this package looks broken; please report it at" >&2
    echo "       https://github.com/Quentin-BRG/weave/issues" >&2
    exit 1
  fi
fi

exit 0
POSTINST
chmod 0755 "$stage/DEBIAN/postinst"

mkdir -p "$outdir"
target="$outdir/weave-linux-x64.deb"
# --root-owner-group keeps the package reproducible without fakeroot.
dpkg-deb --root-owner-group --build "$stage" "$target"

echo
dpkg-deb --info "$target"
echo "build-deb.sh: $target"
