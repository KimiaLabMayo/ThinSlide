# wsi-tools

A fast command-line toolkit for Whole Slide Image (WSI) processing.

| Command | Input | Output |
|---|---|---|
| `dcm2tiff` | DICOM WSI (directory) | Pyramidal TIFF / OME-TIFF / SVS |
| `tiffds` | TIFF / SVS (directory) | Downsampled pyramidal OME-TIFF / TIFF |

## Requirements

- Rust toolchain (edition 2024)
- [libtiff](http://www.libtiff.org/)

**macOS:** `brew install libtiff`  
**Debian/Ubuntu:** `sudo apt install libtiff-dev`  
**Fedora/RHEL:** `sudo dnf install libtiff-devel`

## Building

```sh
cargo build --release
```

Binaries are placed at `target/release/dcm2tiff` and `target/release/tiffds`.

---

## dcm2tiff

Converts Whole Slide Image DICOM sets into pyramidal TIFF files.

### Modes

- **Passthrough** (default): compressed pixel data is written directly without decoding, preserving original quality and maximising speed.
- **Downsampling** (`--mpp`): tiles are decoded, resized to a target resolution, and re-encoded as JPEG.

### Output formats

By default the output is OME-TIFF. Pass `--legacy` to select a format based on the DICOM transfer syntax:

| DICOM compression | Default | `--legacy` | `--mpp` |
|---|---|---|---|
| JPEG | OME-TIFF | BigTIFF | OME-TIFF (or BigTIFF with `--legacy`) |
| JPEG 2000 | OME-TIFF | Aperio SVS | OME-TIFF (or BigTIFF with `--legacy`) |

### Usage

```
dcm2tiff <input_dir> <output_dir> [OPTIONS]
```

| Option | Description | Default |
|---|---|---|
| `--mpp <N>` | Downsample to this resolution (µm/pixel) | off (passthrough) |
| `--half` | Halve width and height (fastest downsampling; mutually exclusive with `--mpp`) | off |
| `--legacy` | Write BigTIFF or SVS instead of OME-TIFF | off |
| `-j` / `--jobs <N>` | Number of parallel threads | all CPUs |
| `--quality <N>` | JPEG quality for downsampling (1–100) | 87 |
| `--filter <NAME>` | Downsampling filter: `nearest`, `triangle`, `catmullrom`, `lanczos3` | `nearest` |
| `--use-parent-name` | Name output files after parent directory instead of Series Instance UID | off |
| `-v` / `--verbose` | Print input/output paths and scan summary | off |

### Examples

```sh
# Passthrough — OME-TIFF, all CPUs
./target/release/dcm2tiff /data/dicoms /data/output

# Passthrough — SVS / BigTIFF (format chosen by DICOM compression type)
./target/release/dcm2tiff /data/dicoms /data/output --legacy

# Downsample to 0.5 µm/px
./target/release/dcm2tiff /data/dicoms /data/output --mpp 0.5

# Downsample to 1.0 µm/px, Lanczos3 filter, quality 90, 4 threads
./target/release/dcm2tiff /data/dicoms /data/output --mpp 1.0 --filter lanczos3 --quality 90 -j 4

# On HDD: process one series at a time to avoid seek contention
./target/release/dcm2tiff /data/dicoms /data/output -j 1

# Halve width and height (fastest for JPEG sources: DCT-domain 1/2 decode + no resize)
./target/release/dcm2tiff /data/dicoms /data/output --half
```

### Parallelism

| Mode | `-j N` controls |
|---|---|
| Passthrough | Number of series processed concurrently (use `-j 1` on HDD) |
| `--mpp` / `--half` (downsampling) | Tile-level parallelism within one series (all CPUs is fine) |

---

## tiffds

Downsamples existing TIFF / SVS files to a target resolution. Reads every `.tiff` / `.svs` file under the input directory and writes a pyramidal OME-TIFF (default) or BigTIFF (`--legacy`).

Either `--mpp` or `--half` is required.

### Usage

```
tiffds <input_dir> <output_dir> (--mpp <N> | --half) [OPTIONS]
```

| Option | Description | Default |
|---|---|---|
| `--mpp <N>` | Target resolution (µm/pixel); required unless `--half` | — |
| `--half` | Halve width and height (fastest downsampling; mutually exclusive with `--mpp`) | off |
| `--legacy` | Write flat pyramidal BigTIFF instead of OME-TIFF | off |
| `-j` / `--jobs <N>` | Number of parallel threads | all CPUs |
| `--quality <N>` | JPEG quality (1–100) | 87 |
| `--filter <NAME>` | Downsampling filter: `nearest`, `triangle`, `catmullrom`, `lanczos3` | `nearest` |
| `-v` / `--verbose` | Print input/output paths | off |

### Examples

```sh
# Downsample all TIFFs/SVS files to 2.0 µm/px
./target/release/tiffds /data/slides /data/output --mpp 2.0

# High-quality downsampling to 1.0 µm/px
./target/release/tiffds /data/slides /data/output --mpp 1.0 --filter lanczos3 --quality 90

# Write flat BigTIFF instead of OME-TIFF
./target/release/tiffds /data/slides /data/output --mpp 2.0 --legacy

# Halve width and height (fastest for JPEG sources)
./target/release/tiffds /data/slides /data/output --half
```

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
| `indicatif` | Progress bars |
| `rayon` | Parallelism |
| `walkdir` | Recursive directory traversal |
| libtiff (FFI) | Writing TIFF/SVS files |
