# ThinSlide

**Whole-slide image (WSI) optimization — toward sustainable digital pathology.**

- **Convert** — WSIs are fragmented. Consolidate them into clean TIFF/OME-TIFF.
- **Downsample** — WSIs are heavy. Normalize 40x scans to 20x and save storage by ~75%. `--scale 20x`
- **Standardize** — Colors are not portable. Bake ICC profiles into the pixels. `--icc-bake`
- **Crop** - WSIs are huge. Keep only the annotated regions. `--roi <file.geojson|DIR>`

## Formats

| Input | default | `--scale 20x` | `--icc-bake` |
|---|:---:|:---:|:---:|
| **DICOM** | 🔵  | 🟢  | 🟢 |
| **SVS / TIFF** | —² | 🟢  | 🟢  |
| **VSI** (CellSens)¹ | 🔵  | 🟢  | 🟢  |
| **NDPI** (Hamamatsu)³ | —³ | 🟢  | 🟢  |

**🔵 repackaging** — compressed tiles are copied straight through, at near-copy speed. No image quality change.

**🟢 re-encoding** — only where the pixels actually change.

Output is OME-TIFF by default, or SVS-like BigTIFF with `--openslide` (matches the
source's original compression, and is readable by [OpenSlide](https://openslide.org/)).

¹ Experimental reader. 8-bit brightfield only.
  `--scale <number>` is not supported for VSI yet.
  
² Skipped unless combined with `--scale`, `--icc-bake` or `--roi`.

³ Always re-encoded (NDPI stores each level as one JPEG strip, so tiles cannot be copied).
  Skipped unless combined with `--scale` or `--roi`; full-resolution output requires `--roi`.
  Z-stacks: only the Z=0 (autofocus) plane is converted.

## Desktop app (no command)

If you'd rather not use the terminal, **`thinslide-gui`** does the same thing in a window.

1. Download `thinslide-macos-arm64.dmg` (macOS) or `thinslide-gui-windows-x86_64.exe` (Windows)
   from the [latest release](../../releases/latest).
2. On macOS, open the .dmg and drag **ThinSlide** into Applications. On Windows, just run the .exe.
3. Open it, then choose a folder of slides and a destination folder.
4. Pick what you want, and click **Run**.

![ThinSlide GUI screenshot](assets/gui_screenshot.png)

> **macOS security warning?** Since ThinSlide isn't notarized by Apple, macOS may
> block it on first launch ("cannot be opened because the developer cannot be
> verified"). Go to **System Settings > Privacy & Security**, scroll down to the
> message about ThinSlide, and click **Open Anyway**. Confirm in the dialog that
> follows, then launch ThinSlide again.

## Command line

```sh
thinslide <input_dir> <output_dir> [options]
```

Input and output are directories — ThinSlide processes every slide it finds, mixed formats included.

```sh
# Convert a folder of DICOM slides into OME-TIFF (uses all CPUs)
thinslide /data/dicoms /data/output

# Same, but write SVS-like BigTIFF instead, OpenSlide-compatible
thinslide /data/dicoms /data/output --openslide

# Normalize everything to 20x
thinslide /data/slides /data/output --scale 20x

# Bake ICC profiles into pixels, output sRGB JPEG
thinslide /data/slides /data/output --icc-bake

# Both at once, in a single pass, tuning quality and threads
thinslide /data/slides /data/output --scale 20x --icc-bake --quality 90 -j 4
```

> OME-TIFF inputs keep their original OME-XML metadata through downsampling.

### Installation

See [doc/installation.md](doc/installation.md) for prebuilt binaries and build-from-source instructions.

## Advanced

See [doc/advanced.md](doc/advanced.md) for full details on `--scale` and `--roi`.

## PHI handling

See [doc/phi-handling.md](doc/phi-handling.md) for details.

## Acknowledgments

ThinSlide's CellSens (.vsi) and MIRAX (.mrxs) readers were developed with reference to,
and in part ported from, the following open-source projects:

- [Bio-Formats](https://www.openmicroscopy.org/bio-formats/) (GPLv2) — CellSens VSI format parsing
- [OpenSlide](https://openslide.org/) (LGPL-2.1) — MIRAX format parsing

## License

Copyright (C) 2026 Wataru Uegami, MD, PhD

ThinSlide is licensed under the GNU General Public License v2.0 or later (GPL-2.0-or-later).


## Disclaimer

ThinSlide is provided for **research use only**. It is not a medical device, has not been
cleared or approved by any regulatory authority, and is not intended for clinical diagnosis,
treatment, or any patient-care decision. The software is provided "as is", without warranty
of any kind, to the extent permitted by applicable law.
