// OME-TIFF writer.
//
// IFD layout
// ----------
//   IFD 0  (main chain)   Full resolution; OME-XML in ImageDescription;
//                          TIFFTAG_SUBIFD lists N sub-resolution offsets;
//                          SubFileType = 0.
//   SubIFDs 0 … N-1       Pyramid sub-resolutions chained from IFD 0 via
//                          TIFFTAG_SUBIFD.  libtiff routes the next N calls
//                          to TIFFWriteDirectory() into this chain automatically.
//                          SubFileType = REDUCEDIMAGE.
//   IFD 1+ (main chain)   Optional thumbnail / label / overview images
//                          (stripped JPEG, SubFileType = REDUCEDIMAGE).
//
// Pixel data is copied verbatim from DICOM fragments — no re-encoding.
// The OME-XML conforms to the OME 2016-06 schema and is parsed by BioFormats.

use crate::bindings::{
    TIFFOpen, TIFFSetField, TIFFWriteRawTile, TIFFWriteDirectory, TIFFClose,
    TIFFTAG_YCBCRSUBSAMPLING, TIFFTAG_ICCPROFILE, TIFFTAG_JPEGTABLES,
    TIFFTAG_SUBIFD, TIFFTAG_IMAGEDESCRIPTION,
    PHOTOMETRIC_RGB, PHOTOMETRIC_YCBCR, PHOTOMETRIC_MINISBLACK,
    FILETYPE_REDUCEDIMAGE,
};
use crate::source::dicom::{
    DcmMetadata, ColorSpace,
    tiff_compression_tag, infer_color_space, extract_icc_profile,
    group_by_resolution, frame_to_tile_indices,
};
use crate::pipeline::encode::split_jpeg_to_tables_and_tile;
use crate::pipeline::icc::{IccTransform, build_icc_transform};
use crate::pipeline::ome::generate_dicom_ome_xml;
use std::ffi::CString;
use std::os::raw::c_void;
use std::sync::Arc;
use indicatif::ProgressBar;

