# dcm2tiff

An fast command-line tool that converts Whole Slide Image (WSI) DICOM files into OME-TIFF (default) or legacy pyramidal TIFF / Aperio SVS files.

Two conversion modes are supported:

- **Passthrough** (default): compressed pixel data is written directly to the output file **without decoding or re-encoding**, preserving the original quality and maximising speed.
- **Resampling** (`--mpp`): each tile is decoded, resized to a target resolution, and re-encoded as JPEG. This produces a downsampled copy at a specified microns-per-pixel value.

## Overview

Digital pathology scanners store WSI data as multi-file DICOM sets. This tool reads those files and assembles them into a single pyramidal image file suitable for use with OpenSlide, QuPath, BioFormats, or other WSI viewers.

### Output formats

By default the output is always OME-TIFF. Pass `--legacy` to select a format based on the DICOM transfer syntax:

| DICOM compression | Default output | `--legacy` | `--mpp` (resampling) |
|---|---|---|---|
| JPEG (Baseline, Extended, Lossless, LS) | OME-TIFF (`.ome.tiff`) | BigTIFF (`.tiff`) | OME-TIFF or `.tiff` |
| JPEG 2000 (all variants) | OME-TIFF (`.ome.tiff`) | Aperio SVS (`.svs`) | OME-TIFF or `.tiff` |

Note:

- OME-TIFF is always BigTIFF.
- Resampling mode (`--mpp`) does not produce SVS output; use the default OME-TIFF or `--legacy` BigTIFF instead.
- BigTIFF with Jpeg 2000 compression is not widely supported by popular libraries, so SVS is used as the legacy format for JPEG 2000 input.

## Features

- Optimized for speed. Full support for multi-threaded conversion.
    - **Passthrough** (default): compressed pixel data is written directly without decoding, preserving quality and maximizing speed. One thread per series, limited by disk I/O.
    - **Resampling** (`--mpp`): each tile is decoded, resized to target resolution, and re-encoded as JPEG for downsampling at a specified microns-per-pixel value. (See details below.) Supports parallel processing of tiles within each series.
- For OME-TIFF output, embeds a conforming OME-XML 2016-06 block and uses the TIFF `SubIFD` tag to chain sub-resolution levels, making the pyramid readable by BioFormats and QuPath
- Resampling mode (`--mpp`): decodes each tile, resizes to the target resolution using nearest interpolation, and re-encodes as JPEG; produces a pyramidal OME-TIFF whose resolution tags reflect the actual stored mpp; supports JPEG and JPEG 2000 source data equally

## Requirements

