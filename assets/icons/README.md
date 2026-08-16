# Application icons

Platform icons for the `weave` binary and its installers. These are build
inputs — the documentation artwork lives in [`docs/assets`](../../docs/assets).

## Layout

| Path | Platform | Notes |
| --- | --- | --- |
| `weave-icon-macos.svg` | macOS | master, Apple icon grid |
| `macos/weave.icns` | macOS | 10 elements, 16 → 1024 |
| `macos/weave.iconset/` | macOS | the PNGs `weave.icns` is built from |
| `windows/weave.ico` | Windows | 10 images, 16 → 256 |
| `windows/wizard-large.bmp` | Windows | Inno Setup `WizardImageFile`, 164 × 314 |
| `windows/wizard-small.bmp` | Windows | Inno Setup `WizardSmallImageFile`, 55 × 55 |
| `linux/hicolor/<size>/apps/weave.png` | Linux | 16, 22, 24, 32, 48, 64, 128, 256, 512 |
| `linux/hicolor/scalable/apps/weave.svg` | Linux | vector fallback |

## Why two shapes

Windows and Linux draw an app icon exactly as supplied — neither OS masks it —
so those use a **full-bleed** rounded square: the background reaches all four
edges, corner radius 20.7 % of the side.

macOS is different. Icons must sit on Apple's grid or they look oversized next
to everything else in the Dock, so `weave-icon-macos.svg` insets an **824 pt
shape on a 1024 pt canvas** (100 pt of transparent margin) with a 185.4 pt
corner radius. That corner is *continuous* (a superellipse quadrant, exponent
4), not the circular arc that SVG `rx` produces — curvature falls to zero at
the tangent points, which is what makes an Apple corner read as smooth.

Neither file is a masked target: do **not** hand these to iOS, Android or a
PWA `purpose: "maskable"` slot. Those platforms apply their own mask and would
round the already-rounded corner a second time. They need a square, full-bleed
source with a safe zone instead.

## Geometry

Both shapes are built from the same vector as
[`docs/assets/weave-icon.svg`](../../docs/assets/weave-icon.svg); the `W` path
data is byte-identical to the original logo in every one of them. The only
transforms applied are a translation and, for macOS, a uniform scale.

| | full-bleed | Apple grid |
| --- | --- | --- |
| canvas | 512 | 1024 |
| shape | 512 (full bleed) | 824, centred |
| corner radius | 106 (0.207) | 185.4 (0.225) |
| corner curve | circular arc | superellipse, n = 4 |
| `W` width | 0.675 of the shape | 0.675 of the shape |

Colours are the brand palette throughout: background `#181A1D`, strands
`#FFFFFF` and `#0044FE`.

## Container details

`weave.ico` stores 16–128 as 32-bit BMP/DIB and 256 as PNG. That split is
deliberate: `System.Drawing`/GDI+ cannot decode PNG-compressed ICO entries, so
keeping the everyday sizes as BMP means every Windows toolchain reads them,
while the 256 stays PNG to avoid a 270 KB uncompressed frame. All ten frames
decode under WIC, which is what Explorer and the installers actually use.

`weave.icns` uses PNG payloads for all ten elements, the same mapping Apple's
`iconutil` emits from an `.iconset`.

## Regenerating

Each PNG is rasterised from the vector at its target size — nothing is
downsampled from a larger bitmap, which is what keeps the small sizes crisp.
On a machine with the usual tooling:

```bash
# macOS: iconset -> icns
iconutil -c icns assets/icons/macos/weave.iconset -o assets/icons/macos/weave.icns
```

```bash
# any OS: re-rasterise one size from the vector
rsvg-convert -w 256 -h 256 docs/assets/weave-icon.svg -o out.png
```

## Wiring it up

Weave is a CLI, so these appear where a CLI legitimately has a face: the
installers and the package metadata. No `.app` bundle and no `.desktop` entry
exist, and none should be invented merely to hang an icon on.

- **Windows** — `packaging/windows/weave.iss` uses `windows/weave.ico` as
  `SetupIconFile` (the installer's own icon) and as `UninstallDisplayIcon`, so
  Weave shows the mark in **Installed apps**. The file is also installed
  alongside `weave.exe` for the uninstall entry to reference.
  `packaging/icons/generate-windows-wizard.ps1` composites the two wizard
  bitmaps Inno Setup wants — 164×314 and 55×55, BMP only — from the 128 and 64
  `hicolor` PNGs on the brand background, and writes them to
  `windows/wizard-large.bmp` and `windows/wizard-small.bmp`. Rerun it after
  changing the mark; it redraws nothing, it only places and composites.
- **macOS** — `packaging/macos/build-pkg.sh` copies
  `macos/weave.iconset/icon_256x256.png` into the `productbuild` resources as
  the installer background, which is the only branding surface an installer
  package has. `weave.icns` stays for a future `.app`, if Weave ever has one.
- **Linux** — `packaging/linux/build-deb.sh` installs `linux/hicolor/` into
  `/usr/share/icons/hicolor/`, so desktop tooling and package browsers that
  look up an icon by package name find one. No `.desktop` entry is shipped:
  there is nothing to launch.
