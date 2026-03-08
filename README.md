# dcm2tiff

An extremely fast command-line tool that converts Whole Slide Image (WSI) DICOM files into OME-TIFF (default) or legacy pyramidal TIFF / Aperio SVS files — without re-encoding the pixel data.

## Overview

Digital pathology scanners often store WSI data as multi-file DICOM sets. This tool reads those files and assembles them into a single pyramidal image file suitable for use with OpenSlide, QuPath, BioFormats, or other WSI viewers.

By default the output is always OME-TIFF. Pass `--legacy` to get the format-specific output based on the DICOM transfer syntax:

| DICOM compression | Default output | With `--legacy` |
|---|---|---|
| JPEG (Baseline, Extended, Lossless, LS) | OME-TIFF (`.ome.tiff`) | Pyramidal BigTIFF (`.tiff`) |
| JPEG 2000 (all variants) | OME-TIFF (`.ome.tiff`) | Aperio SVS (`.svs`) |

OME-TIFF is always BigTIFF and works with any compression type. In legacy mode, SVS is used for JPEG 2000 input because OpenSlide does not support JPEG 2000-compressed tiles in generic BigTIFF.

Pixel data is written directly to the output file without decoding or re-encoding, preserving the original compressed data.

## Features

- Scans an input directory recursively for `.dcm` files
- Groups files by DICOM series and identifies WSI series (SOP Class UID `1.2.840.10008.5.1.4.1.1.77.1.6`, Modality `SM`)
- Assembles multi-file pyramid levels (a single resolution level may be split across multiple DICOM instances)
- Correctly places tiles using `PerFrameFunctionalGroupsSequence` / `PlanePositionSlideSequence` position metadata
- Detects color space from `PhotometricInterpretation`, APP14 JPEG markers, or `SamplesPerPixel`
- Detects YCbCr chroma subsampling factors from the JPEG stream and writes the correct `YCbCrSubSampling` TIFF tag
- Embeds resolution metadata (microns per pixel) as TIFF `XResolution`/`YResolution` (pixels/cm)
- For SVS output, writes Aperio `ImageDescription` with magnification and MPP, and includes thumbnail, label, and macro/overview images
- For OME-TIFF output, embeds a conforming OME-XML 2016-06 block and uses the TIFF `SubIFD` tag to chain sub-resolution levels, making the pyramid readable by BioFormats and QuPath

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
dcm2tiff <input_dir> <output_dir> [--legacy] [-v | --verbose]
```

- `<input_dir>`: Directory (searched recursively) containing `.dcm` files
- `<output_dir>`: Directory where output files will be written (created if it does not exist)
- `--legacy`: Write SVS or generic BigTIFF instead of OME-TIFF (format chosen by compression type)
- `-v` / `--verbose`: Print input/output paths and DICOM scan summary in addition to progress bars

Output files are named after the DICOM Series Instance UID:

- `<SeriesInstanceUID>.ome.tiff` — any compression, default
- `<SeriesInstanceUID>.tiff` — JPEG-compressed input, with `--legacy`
- `<SeriesInstanceUID>.svs` — JPEG 2000-compressed input, with `--legacy`


### Examples

```sh
# Default: OME-TIFF with live progress bars
./target/release/dcm2tiff /data/wsi_dicoms /data/output

# Legacy: auto-select BigTIFF or SVS based on compression type
./target/release/dcm2tiff /data/wsi_dicoms /data/output --legacy
```

## IFD Structure

### OME-TIFF (default)

The default output uses the TIFF `SubIFD` tag so that BioFormats can navigate sub-resolutions natively:

| Location | Content | SubFileType |
|---|---|---|
| IFD 0 (main chain) | Full resolution, tiled; OME-XML in `ImageDescription`; `SubIFD` tag pointing to sub-resolutions | 0 |
| SubIFD 0…N-1 (chained from IFD 0) | Reduced pyramid levels (descending), tiled | 1 |
| IFD 1+ (main chain, optional) | Thumbnail / label / overview, stripped JPEG | 1 |

The OME-XML embedded in `ImageDescription` conforms to the [OME 2016-06 schema](https://www.openmicroscopy.org/Schemas/OME/2016-06). Key attributes:

- `DimensionOrder="XYZCT"`, `SizeZ/T=1`
- `SizeC` and `SamplesPerPixel` from DICOM `SamplesPerPixel` / `BitsAllocated`
- `PhysicalSizeX/Y` in µm derived from DICOM volume dimensions or `PixelSpacing`
- `Interleaved="true"` for colour images
- `TiffData IFD="0"` referencing the full-resolution IFD

### Generic pyramidal BigTIFF (`--legacy`, JPEG input)

| IFD | Content | SubFileType |
|---|---|---|
| 0 | Full resolution, tiled | 0 |
| 1..N | Reduced pyramid levels (descending size), tiled | 1 |

### SVS (Aperio) (`--legacy`, JPEG 2000 input)

The Aperio SVS format requires IFDs in a specific order for OpenSlide compatibility:

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
| `dicom-pixeldata` | Decoding pixel data (used for thumbnail/label/overview re-encoding) |
| `image` | Encoding decoded frames as JPEG |
| `indicatif` | Live per-series progress bars with `MultiProgress` |
| `rayon` | Parallel conversion of multiple series |
| `walkdir` | Recursive directory traversal |
| libtiff (FFI) | Writing TIFF/SVS files with raw tile support |

## Limitations

- Only WSI DICOM files (SOP Class `1.2.840.10008.5.1.4.1.1.77.1.6`, Modality `SM`) are processed; other DICOM files in the input directory are ignored.
- Uncompressed or transfer syntaxes not listed in the supported set are treated as JPEG 2000 by default.
- The build script uses `pkg-config` to find libtiff. If `pkg-config` is not available, it falls back to Homebrew paths on macOS; on other systems without `pkg-config`, set `LIBRARY_PATH` or adjust `build.rs`.
- OME-TIFF output assumes a 2D slide (no Z-stack or time series); `SizeZ=1`, `SizeT=1`.
