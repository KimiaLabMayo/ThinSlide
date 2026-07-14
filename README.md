# ThinSlide

Optimize whole-slide images for storage, portability, and interoperability.

- **Repack** — WSIs are fragmented. Consolidate them into clean TIFF/OME-TIFF at near-copy speed.
- **Slim** — WSIs are heavy. Reduce storage by 75–85% with fast downsampling.
- **Color** — Colors are not portable. Bake ICC profiles into pixels for consistent visualization.

Powered by a shared zero-decode pipeline.

## Format support

| Format | Repack | Downsample | Color |
|---|:---:|:---:|:---:|
| DICOM | ✓ | ✓ `--half` / `--mpp` | ✓ `--icc-bake` |
| SVS / TIFF | ✓¹ | ✓ `--half` / `--mpp` | ✓ `--icc-bake` |
| VSI (CellSens)² | ✓ | `--half` only | — |
| MRXS (MIRAX)² | ✓ | `--half` only | — |

¹ A bare SVS/TIFF input is already a valid file, so plain repack is skipped unless combined with `--half`, `--mpp`, or `--icc-bake`.
² Experimental readers; VSI 16-bit fluorescence channels are skipped (8-bit brightfield only).

> [!WARNING]
> **Research use only.** Thinslide is not a medical device and is not intended for clinical or diagnostic use. 

## Desktop app (no command line)

If you'd rather not use the terminal, **`thinslide-gui`** is a simple desktop app that does the same thing in a window.

1. Download `thinslide-gui` for [macOS or Windows](../../releases/latest).
2. Open it, then choose a folder of slides and a destination folder.
3. Pick what you want — repack, halve the size, or fix color — and click **Run**.

On macOS, also run `brew install libtiff little-cms2` once. No such step is needed on Windows.

![ThinSlide GUI screenshot](assets/gui_screenshot.png)


## Command line

### Installation

#### Prebuilt binaries

Prebuilt binaries are attached to every [release](https://github.com/uegamiw/thinslide/releases/latest). Download the one for your platform, make it executable, and put it on your `PATH`:

| Platform | Asset | Includes GUI | Dependencies |
|----------|-------|:---:|---|
| Linux x86_64 | `thinslide-linux-x86_64-musl` | — | none (static musl) |
| macOS arm64 | `thinslide-macos-arm64` | ✓ | libtiff, Little CMS 2 |
| Windows x86_64 | `thinslide-windows-x86_64.exe` | ✓ | none (static) |

```sh
# Linux / macOS
curl -L -o thinslide https://github.com/uegamiw/ThinSlide/releases/latest/download/thinslide-linux-x86_64-musl
chmod +x ThinSlide
sudo mv ThinSlide /usr/local/bin/
```

On Windows, download `thinslide-windows-x86_64.exe` and add its folder to `PATH`.

The macOS build links libtiff and Little CMS 2 dynamically, so install them once with `brew install libtiff little-cms2`. The Linux and Windows builds are statically linked and need nothing else.

#### From crates.io or source

Building locally requires a [Rust toolchain](https://rustup.rs) (edition 2024) and the **development** headers for [libtiff](http://www.libtiff.org/) and [Little CMS 2](https://www.littlecms.com/) (the `-dev`/`-devel` packages):

```sh
# macOS
brew install libtiff little-cms2
# Debian / Ubuntu
sudo apt install libtiff-dev liblcms2-dev
# Fedora / RHEL
sudo dnf install libtiff-devel lcms2-devel
```

Then install from crates.io:

```sh
cargo install thinslide
```

Or build from source:

```sh
git clone https://github.com/uegamiw/thinslide
cd thinslide
cargo install --path .         # or install it onto your PATH (~/.cargo/bin)
```

### Usage

```sh
thinslide <input_dir> <output_dir> [options]
```

Input and output are directories — Thinslide processes every slide it finds, mixed formats included.

### Examples

```sh
# Repack DICOM → OME-TIFF (default; uses all CPUs)
thinslide /data/dicoms /data/output

# Repack to SVS/BigTIFF instead (output format follows DICOM compression)
thinslide /data/dicoms /data/output --legacy

# Halve dimensions — the fast path (DCT-domain 1/2 decode, no resample)
thinslide /data/slides /data/output --half

# Bake ICC profile into pixels, output sRGB JPEG (no ICC tag)
thinslide /data/slides /data/output --icc-bake

# Downsample to an arbitrary resolution instead of halving
thinslide /data/slides /data/output --mpp 0.5

# ...tuning the filter, JPEG quality, and thread count
thinslide /data/slides /data/output --mpp 1.0 --filter lanczos3 --quality 90 -j 4

# Combine: bake color and halve in one pass
thinslide /data/slides /data/output --icc-bake --half
```

> OME-TIFF inputs keep their original OME-XML metadata through downsampling.

## Acknowledgments

Thinslide's MIRAX (.mrxs) and CellSens (.vsi) readers were developed with
reference to, and in part ported from, the following open-source projects:

- [OpenSlide](https://openslide.org/) (LGPL-2.1) — MIRAX format parsing
- [Bio-Formats](https://www.openmicroscopy.org/bio-formats/) (GPLv2) — CellSens VSI format parsing

## License

Copyright (C) 2026 Wataru Uegami, MD, PhD

Thinslide is licensed under the **GNU General Public License v2.0** — see [LICENSE](LICENSE).

## Disclaimer

Thinslide is provided for **research use only**. It is not a medical device, has not been cleared or approved by any regulatory authority, and is not intended for clinical diagnosis, treatment, or any patient-care decision. The software is provided "as is", without warranty of any kind, to the extent permitted by applicable law.
