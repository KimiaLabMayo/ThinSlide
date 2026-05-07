// SVS writer (JPEG 2000-compressed DICOM → Aperio SVS).
//
// SVS IFD order (required by OpenSlide):
//   IFD 0:         Full resolution pyramid level (largest), tiled, SubFileType=0
//   IFD 1:         Thumbnail (stripped JPEG, small), SubFileType=1
//   IFDs 2..N:     Remaining pyramid levels (descending), tiled, SubFileType=1
//   IFD N+1:       Label image (stripped JPEG), SubFileType=1   [optional]
//   IFD N+2:       Macro/Overview image (stripped JPEG), SubFileType=9 [optional]

use crate::bindings::{
    TIFF, TIFFOpen, TIFFSetField, TIFFWriteRawTile, TIFFWriteRawStrip, TIFFWriteDirectory, TIFFClose,
    TIFFTAG_SUBFILETYPE, TIFFTAG_IMAGEWIDTH, TIFFTAG_IMAGELENGTH,
    TIFFTAG_COMPRESSION, TIFFTAG_PHOTOMETRIC, TIFFTAG_SAMPLESPERPIXEL,
    TIFFTAG_BITSPERSAMPLE, TIFFTAG_PLANARCONFIG,
    PHOTOMETRIC_RGB, PHOTOMETRIC_YCBCR, PHOTOMETRIC_MINISBLACK,
    TIFFTAG_IMAGEDESCRIPTION, TIFFTAG_YCBCRSUBSAMPLING, TIFFTAG_ICCPROFILE, TIFFTAG_JPEGTABLES,
    PLANARCONFIG_CONTIG, TIFFTAG_ROWSPERSTRIP,
    FILETYPE_REDUCEDIMAGE,
};
use crate::source::dicom::{
    DcmMetadata, ColorSpace,
    infer_color_space, extract_icc_profile,
    frame_to_tile_indices, map_transfer_syntax_to_compression, is_jpeg2000,
};
use crate::source::tiff::{COMPRESSION_APERIO_JP2_YCBCR, COMPRESSION_APERIO_JP2_RGB};
use crate::pipeline::encode::split_jpeg_to_tables_and_tile;
use std::ffi::CString;
use std::os::raw::c_void;
use indicatif::ProgressBar;
use rayon::prelude::*;

