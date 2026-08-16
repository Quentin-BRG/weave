# Packaging

Everything needed to turn `cargo build --release` into the four assets a release
publishes. `.github/workflows/release.yml` drives all of it; each script here
also runs standalone on the matching operating system.

| Asset | Built by | Contents |
| --- | --- | --- |
| `WeaveSetup-x64.exe` | `windows/build.ps1` → `windows/weave.iss` | Inno Setup 6, per-user, unsigned |
| `Weave-macos-universal.pkg` | `macos/build-pkg.sh` | `pkgbuild` + `productbuild`, unsigned, **not notarized** |
| `weave-linux-x64.deb` | `linux/build-deb.sh` | `dpkg-deb`, x86_64 Debian/Ubuntu |
| `weave-linux-x64.tar.gz` | `linux/build-tarball.sh` | Relocatable tree with `install.sh` |

`SHA256SUMS` over all four is computed in the release job, after every package
exists.

## No signing credentials are involved

No script, workflow step or installer definition references a code-signing
certificate, an Apple Developer ID, a notarization credential, or any secret
beyond the automatic `GITHUB_TOKEN` used to create the release. The Windows
installer is unsigned and the macOS package is unsigned and not notarized; both
facts are stated in the README, in the macOS installer's own welcome screen, and
in the release notes footer.

Adding signing later touches exactly two places: an `ISCC /S` sign tool for
Windows, and `--sign`/`notarytool` for macOS. Nothing else in this directory
assumes the current unsigned state.

## The bundled cloudflared

```
cloudflared/pinned.env       CLOUDFLARED_VERSION and the download base URL
cloudflared/SHA256SUMS       one digest per platform asset
cloudflared/fetch.sh         download, verify, extract, chmod
cloudflared/licenses/        Apache-2.0 licence text and Weave's NOTICE
```

`fetch.sh <asset> <destination>` downloads one asset from the pinned cloudflared
release, checks it against `SHA256SUMS`, unpacks it if it is a `.tgz`, and writes
an executable to `<destination>`. **A checksum mismatch aborts the build**, so a
release can never bundle a binary nobody pinned.

No cloudflared binary is committed to this repository. Cloudflare publishes no
checksum file of its own, so the digests in `SHA256SUMS` were computed once from
the official release assets and committed; they are the pin.

Cloudflare also publishes **no universal macOS build** — only
`cloudflared-darwin-amd64.tgz` and `cloudflared-darwin-arm64.tgz` — so the macOS
package installs both, named `cloudflared-aarch64` and `cloudflared-x86_64`, and
`src/install.rs` selects by `std::env::consts::ARCH` at run time. `build-pkg.sh`
verifies each slice with `lipo -archs` rather than trusting the filename.

`packaging/cloudflared/pinned.env` and `weave::install::CLOUDFLARED_VERSION` must
agree; `tests/packaging.rs` fails if they drift apart.

### Upgrading the pin

1. Edit `CLOUDFLARED_VERSION` in `cloudflared/pinned.env` **and**
   `CLOUDFLARED_VERSION` in `src/install.rs`.
2. Recompute the four digests from the new release and replace
   `cloudflared/SHA256SUMS`:
   ```bash
   for a in cloudflared-windows-amd64.exe cloudflared-darwin-amd64.tgz \
            cloudflared-darwin-arm64.tgz cloudflared-linux-amd64; do
     curl -sSLO "https://github.com/cloudflare/cloudflared/releases/download/<version>/$a"
   done
   sha256sum cloudflared-* > packaging/cloudflared/SHA256SUMS
   ```
3. Refresh `cloudflared/licenses/cloudflared/LICENSE` from the same tag and
   update the version named in `NOTICE`.
4. `cargo test --test packaging`.

## The bundle manifest

`bundle-manifest.sh <package-id> <destination>` writes the `weave-bundle.json`
that every package installs beside its cloudflared:

