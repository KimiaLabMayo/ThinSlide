# Advanced options

## `--scale <20x|half|quarter|number>`

Pick exactly one downsampling target:

- **`20x`** — auto-detect from source MPP and normalize to 20x scan magnification.
- **`half`** — halve both dimensions unconditionally, without reading source MPP.
  Useful when the source has no resolution metadata (so `20x` would have to skip it).
- **`quarter`** — quarter both dimensions unconditionally, without reading source MPP.
  Same idea as `half`, but a 1/4-scale level is usually already precomputed in the
  source pyramid (unlike 1/2), so this is typically faster.
- **`<number>`** — downsample to an arbitrary resolution (µm/px) instead of normalizing
  to 20x. Always resamples in full, so it is slower than `20x`/`half`/`quarter`.
  Use `--kernel` to pick the resampling kernel.

```sh
thinslide /data/slides /data/output --scale half
thinslide /data/slides /data/output --scale quarter
thinslide /data/slides /data/output --scale 0.5 --kernel lanczos3
```

## `--roi <file.geojson|DIR>`

Keep only the annotated regions (SVS / TIFF, DICOM and VSI input; not MRXS).
Annotations are [QuPath](https://qupath.github.io/) GeoJSON in level-0 pixel coordinates
(`FeatureCollection`, an array of features, a single feature, or a bare geometry;
`Polygon` and `MultiPolygon` are used, holes included).

- Processing stays tile-based: every tile an annotation passes through is kept, and every
  tile completely outside is filled with white.
- The output is cropped to the bounding box of the kept tiles (aligned to the tile grid),
  so a small region gives a small image.
- Tiles outside the annotations are never read, so conversion is faster and the
  output smaller.
- Full-resolution tiles go through the normal pipeline: copied straight through where possible
  (no quality change), re-encoded only where `--scale` or `--icc-bake` changes the pixels.
  The lower pyramid levels are rebuilt from them at 1/4 steps.
- Without `--scale`, the slide is cropped at full resolution.
- **`<file.geojson>`** — applies when the input holds a single slide (a slide file, or a
  folder with one DICOM series or one VSI).
- **`<DIR>`** — each slide is matched to `<DIR>/<name>.geojson`. Slides without a match are
  converted in full. The name is the file name without its extension for SVS / TIFF / VSI
  (e.g. `CMU-1.svs` → `CMU-1.geojson`), and the parent folder name for DICOM
  (e.g. `JP2K-33003-1/DCM_0.dcm` → `JP2K-33003-1.geojson`).
- JPEG 2000 tiles copied straight through are kept in `.ome.tiff`; with `--openslide` they are
  written as `.svs`, since OpenSlide reads JPEG 2000 only from SVS.
- **`--roi-id <ID[,ID...]>`** — use only the features whose top-level `"id"` matches one of
  the comma-separated IDs (e.g. `section-0` in a multi-section `sections.geojson`).
  Multiple IDs are cropped together into one output. A slide whose GeoJSON lacks any listed
  ID fails. Bare geometries (no feature) are ignored.

```sh
thinslide /data/CMU-1.svs /data/output --roi /data/CMU-1.geojson
thinslide /data/slides /data/output --roi /data/annotations --scale 20x
thinslide /data/CMU-3.svs /data/output --roi /data/sections.geojson --roi-id section-0 --scale half
```
