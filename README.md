# dcm2tiff

A command-line tool written in Rust that converts Whole Slide Image (WSI) DICOM files into pyramidal TIFF, OME-TIFF, or Aperio SVS files — without re-encoding the pixel data.

## Overview

Digital pathology scanners often store WSI data as multi-file DICOM sets. This tool reads those files and assembles them into a single pyramidal image file suitable for use with OpenSlide, QuPath, BioFormats, or other WSI viewers.

The output format depends on the DICOM transfer syntax and the `--ome` flag:

| DICOM compression | Default output | With `--ome` |
|---|---|---|
| JPEG (Baseline, Extended, Lossless, LS) | Pyramidal BigTIFF (`.tiff`) | OME-TIFF (`.ome.tiff`) |
| JPEG 2000 (all variants) | Aperio SVS (`.svs`) | OME-TIFF (`.ome.tiff`) |

Note that OpenSlide does not support JPEG 2000-compressed tiles in BigTIFF, so the default for JPEG 2000 input is SVS. OME-TIFF is always BigTIFF and works with any compression type.

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

On macOS with Homebrew:

```sh
brew install libtiff
```

The build script (`build.rs`) automatically links against `/opt/homebrew/lib/libtiff`. If your libtiff is installed elsewhere, update `build.rs` accordingly.

## Building

```sh
cargo build --release
```

The `bindings.rs` file contains pre-generated bindgen bindings for `tiffio.h`. If you need to regenerate them, install `bindgen-cli` and run it against `wrapper.h`.

## Usage

```
dcm2tiff <input_dir> <output_dir> [--ome]
```

- `<input_dir>`: Directory (searched recursively) containing `.dcm` files
- `<output_dir>`: Directory where output files will be written (must exist)
- `--ome`: Write OME-TIFF instead of the default format (BigTIFF or SVS)

Output files are named after the DICOM Series Instance UID:

- `<SeriesInstanceUID>.tiff` — JPEG-compressed input, default
- `<SeriesInstanceUID>.svs` — JPEG 2000-compressed input, default
- `<SeriesInstanceUID>.ome.tiff` — any compression, when `--ome` is specified

### Examples

```sh
# Default: auto-select TIFF or SVS based on compression
./target/release/dcm2tiff /data/wsi_dicoms /data/output

# OME-TIFF output (BioFormats / QuPath compatible pyramid)
./target/release/dcm2tiff /data/wsi_dicoms /data/output --ome
```

## IFD Structure

### Generic pyramidal BigTIFF / OME-TIFF

For JPEG-compressed input (default) the IFDs are laid out as a flat sequence:

| IFD | Content | SubFileType |
|---|---|---|
| 0 | Full resolution, tiled | 0 |
| 1..N | Reduced pyramid levels (descending size), tiled | 1 |

#### OME-TIFF (`--ome`)

When `--ome` is specified the pyramid uses the TIFF `SubIFD` tag so that BioFormats can navigate sub-resolutions natively:

| Location | Content | SubFileType |
|---|---|---|
| IFD 0 (main chain) | Full resolution, tiled; OME-XML in `ImageDescription`; `SubIFD` tag pointing to sub-resolutions | 0 |
| SubIFD 0…N-1 (chained from IFD 0) | Reduced pyramid levels (descending), tiled | 1 |
| IFD 1+ (main chain, optional) | Thumbnail / label / overview, stripped JPEG | 1 |

The OME-XML embedded in `ImageDescription` conforms to the [OME 2016-06 schema](https://www.openmicroscopy.org/Schemas/OME/2016-06). Key attributes written:

- `DimensionOrder="XYZCT"`, `SizeZ/T=1`
- `SizeC` and `SamplesPerPixel` from DICOM `SamplesPerPixel` / `BitsAllocated`
- `PhysicalSizeX/Y` in µm derived from DICOM volume dimensions or `PixelSpacing`
- `Interleaved="true"` for colour images
- `TiffData IFD="0"` referencing the full-resolution IFD

### SVS (Aperio)

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
| `walkdir` | Recursive directory traversal |
| libtiff (FFI) | Writing TIFF/SVS files with raw tile support |

## Limitations

- Only WSI DICOM files (SOP Class `1.2.840.10008.5.1.4.1.1.77.1.6`, Modality `SM`) are processed; other DICOM files in the input directory are ignored.
- Uncompressed or transfer syntaxes not listed in the supported set are treated as JPEG 2000 by default.
- The build script assumes Homebrew libtiff on macOS (`/opt/homebrew/lib`). Linux users will need to adjust `build.rs`.
- OME-TIFF output assumes a 2D slide (no Z-stack or time series); `SizeZ=1`, `SizeT=1`.
