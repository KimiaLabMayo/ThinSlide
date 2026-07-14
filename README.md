# ThinSlide
**A high-speed WSI optimizer for DICOM/TIFF/SVS/OME-TIFF.**

## Key Use Cases: Why use ThinSlide?

### 1. DICOM → TIFF conversion
* **Problem**: Fragmented DICOM files are difficult to manage, move, or preview. Redundant JPEG tables and ICC profiles.
* **Solution**: Repacks fragments into a single tiff/OME-TIFF with no redundant data.
* **Performance**: **Zero-decode transcoding** — works at near-copy speeds with zero quality loss.

### 2. Downsampling
* **Problem**: Massive WSI files (1GB+) hinder network transfer and inflate storage costs.
* **Solution**: Downsample to a target resolution using `--mpp` or `--half`. (e.g. 40× → 20×)
* **Benefit**: Reduces file size by **75–85%** while maintaining diagnostic fidelity, making data sharing seamless.

### 3. Color correction with ICC profiles (`--icc-bake`)
* **Problem**: WSI colors look inconsistent in AI scripts, web browsers, or non-specialized viewers.
* **Solution**: Bakes ICC profiles directly into pixel data (supports DICOM, TIFF, SVS, and OME-TIFF).
* **Benefit**: Consistent color in any environment.

---

## Installation

Download the latest pre-built binary from [**Releases**](../../releases/latest). No dependencies required.

| Platform | Binary |
|---|---|
| Linux x86_64 | `thinslide-linux-x86_64-musl` |
| macOS arm64 | `thinslide-macos-arm64`, `thinslide-gui-macos-arm64` |
| macOS x86_64 | `thinslide-macos-x86_64`, `thinslide-gui-macos-x86_64` |
| Windows x86_64 | `thinslide-windows-x86_64.exe`, `thinslide-gui-windows-x86_64.exe` |

**Linux / macOS — make executable after download:**
```sh
chmod +x thinslide-linux-x86_64-musl
```

---

## Usage

```
thinslide <input_dir> <output_dir> [OPTIONS]
```

`thinslide` processes all supported files under `input_dir` in one pass:

| Source file type | Condition | Output |
|---|---|---|
| DICOM (`.dcm`) | always | OME-TIFF (or BigTIFF/SVS with `--legacy`) |
| TIFF / SVS (`.tiff`, `.svs`) | `--mpp`, `--half`, or `--icc-bake` required | OME-TIFF (or BigTIFF with `--legacy`) |
| OME-TIFF (`.ome.tiff`) | `--mpp`, `--half`, or `--icc-bake` required | OME-TIFF with original metadata preserved |

OME-TIFF input is detected by the presence of an OME-XML `ImageDescription` tag (namespace `openmicroscopy.org`), regardless of file extension.
When the source is an OME-TIFF, the output OME-XML inherits all original metadata (Image name, Channel info, Instrument, etc.) and updates only the dimensions and physical pixel size.


### Output formats

By default the output is OME-TIFF. Pass `--legacy` to select a format based on the source:

| Source compression | Default | `--legacy` | `--mpp` / `--icc-bake` |
|---|---|---|---|
| DICOM JPEG | OME-TIFF | BigTIFF | OME-TIFF (or BigTIFF with `--legacy`) |
| DICOM JPEG 2000 | OME-TIFF | Aperio SVS | OME-TIFF (or BigTIFF with `--legacy`) |
| TIFF / SVS (any) | — | — | OME-TIFF (or BigTIFF with `--legacy`) |
| OME-TIFF | — | — | OME-TIFF with inherited metadata (or BigTIFF with `--legacy`) |

### Options

| Option | Description | Default |
|---|---|---|
| `--mpp <N>` | Downsample to this resolution (µm/pixel) | off (passthrough for DICOM) |
| `--half` | Halve width and height (fastest downsampling; mutually exclusive with `--mpp`) | off |
| `--icc-bake` | Apply embedded ICC profile to pixel data and write sRGB output without an ICC profile | off |
| `--legacy` | Write BigTIFF or SVS instead of OME-TIFF | off |
| `-j` / `--jobs <N>` | Number of parallel threads | all CPUs |
| `--quality <N>` | JPEG quality for re-encoding (1–100); used with `--mpp`, `--half`, or `--icc-bake` | 87 |
| `--filter <NAME>` | Resampling filter for `--mpp`: `nearest`, `triangle`, `catmullrom`, `lanczos3`. **Ignored with `--half`** — decode-side halving produces the exact target size with no resize step. | `nearest` |
| `--use-parent-name` | Name DICOM output files after parent directory instead of Series Instance UID | off |
| `-v` / `--verbose` | Print input/output paths and scan summary | off |

