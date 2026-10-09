// --roi writer for DICOM sources whose chosen level can be copied raw (JPEG with
// 16-aligned tiles, or JPEG 2000). The level is cropped to the tile-aligned
// bounding box of the annotations, tiles outside them are filled with white, and
// the levels below are rebuilt from the copied tiles at 1/4 steps.

use crate::bindings::{
    TIFFOpen, TIFFSetField, TIFFWriteRawTile, TIFFWriteDirectory, TIFFClose,
    TIFFTAG_YCBCRSUBSAMPLING, TIFFTAG_ICCPROFILE, TIFFTAG_SUBIFD, TIFFTAG_IMAGEDESCRIPTION,
    PHOTOMETRIC_RGB, PHOTOMETRIC_YCBCR, PHOTOMETRIC_MINISBLACK,
};
use crate::source::dicom::{
    DcmMetadata, ColorSpace,
    tiff_compression_tag, infer_color_space, extract_icc_profile,
    frame_to_tile_indices, map_transfer_syntax_to_compression, is_jpeg2000,
};
use crate::source::tiff::{COMPRESSION_APERIO_JP2_YCBCR, COMPRESSION_APERIO_JP2_RGB};
use crate::pipeline::encode::{split_jpeg_to_tables_and_tile, white_jpeg_tile, white_jp2k_tile};
use crate::roi::{Roi, RoiCrop};
use crate::tiffds::{Reducer, roi_reduced_levels, roi_reduced_tiles, write_reduced_levels};
use std::collections::BTreeMap;
use std::ffi::CString;
use std::os::raw::c_void;
use indicatif::ProgressBar;