/// Write one J2K-tiled pyramid level IFD.
/// `image_desc` is only set for IFD 0 (full resolution).
unsafe fn write_svs_tiled_level(
    tiff: *mut TIFF,
    metadata: &DcmMetadata,
    svs_compression: u32,
    photometric: u32,
    res_x: f64,
    res_y: f64,
    subfile_type: u32,
    image_desc: Option<&CString>,
    icc_transform: Option<&crate::IccTransform>,
    quality: u8,
    jp2k_has_ict_rct: bool,
) { unsafe {
    let dicom_obj  = dicom::object::open_file(&metadata.file_path).unwrap();
    let fragments  = super::pixel_fragments(&dicom_obj);

    let ifd_width  = metadata.px_columns.unwrap_or(0);
    let ifd_height = metadata.px_rows.unwrap_or(0);
    let (tile_w, tile_h) = metadata.tile_size.unwrap_or((ifd_width, ifd_height));

    // When ICC baking, override compression to standard JPEG (7) and photometric to YCBCR.
    let baking = icc_transform.is_some();
    let out_compression = if baking { 7u32 } else { svs_compression };
    let out_photometric = if baking { PHOTOMETRIC_YCBCR as u32 } else { photometric };

    super::set_tiff_ifd_tags(tiff, subfile_type,
        ifd_width, ifd_height, tile_w, tile_h,
        out_compression, out_photometric, 3,
        1e4 / res_x, 1e4 / res_y);

    if let Some(desc) = image_desc {
        TIFFSetField(tiff, TIFFTAG_IMAGEDESCRIPTION as u32, desc.as_ptr());
    }

    if baking {
        // Baked tiles are re-encoded with turbojpeg Sub2x2 (4:2:0).
        TIFFSetField(tiff, TIFFTAG_YCBCRSUBSAMPLING as u32, 2u32, 2u32);
    } else if out_photometric == PHOTOMETRIC_YCBCR as u32 && svs_compression == 7 {
        if let Some(first) = fragments.iter().find(|f| !f.is_empty()) {
            if let Some((h, v)) = crate::detect_jpeg_subsampling(first) {
                TIFFSetField(tiff, TIFFTAG_YCBCRSUBSAMPLING as u32, h as u32, v as u32);
            }
        }
    }

    let tile_indices = frame_to_tile_indices(&dicom_obj, tile_w, tile_h, ifd_width);

    if baking {
        // Encode all tiles in parallel, then write serially.
        let xf = icc_transform.unwrap();
        let frags: Vec<(u32, &[u8])> = fragments.iter().enumerate()
            .filter(|(_, f)| !f.is_empty())
            .map(|(i, f)| (tile_indices.get(i).copied().unwrap_or(i as u32), f.as_slice()))
            .collect();
        let mut encoded: Vec<(u32, Vec<u8>)> = frags.par_iter()
            .filter_map(|(tile_num, frag)| {
                let jpeg = if svs_compression == 7 {
                    super::bake_jpeg_tile(frag, xf, quality,
                        crate::tile_align(tile_w, 16) as usize, crate::tile_align(tile_h, 16) as usize)
                } else {
                    super::bake_jp2k_tile(frag, jp2k_has_ict_rct, xf, quality)
                }?;
                Some((*tile_num, jpeg))
            })
            .collect();
        encoded.sort_unstable_by_key(|(n, _)| *n);
        let mut registered_tables: Option<Vec<u8>> = None;
        for (tile_num, jpeg) in &encoded {
            let split = split_jpeg_to_tables_and_tile(jpeg);
            let write_bytes: &[u8] = if let Some((ref tables, ref tile_data)) = split {
                match registered_tables {
                    None => {
                        TIFFSetField(tiff, TIFFTAG_JPEGTABLES as u32,
                            tables.len() as u32, tables.as_ptr());
                        registered_tables = Some(tables.clone());
                        tile_data.as_slice()
                    }
                    Some(ref rt) if rt == tables => tile_data.as_slice(),
                    _ => jpeg.as_slice(),
                }
            } else {
                jpeg.as_slice()
            };
            TIFFWriteRawTile(tiff, *tile_num,
                write_bytes.as_ptr() as *mut c_void, write_bytes.len() as i64);
        }
    } else {
        // Passthrough: no per-tile CPU work, stay sequential.
        let mut registered_tables: Option<Vec<u8>> = None;
        for (i, fragment) in fragments.iter().enumerate() {
            if fragment.is_empty() { continue; }
            let tile_num = tile_indices.get(i).copied().unwrap_or(i as u32);
            let split = (out_compression == 7)
                .then(|| split_jpeg_to_tables_and_tile(fragment))
                .flatten();
            let write_bytes: &[u8] = if let Some((ref tables, ref tile_data)) = split {
                match registered_tables {
                    None => {
                        TIFFSetField(tiff, TIFFTAG_JPEGTABLES as u32,
                            tables.len() as u32, tables.as_ptr());
                        registered_tables = Some(tables.clone());
                        tile_data.as_slice()
                    }
                    Some(ref rt) if rt == tables => tile_data.as_slice(),
                    _ => fragment.as_slice(),
                }
            } else {
                fragment.as_slice()
            };
            TIFFWriteRawTile(tiff, tile_num,
                write_bytes.as_ptr() as *mut c_void, write_bytes.len() as i64);
        }
    }
}}