```json
{
  "weave_version": "1.0.0",
  "cloudflared_version": "2026.8.2",
  "package": "windows-x64-inno",
  "built_at": "2026-08-16T00:00:00Z"
}
```

It is what tells a running `weave` that it is an installed package rather than a
`cargo build`. That distinction is load-bearing: in a package, a missing or
mismatched bundled cloudflared is a **failure** and the installer refuses to
report success; in a source build it is only a warning, because `weave host
--lan` works perfectly well without one. It honours `SOURCE_DATE_EPOCH`.

## Installation layouts

The three layouts `src/install.rs` resolves, and nothing else:

```
Windows   %LOCALAPPDATA%\Programs\Weave\weave.exe
                                       cloudflared.exe
                                       weave-bundle.json
                                       weave.ico
                                       licenses\cloudflared\{LICENSE,NOTICE}

macOS     /usr/local/bin/weave                       (universal)
          /usr/local/libexec/weave/cloudflared-aarch64
                                   cloudflared-x86_64
                                   weave-bundle.json
                                   licenses/cloudflared/{LICENSE,NOTICE}
          /usr/local/share/doc/weave/{LICENSE,README.md}

Linux     /usr/bin/weave
          /usr/lib/weave/cloudflared
                         weave-bundle.json
          /usr/share/doc/weave/{copyright,changelog.Debian.gz,README.md,
                                third-party/cloudflared/{LICENSE,NOTICE}}
          /usr/share/icons/hicolor/<size>/apps/weave.png
```

Discovery is anchored on the running executable — `<exe dir>`,
`<exe dir>/../lib/weave`, `<exe dir>/../libexec/weave`, plus the absolute system
locations — so the portable tarball works from any prefix and nothing depends on
`PATH`. `WEAVE_CLOUDFLARED` overrides everything for development; if it points at
something unusable, discovery reports that rather than silently falling back.

## The installer self-check

Every installer runs `weave doctor --install` and fails loudly if it does not
pass. That check deliberately touches no repository and no user-level Weave state,
because it runs as **root** from the `.deb` postinst and the `.pkg` postinstall
script; a user-specific repository diagnostic there would be both meaningless and
wrong. The Windows installer runs it as the installing user and writes
`install-check.log` beside `weave.exe`.

## Icons

`assets/icons/` is generated from `docs/assets/weave-icon.svg`, the canonical
mark — nothing is redrawn. `packaging/icons/generate-windows-wizard.ps1`
composites the wizard bitmaps Inno Setup needs from the same rasterisations. See
`assets/icons/README.md`.

## Building by hand

```bash
# Windows (PowerShell, needs Git Bash for the shared scripts)
cargo build --release
pwsh packaging/windows/build.ps1 -Binary target/release/weave.exe -OutputDir dist

# macOS
cargo build --release --target aarch64-apple-darwin
cargo build --release --target x86_64-apple-darwin
lipo -create target/{aarch64,x86_64}-apple-darwin/release/weave -output dist/weave-universal
packaging/macos/build-pkg.sh dist/weave-universal dist

# Linux
cargo build --release
packaging/linux/build-deb.sh target/release/weave dist
packaging/linux/build-tarball.sh target/release/weave dist
```

## Releasing

```bash
# 1. Bump `version` in Cargo.toml, commit, and make sure Cargo.lock followed.
# 2. Tag it. The tag must equal the Cargo version with a leading `v`.
git tag -a v1.0.0 -m "Weave 1.0.0"
git push origin v1.0.0
```

The workflow validates the tag against `Cargo.toml`, runs `cargo fmt --check`,
`cargo clippy --all-targets -- -D warnings` and the full offline test suite, then
builds all three packages on native runners. The release is created only after
every package job succeeds. A tag containing a hyphen (`v1.0.0-rc.1`) is
published as a GitHub prerelease.

`workflow_dispatch` with a tag name builds everything and publishes nothing,
which is the way to rehearse a release.