- Rust toolchain (edition 2024)
- [libtiff](http://www.libtiff.org/) (used via FFI)

**macOS (Homebrew):**
```sh
brew install libtiff
```

**Linux (Debian / Ubuntu):**
```sh
sudo apt install libtiff-dev
```

**Linux (Fedora / RHEL):**
```sh
sudo dnf install libtiff-devel
```

The build script uses `pkg-config` to locate libtiff automatically on both macOS and Linux. On macOS without `pkg-config`, it falls back to the Homebrew library paths (`/opt/homebrew/lib`, `/usr/local/lib`).

## Building

```sh
cargo build --release
```

The `bindings.rs` file contains pre-generated bindgen bindings for `tiffio.h`. If you need to regenerate them, install `bindgen-cli` and run it against `wrapper.h`.

## Usage

```
dcm2tiff <input_dir> <output_dir> [OPTIONS]
```

### Options

| Option | Description | Default |
|---|---|---|
| `--legacy` | Write SVS or BigTIFF instead of OME-TIFF (format chosen by compression type) | off |
| `-j` / `--jobs <N>` | Number of parallel threads (see [Parallelism](#parallelism)) | all CPUs |
| `-v` / `--verbose` | Print input/output paths and scan summary | off |
| `--mpp <N>` | Resample output to this resolution (µm/pixel). Triggers decode + resize + JPEG re-encode. Falls back to passthrough if `N` is finer than the source mpp. | off (passthrough) |
| `--quality <N>` | JPEG quality for `--mpp` resampling (1–100) | 87 |
| `--filter <NAME>` | Resampling filter for `--mpp` mode: `nearest`, `triangle`, `catmullrom`, `gaussian`, `lanczos3` | `nearest` |
| `--use-parent-name` | Use the parent directory name of the DICOM files as the output filename stem instead of the Series Instance UID | off |

### Output file naming

By default, output files are named after the DICOM **Series Instance UID**:

- `<SeriesInstanceUID>.ome.tiff` — default (OME-TIFF, any compression)
- `<SeriesInstanceUID>.tiff` — JPEG input with `--legacy`, or any input with `--mpp --legacy`
- `<SeriesInstanceUID>.svs` — JPEG 2000 input with `--legacy` (not produced with `--mpp`)

With `--use-parent-name`, the stem is replaced by the **name of the directory containing the DICOM files**:

```
/data/slides/case001/slide.dcm  →  <output_dir>/case001.ome.tiff
```

This is convenient when DICOM files are organised into per-case or per-slide subdirectories and human-readable names are preferred over UIDs.

### Examples

```sh
# Default: OME-TIFF passthrough, all available CPUs
./target/release/dcm2tiff /data/wsi_dicoms /data/output

# Legacy: auto-select BigTIFF or SVS based on compression type
./target/release/dcm2tiff /data/wsi_dicoms /data/output --legacy

# Resample to 0.5 µm/px, default filter (nearest) and quality (87)
./target/release/dcm2tiff /data/wsi_dicoms /data/output --mpp 0.5

# Resample to 1.0 µm/px, bilinear filter, quality 90, 4 threads
./target/release/dcm2tiff /data/wsi_dicoms /data/output --mpp 1.0 --filter triangle --quality 90 -j 4

# Resample to 0.5 µm/px, high quality (Lanczos3)
./target/release/dcm2tiff /data/wsi_dicoms /data/output --mpp 0.5 --filter lanczos3

# Resample to 0.5 µm/px, write as flat BigTIFF (no OME-XML)
./target/release/dcm2tiff /data/wsi_dicoms /data/output --mpp 0.5 --legacy

# Use parent directory name as output filename (e.g. case001.ome.tiff)
./target/release/dcm2tiff /data/wsi_dicoms /data/output --use-parent-name
```

## Parallelism

`-j N` sets the number of threads, but its effect depends on the mode:

### Resampling mode (`--mpp`)

WSI series are processed **one at a time**. All `N` threads are used in parallel to decode, resize, and JPEG-encode the tiles within that single series. Only one output file is written at any moment, so there is no concurrent seeking even on HDD.

```
Series A  ────[tile decode / resize / encode, N threads]──── write ──► done
Series B  ──────────────────────────────────────────────[tile decode / resize / encode, N threads]──── write ──► done
```

Use a higher `-j` (or the default, all CPUs) to maximise encoding throughput. Reducing `-j` in resampling mode only slows down tile encoding with no I/O benefit.

### Passthrough mode (default, no `--mpp`)

Up to `N` series are processed **concurrently**, each writing its own output file simultaneously. This overlaps I/O across series, which benefits SSD but causes random seeking on HDD.

```
Series A  ──[read raw tiles]──[write TIFF]──────────────────────────────────► done
Series B          ──[read raw tiles]──[write TIFF]──────────────────────────► done
Series C                  ──[read raw tiles]──[write TIFF]──────────────────► done
```

**On HDD**, use `-j 1` to process series sequentially and avoid concurrent seeking:

```sh
./target/release/dcm2tiff /data/wsi_dicoms /data/hdd_output -j 1
```

### Summary

| Mode | `-j N` controls | Recommended for HDD |
|---|---|---|
| `--mpp` (resampling) | Tile-level parallelism (decode + resize + encode) | Default (all CPUs) is fine |
| Passthrough (default) | Number of series processed concurrently | `-j 1` to avoid seek contention |

## Resampling mode (`--mpp`)

When `--mpp <N>` is specified the tool produces a new pyramidal image at the target resolution. For each output pyramid level the tool independently selects the closest existing input level as its source, then either copies tiles directly or decodes, resizes, and re-encodes them depending on how far the source MPP is from the target.

### Per-level source selection

Each output level has its own target MPP derived from the requested `--mpp` value scaled by the pyramid ratio. The input level whose MPP is closest to that target is used as the source for that level:

- **Within 10 %**: tiles are copied as-is (passthrough — no decode/encode).
- **More than 10 % away**: tiles are decoded, resized to the target size, and re-encoded as JPEG.

Example with input levels at 0.25 / 0.5 / 1.0 / 2.0 µm/px and `--mpp 2.0`:

```
output level 0  target 2.0 µm/px  →  source 2.0 µm/px  (0 % diff)   → passthrough
output level 1  target 4.0 µm/px  →  source 2.0 µm/px  (50 % diff)  → resample
```

### Resampling filter (`--filter`)

| Value | Algorithm | Speed | Quality |
|---|---|---|---|
| `nearest` | Nearest-neighbour | fastest | blocky at large downscale ratios |
| `triangle` | Bilinear | fast | smooth, slight blur |
| `catmullrom` | Bicubic (Catmull-Rom) | moderate | sharp, good general-purpose |
| `gaussian` | Gaussian | moderate | soft/blurred |
| `lanczos3` | Lanczos (a=3) | slowest | highest quality |

`nearest` is the default because it is significantly faster than the interpolating filters and produces acceptable results for large downsampling factors typical in WSI workflows (e.g. 0.25 → 2.0 µm/px). Use `triangle` or `catmullrom` when image quality matters more than speed.

### How the output resolution is determined

The output MPP may differ slightly from the requested value due to tile-size rounding. Each output level computes its tile size independently from its chosen source level:

```
target_tile_size = nearest_multiple_of_16( source_tile_size × source_mpp / target_mpp )
actual_mpp = source_mpp × source_tile_size / target_tile_size
```

This value is written into the TIFF `XResolution`/`YResolution` tags and into the OME-XML `PhysicalSizeX/Y` attributes, so viewers report the correct pixel size.

### Fallback behaviour

If the requested `--mpp` is smaller (finer) than the base source resolution, the tool falls back to full passthrough mode rather than inventing detail that does not exist in the source data.

```
source mpp: 0.25 µm/px
--mpp 0.1       → fallback to passthrough (0.1 < 0.25)
--mpp 0.25      → fallback to passthrough (within 10 %)
--mpp 0.5       → resample  (0.5 > 0.25)
```

## IFD Structure

### OME-TIFF (default and `--mpp` without `--legacy`)

| Location | Content | SubFileType |
|---|---|---|
| IFD 0 (main chain) | Full / base-resampled resolution, tiled; OME-XML in `ImageDescription`; `SubIFD` tag pointing to sub-resolutions | 0 |
| SubIFD 0…N-1 (chained from IFD 0) | Reduced pyramid levels (descending), tiled | 1 |
| IFD 1+ (main chain, optional) | Thumbnail / label / overview, stripped JPEG (passthrough only) | 1 |

The OME-XML embedded in `ImageDescription` conforms to the [OME 2016-06 schema](https://www.openmicroscopy.org/Schemas/OME/2016-06). In resampling mode the OME-XML reflects the resampled image dimensions and actual stored mpp.

### Generic pyramidal BigTIFF (`--legacy`, JPEG input or `--mpp --legacy`)

| IFD | Content | SubFileType |
|---|---|---|
| 0 | Full / base-resampled resolution, tiled | 0 |
| 1..N | Reduced pyramid levels (descending size), tiled | 1 |

### SVS (Aperio) (`--legacy`, JPEG 2000 input, passthrough only)

| IFD | Content | SubFileType |
|---|---|---|
| 0 | Full resolution, tiled; Aperio `ImageDescription` | 0 |
| 1 | Thumbnail, stripped JPEG | 1 |
| 2..N | Remaining pyramid levels (descending), tiled | 1 |
| N+1 | Label image, stripped JPEG (optional) | 1 |
| N+2 | Macro/Overview image, stripped JPEG (optional) | 9 |

## Dependencies

| Crate | Purpose |
|---|---|
| `dicom` / `dicom-object` | Reading DICOM files and metadata |
| `dicom-pixeldata` | Decoding pixel data (tile decode for resampling; thumbnail/label/overview) |
| `image` | Lanczos3 resize and JPEG encoding in resampling mode |
| `indicatif` | Live per-series progress bars with `MultiProgress` |
| `rayon` | Parallel conversion of multiple series |
| `walkdir` | Recursive directory traversal |
| libtiff (FFI) | Writing TIFF/SVS files with raw tile support and built-in JPEG encoding |

## Limitations

- The build script uses `pkg-config` to find libtiff. If `pkg-config` is not available, it falls back to Homebrew paths on macOS; on other systems without `pkg-config`, set `LIBRARY_PATH` or adjust `build.rs`.
- OME-TIFF output assumes a 2D slide (no Z-stack or time series); `SizeZ=1`, `SizeT=1`.
- In resampling mode, pyramid generation requires that the source DICOM itself contains multiple resolution levels. Single-level source DICOMs produce single-IFD output without a pyramid.