/// Write a stripped JPEG IFD (thumbnail / label / macro).
/// `jpeg_bytes` must be a complete, self-contained JPEG byte stream.
unsafe fn write_svs_stripped_jpeg(
    tiff: *mut TIFF,
    jpeg_bytes: &[u8],
    width: u32,
    height: u32,
    subfile_type: u32,
) { unsafe {
    TIFFSetField(tiff, TIFFTAG_SUBFILETYPE as u32,     subfile_type);
    TIFFSetField(tiff, TIFFTAG_IMAGEWIDTH as u32,      width);
    TIFFSetField(tiff, TIFFTAG_IMAGELENGTH as u32,     height);
    TIFFSetField(tiff, TIFFTAG_COMPRESSION as u32,     7u32); // JPEG
    TIFFSetField(tiff, TIFFTAG_PHOTOMETRIC as u32,     PHOTOMETRIC_YCBCR as u32);
    TIFFSetField(tiff, TIFFTAG_SAMPLESPERPIXEL as u32, 3u32);
    TIFFSetField(tiff, TIFFTAG_BITSPERSAMPLE as u32,   8u32);
    TIFFSetField(tiff, TIFFTAG_PLANARCONFIG as u32,    PLANARCONFIG_CONTIG as u32);
    TIFFSetField(tiff, TIFFTAG_ROWSPERSTRIP as u32,    height);
    TIFFWriteRawStrip(
        tiff, 0,
        jpeg_bytes.as_ptr() as *mut c_void,
        jpeg_bytes.len() as i64,
    );
}}