### Examples

```sh
# DICOM passthrough — OME-TIFF, all CPUs
thinslide /data/dicoms /data/output

# DICOM passthrough — SVS / BigTIFF (format chosen by DICOM compression type)
thinslide /data/dicoms /data/output --legacy

# Downsample DICOM and TIFF/SVS to 0.5 µm/px
thinslide /data/slides /data/output --mpp 0.5

# Downsample to 1.0 µm/px, Lanczos3 filter, quality 90, 4 threads
thinslide /data/slides /data/output --mpp 1.0 --filter lanczos3 --quality 90 -j 4

# Halve width and height (fastest for JPEG sources: DCT-domain 1/2 decode + no resize)
thinslide /data/slides /data/output --half

# Bake ICC profile into pixels and write sRGB JPEG output without ICC tag
thinslide /data/slides /data/output --icc-bake

# ICC bake combined with downsampling to 0.5 µm/px
thinslide /data/slides /data/output --icc-bake --mpp 0.5

# Mixed directory: DICOM passthrough + TIFF/SVS/OME-TIFF downsampled to 2.0 µm/px
thinslide /data/mixed /data/output --mpp 2.0

# Downsample an OME-TIFF to 0.5 µm/px, preserving original OME-XML metadata
thinslide /data/ome /data/output --mpp 0.5
```

### Parallelism

| Mode | `-j N` controls |
|---|---|
| DICOM passthrough | Number of series processed concurrently (use `-j 1` on HDD) |
| `--mpp` / `--half` (downsampling) | Tile-level parallelism within one series/file (all CPUs is fine) |
| `--icc-bake` | Tile-level parallelism within one series (series are processed sequentially, same as downsampling) |

TIFF/SVS files are always processed one file at a time (tile-level parallelism within each file).


### Downsampling filters

`--filter` applies only to `--mpp` mode. It is **ignored with `--half`**: JPEG sources are halved in the DCT domain (turbojpeg `ONE_HALF`), and JPEG 2000 sources are halved via DWT level-1 reduction. Both paths produce the exact half-size output directly, so no pixel-domain resize step is performed.

| Filter | Algorithm | Notes |
|---|---|---|
| `nearest` | Nearest-neighbour | Fastest; default |
| `triangle` | Bilinear | Smooth, slight blur |
| `catmullrom` | Bicubic | Sharp, good general-purpose |
| `lanczos3` | Lanczos (a=3) | Highest quality, slowest |

---

## GUI (`thinslide-gui`)

`thinslide-gui` is a minimal desktop interface.
It wraps `thinslide` and exposes the same options via a point-and-click window.
Available for macOS and Windows from [Releases](../../releases/latest).

---

## Build from source (optional)

### Prerequisites

- Rust toolchain (edition 2024)
- [libtiff](http://www.libtiff.org/)
- [Little CMS 2](https://www.littlecms.com/)

**macOS:** `brew install libtiff little-cms2`  
**Debian/Ubuntu:** `sudo apt install libtiff-dev liblcms2-dev`  
**Fedora/RHEL:** `sudo dnf install libtiff-devel lcms2-devel`

### CLI only

```sh
cargo build --release --bin thinslide
```

### CLI + GUI

```sh
cargo build --release
```

Both binaries are placed in `target/release/`.

---

## Dependencies

| Crate | Purpose |
|---|---|
| `dicom` / `dicom-object` | Reading DICOM files and metadata |
| `dicom-pixeldata` | Decoding pixel data |
| `image` | Resize and JPEG encoding |
| `fast_image_resize` | Fast tile-level resize |
| `turbojpeg` | Fast JPEG decode/encode for ICC-baked tiles |
| `jpeg2k` | JPEG 2000 decode |
| `lcms2` | ICC color profile transforms (`--icc-bake`) |
| `indicatif` | Progress bars |
| `rayon` | Parallelism |
| `walkdir` | Recursive directory traversal |
| libtiff (FFI) | Writing TIFF/SVS files |
| `eframe` / `egui` | GUI framework (`thinslide-gui`) |
| `rfd` | Native folder picker dialog (`thinslide-gui`) |
