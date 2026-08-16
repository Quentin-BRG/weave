#!/usr/bin/env bash
# Build Weave-macos-universal.pkg.
#
#     packaging/macos/build-pkg.sh <universal-weave-binary> <output-dir>
#
# Installed layout:
#
#     /usr/local/bin/weave                              (universal: arm64 + x86_64)
#     /usr/local/libexec/weave/cloudflared-aarch64
#     /usr/local/libexec/weave/cloudflared-x86_64
#     /usr/local/libexec/weave/weave-bundle.json
#     /usr/local/libexec/weave/licenses/cloudflared/{LICENSE,NOTICE}
#     /usr/local/share/doc/weave/{LICENSE,README.md}
#
# /usr/local/bin is on the default macOS PATH (it is listed in /etc/paths), so
# the package changes no shell configuration.
#
# Weave itself is one universal Mach-O. cloudflared is not: Cloudflare publishes
# `cloudflared-darwin-amd64.tgz` and `cloudflared-darwin-arm64.tgz` and no
# universal build, so both are installed and `src/install.rs` picks the one
# matching `std::env::consts::ARCH` at run time.
#
# THIS PACKAGE IS NOT SIGNED AND NOT NOTARIZED. There is no Developer ID
# involved, `--sign` is never passed, and no Apple credential is required to
# build it. Gatekeeper will therefore warn on first open; see the README.
set -euo pipefail

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
here="$repo/packaging/macos"
binary="${1:?usage: build-pkg.sh <universal-weave-binary> <output-dir>}"
outdir="${2:?usage: build-pkg.sh <universal-weave-binary> <output-dir>}"

version="$(sed -n 's/^version = "\(.*\)"/\1/p' "$repo/Cargo.toml" | head -n1)"
identifier=com.github.quentin-brg.weave

# The binary must really be universal; a thin one would install and then fail
# for half the people who downloaded it.
architectures="$(lipo -archs "$binary")"
case " $architectures " in
  *" arm64 "*) ;;
  *) echo "build-pkg.sh: $binary has no arm64 slice (found: $architectures)" >&2; exit 1 ;;
esac
case " $architectures " in
  *" x86_64 "*) ;;
  *) echo "build-pkg.sh: $binary has no x86_64 slice (found: $architectures)" >&2; exit 1 ;;
esac
echo "build-pkg.sh: weave is universal ($architectures)"

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
root="$work/root"

install -d -m 0755 \
  "$root/usr/local/bin" \
  "$root/usr/local/libexec/weave/licenses/cloudflared" \
  "$root/usr/local/share/doc/weave"

install -m 0755 "$binary" "$root/usr/local/bin/weave"

"$repo/packaging/cloudflared/fetch.sh" cloudflared-darwin-arm64.tgz \
  "$root/usr/local/libexec/weave/cloudflared-aarch64"
"$repo/packaging/cloudflared/fetch.sh" cloudflared-darwin-amd64.tgz \
  "$root/usr/local/libexec/weave/cloudflared-x86_64"
chmod 0755 "$root/usr/local/libexec/weave/cloudflared-aarch64" \
  "$root/usr/local/libexec/weave/cloudflared-x86_64"

# Each slice must be the architecture its name claims.
for arch_pair in "aarch64:arm64" "x86_64:x86_64"; do
  suffix="${arch_pair%%:*}"
  expected="${arch_pair##*:}"
  got="$(lipo -archs "$root/usr/local/libexec/weave/cloudflared-$suffix")"
  if [ "$got" != "$expected" ]; then
    echo "build-pkg.sh: cloudflared-$suffix is $got, expected $expected" >&2
    exit 1
  fi
done
echo "build-pkg.sh: cloudflared arm64 + x86_64 verified"

"$repo/packaging/bundle-manifest.sh" macos-universal-pkg \
  "$root/usr/local/libexec/weave/weave-bundle.json"

install -m 0644 "$repo/packaging/cloudflared/licenses/cloudflared/LICENSE" \
  "$root/usr/local/libexec/weave/licenses/cloudflared/LICENSE"
install -m 0644 "$repo/packaging/cloudflared/licenses/cloudflared/NOTICE" \
  "$root/usr/local/libexec/weave/licenses/cloudflared/NOTICE"
install -m 0644 "$repo/LICENSE" "$root/usr/local/share/doc/weave/LICENSE"
install -m 0644 "$repo/README.md" "$root/usr/local/share/doc/weave/README.md"

# ---------------------------------------------------------------------------
# Component package, then the distribution around it
# ---------------------------------------------------------------------------

scripts="$work/scripts"
install -d -m 0755 "$scripts"
install -m 0755 "$here/scripts/postinstall" "$scripts/postinstall"

pkgbuild \
  --root "$root" \
  --identifier "$identifier" \
  --version "$version" \
  --install-location / \
  --scripts "$scripts" \
  "$work/weave-component.pkg"

resources="$work/resources"
install -d -m 0755 "$resources"
install -m 0644 "$here/resources/welcome.html" "$resources/welcome.html"
install -m 0644 "$here/resources/conclusion.html" "$resources/conclusion.html"
install -m 0644 "$repo/LICENSE" "$resources/LICENSE.txt"
# The installer artwork is the canonical Weave mark, rasterised from the same
# vector as every other icon in assets/icons.
install -m 0644 "$repo/assets/icons/macos/weave.iconset/icon_256x256.png" \
  "$resources/background.png"

distribution="$work/distribution.xml"
sed -e "s|@VERSION@|$version|g" -e "s|@IDENTIFIER@|$identifier|g" \
  "$here/distribution.xml" >"$distribution"

mkdir -p "$outdir"
target="$outdir/Weave-macos-universal.pkg"
productbuild \
  --distribution "$distribution" \
  --package-path "$work" \
  --resources "$resources" \
  "$target"

echo
pkgutil --check-signature "$target" 2>&1 || true
echo "build-pkg.sh: $target (unsigned, not notarized)"