pub(crate) fn write_svs(
    slide_levels: &[DcmMetadata],
    thumbnail_meta: Option<&DcmMetadata>,
    label_meta: Option<&DcmMetadata>,
    overview_meta: Option<&DcmMetadata>,
    output_path: &str,
    pb: Option<&ProgressBar>,
    verbose: bool,
    quality: u8,
    icc_bake: bool,
) {
    // Determine SVS compression code and photometric from the full-res level.
    // For JPEG2000 source use Aperio proprietary codes; for JPEG use standard 7.
    //
    // Aperio codes for JPEG2000:
    //   33003 (YCBCR): OpenSlide decodes J2K then applies YCbCr→RGB conversion.
    //   33005 (RGB):   OpenSlide decodes J2K and treats output as-is (no conversion).
    //
    // DICOM YBR_ICT/YBR_RCT data has ICT applied in the J2K stream.
    // OpenSlide does not reverse ICT when using 33005, so the decoded values
    // remain in YCbCr space and colors appear wrong.
    // Using 33003 tells OpenSlide to apply YCbCr→RGB, which correctly compensates.
    let base = &slide_levels[0];
    let dcm0 = dicom::object::open_file(&base.file_path).unwrap();
    let color_space = infer_color_space(&dcm0);
    let icc_profile   = extract_icc_profile(&dcm0);
    let icc_transform = super::log_icc_and_build_transform(icc_profile.as_deref(), icc_bake, pb, verbose);
    let ts_uid = dcm0.meta().transfer_syntax();
    let is_jp2 = is_jpeg2000(&map_transfer_syntax_to_compression(ts_uid));
    // Read raw PhotometricInterpretation to distinguish YBR_ICT/YBR_RCT from RGB.
    let photometric_interp = dcm0.element_by_name("PhotometricInterpretation")
        .ok()
        .and_then(|e| e.to_str().ok().map(|s| s.trim().to_string()))
        .unwrap_or_default();
    let jp2k_has_ict_rct = matches!(photometric_interp.as_str(), "YBR_ICT" | "YBR_RCT" | "YBR_FULL" | "YBR_FULL_422");
    let (svs_compression, photometric) = match (is_jp2, photometric_interp.as_str()) {
        (true,  "YBR_ICT") | (true, "YBR_RCT")
                              => (COMPRESSION_APERIO_JP2_YCBCR, PHOTOMETRIC_YCBCR as u32),
        (true,  "YBR_FULL") | (true, "YBR_FULL_422")
                              => (COMPRESSION_APERIO_JP2_YCBCR, PHOTOMETRIC_YCBCR as u32),
        (true,  _) if matches!(color_space, ColorSpace::Grayscale)
                              => (COMPRESSION_APERIO_JP2_RGB,   PHOTOMETRIC_MINISBLACK as u32),
        (true,  _)            => (COMPRESSION_APERIO_JP2_RGB,   PHOTOMETRIC_RGB as u32),
        (false, _) if matches!(color_space, ColorSpace::YCbCr)
                              => (7u32,                         PHOTOMETRIC_YCBCR as u32),
        (false, _) if matches!(color_space, ColorSpace::Grayscale)
                              => (7u32,                         PHOTOMETRIC_MINISBLACK as u32),
        (false, _)            => (7u32,                         PHOTOMETRIC_RGB as u32),
    };

    // Base resolution
    let base_mpp_x = base.mpp_x.unwrap_or(0.25);
    let base_mpp_y = base.mpp_y.unwrap_or(base_mpp_x);
    let base_res_x = 1e4 / base_mpp_x; // pixels/cm
    let base_res_y = 1e4 / base_mpp_y;

    // Build Aperio ImageDescription for IFD 0
    let img_w      = base.px_columns.unwrap_or(0);
    let img_h      = base.px_rows.unwrap_or(0);
    let (tile_w, tile_h) = base.tile_size.unwrap_or((256, 256));
    let mag        = base.objective_power.unwrap_or_else(|| {
        // Estimate from MPP: 40x ≈ 0.25 µm/px
        (0.25 / base_mpp_x * 40.0).round()
    });
    let image_desc = format!(
        "Aperio Image Library (DICOM converted)\n\
         {}x{} ({}x{}) JPEG2000|AppMag={:.0}|MPP={:.6}",
        img_w, img_h, tile_w, tile_h, mag, base_mpp_x
    );
    let image_desc_c = CString::new(image_desc).unwrap();

    let total_tiles: u64 = {
        let base_tiles = slide_levels[0].n_frames.unwrap_or(0) as u64;
        let other_tiles: u64 = slide_levels[1..].iter().map(|level| {
            if level.tile_size.is_none() { return 0; }
            let (tile_w, tile_h) = level.tile_size.unwrap();
            if svs_compression == 7 && !super::is_jpeg_tile_aligned(tile_w, tile_h) { return 0; }
            level.n_frames.unwrap_or(0) as u64
        }).sum();
        base_tiles + other_tiles
            + thumbnail_meta.is_some() as u64
            + label_meta.is_some() as u64
            + overview_meta.is_some() as u64
    };
    if let Some(p) = pb { p.set_length(total_tiles); }

    let output_path_c = CString::new(output_path).unwrap();
    let w8_mode_c     = CString::new("w8").unwrap();
    let tiff = unsafe { TIFFOpen(output_path_c.as_ptr(), w8_mode_c.as_ptr()) };
    assert!(!tiff.is_null(), "TIFFOpen failed: cannot create '{}'", output_path);

    // ── IFD 0: Full resolution ────────────────────────────────────────
    if verbose {
        let (btw, bth) = base.tile_size.unwrap_or((256, 256));
        crate::vlog(pb, format!("  [pass ] lv0  {}x{}  {:.4} µm/px  tile {}x{}  ({} tiles)",
            img_w, img_h, base_mpp_x, btw, bth,
            base.n_frames.unwrap_or(0)));
    }
    unsafe { write_svs_tiled_level(
        tiff, base,
        svs_compression, photometric,
        base_res_x, base_res_y,
        0,  // SubFileType: full image
        Some(&image_desc_c),
        icc_transform.as_deref(),
        quality,
        jp2k_has_ict_rct,
    ); }
    if !icc_bake {
        if let Some(ref icc) = icc_profile {
            unsafe { TIFFSetField(tiff, TIFFTAG_ICCPROFILE as u32,
                icc.len() as u32, icc.as_ptr() as *const c_void); }
        }
    }
    unsafe { TIFFWriteDirectory(tiff); }
    if let Some(p) = pb { p.inc(base.n_frames.unwrap_or(0) as u64); }

    // ── IFD 1: Thumbnail (decoded + re-encoded as JPEG) ──────────────
    let thumb_written = thumbnail_meta
        .and_then(|m| super::decode_frame_as_jpeg(&m.file_path, 0, 90))
        .map(|(jpeg, w, h)| {
            unsafe { write_svs_stripped_jpeg(tiff, &jpeg, w, h, FILETYPE_REDUCEDIMAGE); }
            unsafe { TIFFWriteDirectory(tiff); }
            if let Some(p) = pb { p.inc(1); }
        });
    let _ = thumb_written;

    // ── IFDs 2..N: Remaining pyramid levels ───────────────────────────
    let base_cols = base.px_columns.unwrap_or(1) as f64;
    for (_i, level) in slide_levels[1..].iter().enumerate() {
        if level.tile_size.is_none() {
            if verbose {
                crate::vlog(pb, format!("  [skip ] lv{}  {}x{}  no tile size",
                    _i + 1,
                    level.px_columns.unwrap_or(0),
                    level.px_rows.unwrap_or(0)));
            }
            continue;
        }

        let (lvl_tile_w, lvl_tile_h) = level.tile_size.unwrap();
        // For JPEG passthrough without baking, libtiff rejects non-16-aligned tiles.
        if !icc_bake && svs_compression == 7 && !super::is_jpeg_tile_aligned(lvl_tile_w, lvl_tile_h) {
            if verbose {
                crate::vlog(pb, format!("  [skip ] lv{}  {}x{}  tile {}x{} not 16-aligned",
                    _i + 1, level.px_columns.unwrap_or(0), level.px_rows.unwrap_or(0),
                    lvl_tile_w, lvl_tile_h));
            }
            continue;
        }
        let ds = base_cols / level.px_columns.unwrap_or(1) as f64;
        let lv_mpp = base_mpp_x * ds;
        if verbose {
            let tag = if icc_bake { "bake " } else { "pass " };
            crate::vlog(pb, format!("  [{}] lv{}  {}x{}  {:.4} µm/px  tile {}x{}  ({} tiles)",
                tag, _i + 1,
                level.px_columns.unwrap_or(0), level.px_rows.unwrap_or(0),
                lv_mpp, lvl_tile_w, lvl_tile_h,
                level.n_frames.unwrap_or(0)));
        }
        unsafe { write_svs_tiled_level(
            tiff, level,
            svs_compression, photometric,
            base_res_x / ds, base_res_y / ds,
            FILETYPE_REDUCEDIMAGE,
            None,
            icc_transform.as_deref(),
            quality,
            jp2k_has_ict_rct,
        ); }
        unsafe { TIFFWriteDirectory(tiff); }
        if let Some(p) = pb { p.inc(level.n_frames.unwrap_or(0) as u64); }
    }

    // ── Label image ───────────────────────────────────────────────────
    if let Some(m) = label_meta {
        if let Some((jpeg, w, h)) = super::decode_frame_as_jpeg(&m.file_path, 0, 90) {
            unsafe { write_svs_stripped_jpeg(tiff, &jpeg, w, h, FILETYPE_REDUCEDIMAGE); }
            unsafe { TIFFWriteDirectory(tiff); }
            if let Some(p) = pb { p.inc(1); }
        }
    }

    // ── Macro / Overview image ────────────────────────────────────────
    // SubFileType 9 = macro image (Aperio convention, SubFileType bit 3 = 0x8)
    if let Some(m) = overview_meta {
        if let Some((jpeg, w, h)) = super::decode_frame_as_jpeg(&m.file_path, 0, 90) {
            unsafe { write_svs_stripped_jpeg(tiff, &jpeg, w, h, 9); }
            unsafe { TIFFWriteDirectory(tiff); }
            if let Some(p) = pb { p.inc(1); }
        }
    }

    unsafe { TIFFClose(tiff); }
}