/// Writes the cropped `group` level (OME-TIFF if `ome`; otherwise TIFF, or SVS for JPEG 2000).
/// `base` is the level-0 instance the GeoJSON coordinates refer to. Returns the
/// output size, or None (nothing written) if no annotation overlaps the slide.
pub(crate) fn write_roi_passthrough(
    group: &[&DcmMetadata],
    base: &DcmMetadata,
    roi: &Roi,
    output_path: &str,
    ome: bool,
    quality: u8,
    verbose: bool,
    pb: Option<&ProgressBar>,
) -> Option<(u32, u32)> {
    let meta = group[0];
    let (img_w, img_h) = (meta.px_columns.unwrap_or(0), meta.px_rows.unwrap_or(0));
    let (tw, th) = meta.tile_size.unwrap_or((img_w, img_h));
    let grid = (img_w.div_ceil(tw), img_h.div_ceil(th));
    let base_dim = (base.px_columns.unwrap_or(img_w), base.px_rows.unwrap_or(img_h));
    let crop = RoiCrop::from_mask(&roi.tile_mask(grid, (tw, th), (img_w, img_h), base_dim), grid)?;
    let (out_w, out_h) = crop.dim((img_w, img_h), (tw, th));
    let roi_levels = roi_reduced_levels((out_w, out_h));

    let dcm0 = dicom::object::open_file(&meta.file_path).unwrap();
    let color_space = infer_color_space(&dcm0);
    let icc_profile = extract_icc_profile(&dcm0);
    let ts_uid = dcm0.meta().transfer_syntax().to_string();
    let is_jp2 = is_jpeg2000(&map_transfer_syntax_to_compression(&ts_uid));
    let photometric_interp = dcm0.element_by_name("PhotometricInterpretation")
        .ok()
        .and_then(|e| e.to_str().ok().map(|s| s.trim().to_string()))
        .unwrap_or_default();
    let jp2k_has_ict_rct = matches!(photometric_interp.as_str(), "YBR_ICT" | "YBR_RCT" | "YBR_FULL" | "YBR_FULL_422");
    let spp: u32 = if matches!(color_space, ColorSpace::Grayscale) { 1 } else { 3 };
    // Same compression/photometric mapping as write_svs (JP2K SVS) / write_ome_tiff passthrough.
    let (compression, photometric) = if is_jp2 && !ome {
        if jp2k_has_ict_rct { (COMPRESSION_APERIO_JP2_YCBCR, PHOTOMETRIC_YCBCR as u32) }
        else if spp == 1 { (COMPRESSION_APERIO_JP2_RGB, PHOTOMETRIC_MINISBLACK as u32) }
        else { (COMPRESSION_APERIO_JP2_RGB, PHOTOMETRIC_RGB as u32) }
    } else {
        (tiff_compression_tag(&ts_uid), match color_space {
            ColorSpace::RGB       => PHOTOMETRIC_RGB as u32,
            ColorSpace::YCbCr     => PHOTOMETRIC_YCBCR as u32,
            ColorSpace::Grayscale => PHOTOMETRIC_MINISBLACK as u32,
        })
    };
    let subsamp: (u16, u16) = if !is_jp2 && photometric == PHOTOMETRIC_YCBCR as u32 {
        super::pixel_fragments(&dcm0).iter().find(|f| !f.is_empty())
            .and_then(|f| crate::detect_jpeg_subsampling(f)).unwrap_or((2, 2))
    } else { (2, 2) };
    drop(dcm0);

    let mpp_x = meta.mpp_x.unwrap_or(0.0);
    let mpp_y = meta.mpp_y.unwrap_or(mpp_x);
    if verbose {
        crate::vlog(pb, format!("  [roi  ] crop tiles {}x{} at ({}, {}) → {}x{}  {}/{} tiles inside annotations",
            crop.cols, crop.rows, crop.c0, crop.r0, out_w, out_h,
            crop.mask.iter().filter(|&&b| b).count(), crop.mask.len()));
        crate::vlog(pb, format!("  [pass ] lv0  {}x{}  {:.4} µm/px  tile {}x{}", out_w, out_h, mpp_x, tw, th));
    }

    // Collect the source tiles inside the crop, keyed by cropped tile id so they
    // can be written (and fed to the reducer) in tile order.
    let mut tiles: BTreeMap<u32, Vec<u8>> = BTreeMap::new();
    for dcm_meta in group {
        let dicom_obj = dicom::object::open_file(&dcm_meta.file_path).unwrap();
        let tile_indices = frame_to_tile_indices(&dicom_obj, tw, th, img_w);
        for (fi, frag) in super::pixel_fragments(&dicom_obj).iter().enumerate() {
            if frag.is_empty() { continue; }
            let full = tile_indices.get(fi).copied().unwrap_or(fi as u32);
            let (c, r) = (full % grid.0, full / grid.0);
            if c < crop.c0 || r < crop.r0 || c >= crop.c0 + crop.cols || r >= crop.r0 + crop.rows { continue; }
            let id = (r - crop.r0) * crop.cols + (c - crop.c0);
            if crop.mask[id as usize] { tiles.insert(id, frag.clone()); }
        }
    }

    let white = if is_jp2 {
        white_jp2k_tile(tw, th, spp, jp2k_has_ict_rct).expect("white JP2K tile encode failed")
    } else {
        white_jpeg_tile(tw, th, spp, photometric == PHOTOMETRIC_RGB as u32, subsamp, quality)
    };
    let mut reducer = Reducer::new((crop.cols, crop.rows), (tw, th), spp, quality, None, false,
        jp2k_has_ict_rct || color_space == ColorSpace::YCbCr);

    if let Some(p) = pb {
        p.set_length((crop.cols * crop.rows) as u64 + roi_reduced_tiles((crop.cols, crop.rows), roi_levels));
    }

    let path_c = CString::new(output_path).unwrap();
    let w8_mode = CString::new("w8").unwrap();
    let tiff = unsafe { TIFFOpen(path_c.as_ptr(), w8_mode.as_ptr()) };
    assert!(!tiff.is_null(), "TIFFOpen failed: cannot create '{}'", output_path);

    let image_desc = if is_jp2 && !ome {
        let comp_desc = if compression == COMPRESSION_APERIO_JP2_YCBCR { "J2K/YCB" } else { "J2K/RGB" };
        let mag = if std::ptr::eq(meta, base) { meta.objective_power } else { None }
            .unwrap_or_else(|| if mpp_x > 0.0 { (10.0 / mpp_x).round() } else { 0.0 });
        Some(format!("Aperio Image Library (DICOM converted)\n\
            {out_w}x{out_h} [0,0 {out_w}x{out_h}] ({tw}x{th}) {comp_desc}|AppMag = {mag:.0}|MPP = {mpp_x:.6}"))
    } else if ome {
        let mut m = meta.clone();
        (m.px_columns, m.px_rows) = (Some(out_w), Some(out_h));
        Some(crate::pipeline::ome::generate_dicom_ome_xml(&[m]))
    } else {
        None
    };
    // SVS keeps every level as a top-level IFD; OME-TIFF stores them as SubIFDs.
    if ome && roi_levels > 0 {
        let zeros: Vec<u64> = vec![0u64; roi_levels as usize];
        unsafe { TIFFSetField(tiff, TIFFTAG_SUBIFD, roi_levels, zeros.as_ptr()); }
    }

    unsafe {
        super::set_tiff_ifd_tags(tiff, 0, out_w, out_h, tw, th,
            compression, photometric, spp, mpp_x, mpp_y);
        if !is_jp2 && photometric == PHOTOMETRIC_YCBCR as u32 {
            TIFFSetField(tiff, TIFFTAG_YCBCRSUBSAMPLING as u32, subsamp.0 as u32, subsamp.1 as u32);
        }
        if let Some(desc) = image_desc {
            let desc_c = CString::new(desc).unwrap();
            TIFFSetField(tiff, TIFFTAG_IMAGEDESCRIPTION as u32, desc_c.as_ptr());
        }
        if let Some(ref icc) = icc_profile {
            TIFFSetField(tiff, TIFFTAG_ICCPROFILE as u32, icc.len() as u32, icc.as_ptr() as *const c_void);
        }
    }

    // White tiles are self-contained streams, written as-is so they never claim
    // the level's JPEGTABLES.
    let mut registered_tables: Option<Vec<u8>> = None;
    for id in 0..crop.cols * crop.rows {
        let in_roi = crop.mask[id as usize];
        let data = if in_roi { tiles.remove(&id) } else { Some(white.clone()) };
        let Some(data) = data else {
            if let Some(p) = pb { p.inc(1); }
            continue;
        };
        let split = (!is_jp2 && in_roi).then(|| split_jpeg_to_tables_and_tile(&data)).flatten();
        let write_bytes: &[u8] = match (&split, &registered_tables) {
            (Some((tables, tile_data)), None) => {
                super::set_jpeg_tables(tiff, tables);
                registered_tables = Some(tables.clone());
                tile_data
            }
            (Some((tables, tile_data)), Some(rt)) if rt == tables => tile_data,
            _ => &data,
        };
        unsafe { TIFFWriteRawTile(tiff, id, write_bytes.as_ptr() as *mut c_void, write_bytes.len() as i64); }
        reducer.push(id, &data);
        if let Some(p) = pb { p.inc(1); }
    }
    unsafe { TIFFWriteDirectory(tiff); }

    unsafe {
        write_reduced_levels(tiff, reducer, (out_w, out_h), (mpp_x, mpp_y), roi_levels, verbose, pb);
        TIFFClose(tiff);
    }
    Some((out_w, out_h))
}
