# PHI Handling

- **All output formats** — DICOM tags that identify a patient (patient name, ID, birth date,
  accession number, SeriesInstanceUID, etc.) are never copied into the output file's metadata.
  Only non-identifying technical parameters are retained: image dimensions, tile size,
  magnification (MPP), and compression. OME-TIFF additionally embeds the source `Manufacturer`
  in its OME-XML.
- **DICOM input — output filename** — by default the output filename is derived from the
  source SeriesInstanceUID (not from the in-file metadata above). Use `--use-parent-name` to
  name the output after the input folder instead if the UID should not appear in the filename.
- **TIFF / OME-TIFF output** — the label and thumbnail images are dropped from the output.
- **SVS output (`--openslide`)** — the label and thumbnail images are carried over from the
  source DICOM as-is; DICOM PHI tags are still not carried over.
- In all cases, any personal information that is visibly embedded in the tissue region itself
  (as image content, not metadata) is not removed.