pub(crate) fn write_ome_tiff(
    slide_level_metadata_list: &[DcmMetadata],
    _thumbnail_meta: Option<&DcmMetadata>,
    _overview_meta: Option<&DcmMetadata>,
    _label_meta: Option<&DcmMetadata>,
    output_path: &str,
    pb: Option<&ProgressBar>,
    verbose: bool,
    quality: u8,
    icc_bake: bool,
) {
    let groups = group_by_resolution(slide_level_metadata_list);
    let total_tiles: u64 = groups.iter()
        .filter(|g| g[0].tile_size.is_some())
        .map(|g| g.iter().map(|m| m.n_frames.unwrap_or(0) as u64).sum::<u64>())
        .sum();
    if let Some(p) = pb { p.set_length(total_tiles); }

    let ome_xml      = generate_dicom_ome_xml(slide_level_metadata_list);
    let image_desc_c = CString::new(ome_xml).unwrap();

    // Number of sub-resolution levels that will be stored as SubIFDs.
    // Exclude single-tile levels (tile_size == None) as they are skipped below.
    let n_subifds = groups[1..].iter().filter(|g| g[0].tile_size.is_some()).count();

    let output_path_c = CString::new(output_path).unwrap();
    let w8_mode_c     = CString::new("w8").unwrap();
    let tiff = unsafe { TIFFOpen(output_path_c.as_ptr(), w8_mode_c.as_ptr()) }; // BigTIFF
    assert!(!tiff.is_null(), "TIFFOpen failed: cannot create '{}'", output_path);

    // ── IFD 0: Full resolution (main chain) ───────────────────────────
    {
        let group     = &groups[0];
        let metadata  = group[0];
        let first_dcm = dicom::object::open_file(&metadata.file_path).unwrap();

        let ifd_width  = metadata.px_columns.unwrap_or(0);
        let ifd_height = metadata.px_rows.unwrap_or(0);
        let (tile_w, tile_h) = metadata.tile_size.unwrap_or((ifd_width, ifd_height));

        let color_space  = infer_color_space(&first_dcm);
        let icc_profile  = extract_icc_profile(&first_dcm);
        let icc_transform = super::log_icc_and_build_transform(icc_profile.as_deref(), icc_bake, pb, verbose);
        let jp2k_has_ict_rct = first_dcm.element_by_name("PhotometricInterpretation")
            .ok().and_then(|e| e.to_str().ok().map(|s| s.trim().to_string()))
            .map(|s| matches!(s.as_str(), "YBR_ICT" | "YBR_RCT" | "YBR_FULL" | "YBR_FULL_422"))
            .unwrap_or(false);
        let baking = icc_transform.is_some() && !matches!(color_space, ColorSpace::Grayscale);
        let photometric = if baking {
            PHOTOMETRIC_YCBCR
        } else {
            match color_space {
                ColorSpace::RGB       => PHOTOMETRIC_RGB,
                ColorSpace::YCbCr     => PHOTOMETRIC_YCBCR,
                ColorSpace::Grayscale => PHOTOMETRIC_MINISBLACK,
            }
        };
        let spp: u32 = if matches!(color_space, ColorSpace::Grayscale) { 1 } else { 3 };

        let ts_uid = first_dcm.meta().transfer_syntax();
        let compr  = tiff_compression_tag(ts_uid);
        let mpp    = metadata.mpp_x.unwrap_or(0.25);

        if verbose {
            let n_tiles: u64 = group.iter().map(|m| m.n_frames.unwrap_or(0) as u64).sum();
            let tag = if baking { "bake " } else { "pass " };
            crate::vlog(pb, format!("  [{}] lv0  {}x{}  {:.4} µm/px  tile {}x{}  ({} tiles)",
                tag, ifd_width, ifd_height, mpp, tile_w, tile_h, n_tiles));
        }

        // Declare the SubIFD chain BEFORE calling TIFFWriteDirectory.
        // libtiff copies the offset array; we only need it alive until
        // TIFFSetField returns.  The actual offsets are back-patched by
        // libtiff when it writes each SubIFD.
        if n_subifds > 0 {
            let subifd_offsets: Vec<u64> = vec![0u64; n_subifds];
            unsafe { TIFFSetField(tiff, TIFFTAG_SUBIFD,
                n_subifds as u32, subifd_offsets.as_ptr()); }
        }

        unsafe {
            super::set_tiff_ifd_tags(tiff, 0,
                ifd_width, ifd_height, tile_w, tile_h,
                if baking { 7u32 } else { compr }, photometric, spp,
                mpp, mpp);
            // OME-XML in ImageDescription marks this as an OME-TIFF.
            TIFFSetField(tiff, TIFFTAG_IMAGEDESCRIPTION as u32, image_desc_c.as_ptr());
        }

        if baking {
            unsafe { TIFFSetField(tiff, TIFFTAG_YCBCRSUBSAMPLING as u32, 2u32, 2u32); }
        } else if matches!(color_space, ColorSpace::YCbCr) {
            let frags_tmp = super::pixel_fragments(&first_dcm);
            if let Some(frag) = frags_tmp.iter().find(|f| !f.is_empty()) {
                if let Some((h, v)) = crate::detect_jpeg_subsampling(frag) {
                    unsafe { TIFFSetField(tiff, TIFFTAG_YCBCRSUBSAMPLING as u32, h as u32, v as u32); }
                }
            }
        }

        if !baking {
            if let Some(ref icc) = icc_profile {
                unsafe { TIFFSetField(tiff, TIFFTAG_ICCPROFILE as u32,
                    icc.len() as u32, icc.as_ptr() as *const c_void); }
            }
        }

        drop(first_dcm);

        let mut registered_tables_lv0: Option<Vec<u8>> = None;
        for dcm_meta in group.iter() {
            let dicom_obj    = dicom::object::open_file(&dcm_meta.file_path).unwrap();
            let tile_indices = frame_to_tile_indices(&dicom_obj, tile_w, tile_h, ifd_width);
            let fragments    = super::pixel_fragments(&dicom_obj);
            for (fi, fragment) in fragments.iter().enumerate() {
                if !fragment.is_empty() {
                    let tile_num = tile_indices.get(fi).copied().unwrap_or(fi as u32);
                    let baked: Option<Vec<u8>> = if baking {
                        if compr == 7 {
                            icc_transform.as_deref().and_then(|xf| super::bake_jpeg_tile(fragment, xf, quality, crate::tile_align(tile_w, 16) as usize, crate::tile_align(tile_h, 16) as usize))
                        } else {
                            icc_transform.as_deref().and_then(|xf| super::bake_jp2k_tile(fragment, jp2k_has_ict_rct, xf, quality))
                        }
                    } else { None };
                    let src_bytes: &[u8] = baked.as_deref().unwrap_or(fragment.as_slice());
                    let split = (if baking { true } else { compr == 7 })
                        .then(|| split_jpeg_to_tables_and_tile(src_bytes))
                        .flatten();
                    let write_bytes: &[u8] = if let Some((ref tables, ref tile_data)) = split {
                        match registered_tables_lv0 {
                            None => {
                                unsafe { TIFFSetField(tiff, TIFFTAG_JPEGTABLES as u32,
                                    tables.len() as u32, tables.as_ptr()); }
                                registered_tables_lv0 = Some(tables.clone());
                                tile_data.as_slice()
                            }
                            Some(ref rt) if rt == tables => tile_data.as_slice(),
                            // Different DQT/DHT tables — write self-contained JPEG
                            _ => src_bytes,
                        }
                    } else {
                        src_bytes
                    };
                    unsafe { TIFFWriteRawTile(tiff, tile_num,
                        write_bytes.as_ptr() as *mut c_void, write_bytes.len() as i64); }
                }
            }
        }

        // Finalise IFD 0.  libtiff now knows to route the next n_subifds
        // TIFFWriteDirectory calls into the SubIFD chain.
        unsafe { TIFFWriteDirectory(tiff); }
        let ifd0_tiles: u64 = group.iter().map(|m| m.n_frames.unwrap_or(0) as u64).sum();
        if let Some(p) = pb { p.inc(ifd0_tiles); }
    }

    // ── SubIFDs: pyramid sub-resolutions (chained from IFD 0) ─────────
    // libtiff automatically routes the next n_subifds WriteDirectory calls
    // to the SubIFD chain declared above.  No special API call needed here.
    for (sub_idx, group) in groups[1..].iter().enumerate() {
        let metadata  = group[0];
        if metadata.tile_size.is_none() {
            if verbose {
                crate::vlog(pb, format!("  [skip ] lv{}  {}x{}  no tile size",
                    sub_idx + 1,
                    metadata.px_columns.unwrap_or(0),
                    metadata.px_rows.unwrap_or(0)));
            }
            continue;
        }
        let first_dcm = dicom::object::open_file(&metadata.file_path).unwrap();

        let ifd_width  = metadata.px_columns.unwrap_or(0);
        let ifd_height = metadata.px_rows.unwrap_or(0);
        let (tile_w, tile_h) = metadata.tile_size.unwrap_or((ifd_width, ifd_height));

        let color_space = infer_color_space(&first_dcm);
        let jp2k_has_ict_rct_sub = first_dcm.element_by_name("PhotometricInterpretation")
            .ok().and_then(|e| e.to_str().ok().map(|s| s.trim().to_string()))
            .map(|s| matches!(s.as_str(), "YBR_ICT" | "YBR_RCT" | "YBR_FULL" | "YBR_FULL_422"))
            .unwrap_or(false);
        let sub_icc_transform: Option<Arc<IccTransform>> = if icc_bake {
            extract_icc_profile(&first_dcm).and_then(|icc| build_icc_transform(&icc))
        } else {
            None
        };
        let baking_sub = sub_icc_transform.is_some() && !matches!(color_space, ColorSpace::Grayscale);
        let photometric = if baking_sub {
            PHOTOMETRIC_YCBCR
        } else {
            match color_space {
                ColorSpace::RGB       => PHOTOMETRIC_RGB,
                ColorSpace::YCbCr     => PHOTOMETRIC_YCBCR,
                ColorSpace::Grayscale => PHOTOMETRIC_MINISBLACK,
            }
        };
        let spp: u32 = if matches!(color_space, ColorSpace::Grayscale) { 1 } else { 3 };

        let ts_uid = first_dcm.meta().transfer_syntax();
        let compr  = tiff_compression_tag(ts_uid);

        // For JPEG passthrough, libtiff checks that JPEG SOF dimensions match the TIFF tile declaration.
        if !baking_sub && compr == 7 && !super::is_jpeg_tile_aligned(tile_w, tile_h) {
            if verbose {
                crate::vlog(pb, format!("  [skip ] lv{}  {}x{}  tile {}x{} not 16-aligned",
                    sub_idx + 1, ifd_width, ifd_height, tile_w, tile_h));
            }
            drop(first_dcm);
            continue;
        }

        let mpp = metadata.mpp_x.unwrap_or(0.25);

        if verbose {
            let n_tiles: u64 = group.iter().map(|m| m.n_frames.unwrap_or(0) as u64).sum();
            let tag = if baking_sub { "bake " } else { "pass " };
            crate::vlog(pb, format!("  [{}] lv{}  {}x{}  {:.4} µm/px  tile {}x{}  ({} tiles)",
                tag, sub_idx + 1, ifd_width, ifd_height, mpp, tile_w, tile_h, n_tiles));
        }

        unsafe { super::set_tiff_ifd_tags(tiff, FILETYPE_REDUCEDIMAGE,
            ifd_width, ifd_height, tile_w, tile_h,
            if baking_sub { 7u32 } else { compr }, photometric, spp,
            mpp, mpp); }

        if baking_sub {
            unsafe { TIFFSetField(tiff, TIFFTAG_YCBCRSUBSAMPLING as u32, 2u32, 2u32); }
        } else if matches!(color_space, ColorSpace::YCbCr) {
            let frags_tmp = super::pixel_fragments(&first_dcm);
            if let Some(frag) = frags_tmp.iter().find(|f| !f.is_empty()) {
                if let Some((h, v)) = crate::detect_jpeg_subsampling(frag) {
                    unsafe { TIFFSetField(tiff, TIFFTAG_YCBCRSUBSAMPLING as u32, h as u32, v as u32); }
                }
            }
        }
        drop(first_dcm);

        let mut registered_tables_sub: Option<Vec<u8>> = None;
        for dcm_meta in group.iter() {
            let dicom_obj    = dicom::object::open_file(&dcm_meta.file_path).unwrap();
            let tile_indices = frame_to_tile_indices(&dicom_obj, tile_w, tile_h, ifd_width);
            let fragments    = super::pixel_fragments(&dicom_obj);
            for (fi, fragment) in fragments.iter().enumerate() {
                if !fragment.is_empty() {
                    let tile_num = tile_indices.get(fi).copied().unwrap_or(fi as u32);
                    let baked: Option<Vec<u8>> = if baking_sub {
                        if compr == 7 {
                            sub_icc_transform.as_deref().and_then(|xf| super::bake_jpeg_tile(fragment, xf, quality, crate::tile_align(tile_w, 16) as usize, crate::tile_align(tile_h, 16) as usize))
                        } else {
                            sub_icc_transform.as_deref().and_then(|xf| super::bake_jp2k_tile(fragment, jp2k_has_ict_rct_sub, xf, quality))
                        }
                    } else { None };
                    let src_bytes: &[u8] = baked.as_deref().unwrap_or(fragment.as_slice());
                    let split = (if baking_sub { true } else { compr == 7 })
                        .then(|| split_jpeg_to_tables_and_tile(src_bytes))
                        .flatten();
                    let write_bytes: &[u8] = if let Some((ref tables, ref tile_data)) = split {
                        match registered_tables_sub {
                            None => {
                                unsafe { TIFFSetField(tiff, TIFFTAG_JPEGTABLES as u32,
                                    tables.len() as u32, tables.as_ptr()); }
                                registered_tables_sub = Some(tables.clone());
                                tile_data.as_slice()
                            }
                            Some(ref rt) if rt == tables => tile_data.as_slice(),
                            // Different DQT/DHT tables — write self-contained JPEG
                            _ => src_bytes,
                        }
                    } else {
                        src_bytes
                    };
                    unsafe { TIFFWriteRawTile(tiff, tile_num,
                        write_bytes.as_ptr() as *mut c_void, write_bytes.len() as i64); }
                }
            }
        }

        // This call writes into the SubIFD chain while n_subifds > 0,
        // then returns to the main IFD chain.
        unsafe { TIFFWriteDirectory(tiff); }
        let subifd_tiles: u64 = group.iter().map(|m| m.n_frames.unwrap_or(0) as u64).sum();
        if let Some(p) = pb { p.inc(subifd_tiles); }
    }

    unsafe { TIFFClose(tiff); }
}
