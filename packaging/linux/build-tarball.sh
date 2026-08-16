#!/usr/bin/env bash
# Build weave-linux-x64.tar.gz, the portable artifact for anything that is not
# Debian or Ubuntu.
#
#     packaging/linux/build-tarball.sh <weave-binary> <output-dir>
#
# The archive mirrors the .deb layout relative to its own root, so `weave`
# finds its cloudflared whether you run it in place or copy the tree under a
# prefix:
#
#     weave-linux-x64/bin/weave
#     weave-linux-x64/lib/weave/cloudflared
#     weave-linux-x64/lib/weave/weave-bundle.json
#     weave-linux-x64/share/doc/weave/...
#     weave-linux-x64/install.sh
set -euo pipefail

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
binary="${1:?usage: build-tarball.sh <weave-binary> <output-dir>}"
outdir="${2:?usage: build-tarball.sh <weave-binary> <output-dir>}"

name=weave-linux-x64
stage="$(mktemp -d)"
trap 'rm -rf "$stage"' EXIT
chmod 0755 "$stage"
root="$stage/$name"

install -d -m 0755 \
  "$root/bin" \
  "$root/lib/weave" \
  "$root/share/doc/weave/third-party/cloudflared"

install -m 0755 "$binary" "$root/bin/weave"
"$repo/packaging/cloudflared/fetch.sh" cloudflared-linux-amd64 "$root/lib/weave/cloudflared"
chmod 0755 "$root/lib/weave/cloudflared"
"$repo/packaging/bundle-manifest.sh" linux-x64-tar "$root/lib/weave/weave-bundle.json"

install -m 0644 "$repo/LICENSE" "$root/share/doc/weave/LICENSE"
install -m 0644 "$repo/README.md" "$root/share/doc/weave/README.md"
install -m 0644 "$repo/packaging/cloudflared/licenses/cloudflared/LICENSE" \
  "$root/share/doc/weave/third-party/cloudflared/LICENSE"
install -m 0644 "$repo/packaging/cloudflared/licenses/cloudflared/NOTICE" \
  "$root/share/doc/weave/third-party/cloudflared/NOTICE"

cat >"$root/install.sh" <<'INSTALL'
#!/bin/sh
# Copy this tree under a prefix, keeping the layout `weave` expects.
#
#     ./install.sh                 # /usr/local, needs write access there
#     PREFIX=~/.local ./install.sh # per-user
set -eu

prefix="${PREFIX:-/usr/local}"
here="$(cd "$(dirname "$0")" && pwd)"

mkdir -p "$prefix/bin" "$prefix/lib/weave" "$prefix/share/doc/weave"
cp "$here/bin/weave" "$prefix/bin/weave"
cp -R "$here/lib/weave/." "$prefix/lib/weave/"
cp -R "$here/share/doc/weave/." "$prefix/share/doc/weave/"
chmod 755 "$prefix/bin/weave" "$prefix/lib/weave/cloudflared"

echo "Installed weave into $prefix."
if ! "$prefix/bin/weave" doctor --install; then
  echo "The installation self-check failed." >&2
  exit 1
fi
case ":$PATH:" in
  *":$prefix/bin:"*) ;;
  *) echo "Add $prefix/bin to your PATH to run \`weave\`." ;;
esac
INSTALL
chmod 0755 "$root/install.sh"

mkdir -p "$outdir"
target="$outdir/$name.tar.gz"
# root:root rather than whoever happened to build it, matching the .deb.
tar --owner=0 --group=0 --numeric-owner -czf "$target" -C "$stage" "$name"

echo "build-tarball.sh: $target"
