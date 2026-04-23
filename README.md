# SlideLeaner: The high-speed WSI optimizer for sustainable pathology.

A fast command-line toolkit for Whole Slide Image (WSI) processing.

## Features

- **Zero-Decode Transcoding**: Repacks DICOM to TIFF by mapping binary bitstreams directly into containers. No re-encoding, zero quality loss, and near-instantaneous execution.

- **High-Speed Parallel Architecture**: Leverages multi-core processing and concurrent I/O pipelines (read/transform/write) to eliminate processing bottlenecks.

- **Interoperability by Default**: Outputs OME-TIFF to ensure seamless integration with the global bio-imaging ecosystem (Legacy TIFF mode available via --legacy).

- **Smart Downsampling**: Optimized tile-based resampling and stitching for arbitrary MPP or high-speed 1/2 downsizing without memory bloat.

- **Color Accuracy (--icc-bake)**: (Experimental) Bakes ICC profiles directly into RGB pixel data for consistent visualization across non-ICC aware analysis tools.

- **Compact Storage**: Optimizes file size using JPEG tables within TIFF, ensuring high-fidelity data remains manageable and sustainable.

- **Research-Ready**: Consolidates fragmented DICOM file sets into a single, high-performance file, streamlining data management and transfer for downstream projects.

`slean` scans an input directory for DICOM WSI files (`.dcm`) and pyramidal TIFF/SVS files (`.tiff`, `.svs`) and converts or downsamples them into pyramidal TIFF files.

## Requirements

- Rust toolchain (edition 2024)
- [libtiff](http://www.libtiff.org/)
- [Little CMS 2](https://www.littlecms.com/) (required for `--icc-bake`)

**macOS:** `brew install libtiff little-cms2`  
**Debian/Ubuntu:** `sudo apt install libtiff-dev liblcms2-dev`  
**Fedora/RHEL:** `sudo dnf install libtiff-devel lcms2-devel`

## Building

```sh
cargo build --release
```

Binary is placed at `target/release/slean`.

---

## Usage

```
slean <input_dir> <output_dir> [OPTIONS]
```

`slean` processes all supported files under `input_dir` in one pass:

| Source file type | Condition | Output |
|---|---|---|
| DICOM (`.dcm`) | always | OME-TIFF (or BigTIFF/SVS with `--legacy`) |
| TIFF / SVS | `--mpp` or `--half` required | OME-TIFF (or BigTIFF with `--legacy`) |

### Modes

- **Passthrough** (DICOM, default): compressed pixel data is written directly without decoding, preserving original quality and maximising speed.
- **Downsampling** (`--mpp` / `--half`): tiles are decoded, resized to a target resolution, and re-encoded as JPEG. Applies to both DICOM and TIFF/SVS sources.
- **ICC bake** (`--icc-bake`, DICOM only): tiles are decoded, the embedded ICC color profile is applied to convert pixels to sRGB, and the tiles are re-encoded as JPEG. The ICC profile tag is omitted from the output, so any viewer displays correct colors without needing ICC support. Useful when the source has a large per-WSI ICC profile (e.g. Hamamatsu: ~13 MB) that causes storage bloat or viewer failures. Note that this feature is experimental and not optimised for performance.

### Output formats

By default the output is OME-TIFF. Pass `--legacy` to select a format based on the source:

| Source compression | Default | `--legacy` | `--mpp` / `--icc-bake` |
|---|---|---|---|
| DICOM JPEG | OME-TIFF | BigTIFF | OME-TIFF (or BigTIFF with `--legacy`) |
| DICOM JPEG 2000 | OME-TIFF | Aperio SVS | OME-TIFF (or BigTIFF with `--legacy`) |
| TIFF/SVS (any) | — | — | OME-TIFF (or BigTIFF with `--legacy`) |

### Options

| Option | Description | Default |
|---|---|---|
| `--mpp <N>` | Downsample to this resolution (µm/pixel) | off (passthrough for DICOM) |
| `--half` | Halve width and height (fastest downsampling; mutually exclusive with `--mpp`) | off |
| `--icc-bake` | Apply embedded ICC profile to pixel data and write sRGB output without an ICC tag (DICOM only) | off |
| `--legacy` | Write BigTIFF or SVS instead of OME-TIFF | off |
| `-j` / `--jobs <N>` | Number of parallel threads | all CPUs |
| `--quality <N>` | JPEG quality for re-encoding (1–100); used with `--mpp`, `--half`, or `--icc-bake` | 87 |
| `--filter <NAME>` | Downsampling filter: `nearest`, `triangle`, `catmullrom`, `lanczos3` | `nearest` |
| `--use-parent-name` | Name DICOM output files after parent directory instead of Series Instance UID | off |
| `-v` / `--verbose` | Print input/output paths and scan summary | off |

### Examples

```sh
# DICOM passthrough — OME-TIFF, all CPUs
./target/release/slean /data/dicoms /data/output

# DICOM passthrough — SVS / BigTIFF (format chosen by DICOM compression type)
./target/release/slean /data/dicoms /data/output --legacy

# Downsample DICOM and TIFF/SVS to 0.5 µm/px
./target/release/slean /data/slides /data/output --mpp 0.5

# Downsample to 1.0 µm/px, Lanczos3 filter, quality 90, 4 threads
./target/release/slean /data/slides /data/output --mpp 1.0 --filter lanczos3 --quality 90 -j 4

# Halve width and height (fastest for JPEG sources: DCT-domain 1/2 decode + no resize)
./target/release/slean /data/slides /data/output --half

# Bake ICC profile into pixels and write sRGB JPEG output without ICC tag (DICOM only)
./target/release/slean /data/dicoms /data/output --icc-bake

# ICC bake combined with downsampling to 0.5 µm/px
./target/release/slean /data/dicoms /data/output --icc-bake --mpp 0.5

# Mixed directory: DICOM passthrough + TIFF/SVS downsampled to 2.0 µm/px
./target/release/slean /data/mixed /data/output --mpp 2.0

# On HDD: process DICOM one series at a time to avoid seek contention
./target/release/slean /data/dicoms /data/output -j 1
```

### Parallelism

| Mode | `-j N` controls |
|---|---|
| DICOM passthrough | Number of series processed concurrently (use `-j 1` on HDD) |
| `--mpp` / `--half` (downsampling) | Tile-level parallelism within one series/file (all CPUs is fine) |
| `--icc-bake` | Tile-level parallelism within one series (series are processed sequentially, same as downsampling) |

TIFF/SVS files are always processed one file at a time (tile-level parallelism within each file).

---

## Downsampling details

When `--mpp` is specified, each output pyramid level independently selects the closest source level as its source:

- **Within 10 %**: tiles are copied as-is (passthrough).
- **More than 10 % away**: tiles are decoded, resized, and re-encoded as JPEG.

If the requested `--mpp` is finer than the source base resolution, the tool falls back to passthrough rather than inventing detail.

### Downsampling filters

| Filter | Algorithm | Notes |
|---|---|---|
| `nearest` | Nearest-neighbour | Fastest; default |
| `triangle` | Bilinear | Smooth, slight blur |
| `catmullrom` | Bicubic | Sharp, good general-purpose |
| `lanczos3` | Lanczos (a=3) | Highest quality, slowest |

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
