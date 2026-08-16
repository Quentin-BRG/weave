---

## Install

| Platform | Download | Notes |
| --- | --- | --- |
| Windows 10/11 (x64) | `WeaveSetup-x64.exe` | Installs for the current user, no administrator rights needed. |
| macOS 11+ (Apple silicon and Intel) | `Weave-macos-universal.pkg` | Universal binary. |
| Debian/Ubuntu (x86_64) | `weave-linux-x64.deb` | `sudo apt install ./weave-linux-x64.deb` |
| Any modern Linux (x86_64) | `weave-linux-x64.tar.gz` | Portable; run `install.sh` or add `bin/` to your `PATH`. |

Every package contains Weave and a pinned copy of `cloudflared`. You do not need
Rust, Cargo, or a separate `cloudflared` install. After installing, `cd` into a
Git repository and run `weave host` or `weave join`.

## These builds are not code-signed

**Windows:** the installer is not signed with a code-signing certificate.
SmartScreen may show "Windows protected your PC" — choose **More info** →
**Run anyway**.

**macOS:** the package is **not signed with a Developer ID and not notarized**.
Gatekeeper will refuse to open it on the first double-click. Control-click the
`.pkg` → **Open**, or allow it under **System Settings → Privacy & Security**.

Verify what you downloaded against `SHA256SUMS`, which lists a SHA-256 digest
for every asset above:

```
sha256sum -c SHA256SUMS --ignore-missing      # Linux
shasum -a 256 -c SHA256SUMS --ignore-missing  # macOS
```
