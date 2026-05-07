use crate::bindings::{
    TIFFOpen, TIFFSetField, TIFFWriteRawTile, TIFFWriteDirectory, TIFFClose,
    TIFFTAG_YCBCRSUBSAMPLING, TIFFTAG_ICCPROFILE, TIFFTAG_JPEGTABLES,
    PHOTOMETRIC_RGB, PHOTOMETRIC_YCBCR, PHOTOMETRIC_MINISBLACK,
    FILETYPE_REDUCEDIMAGE,
};
use crate::source::dicom::{
    DcmMetadata, CompressionType, ColorSpace,
    tiff_compression_tag, infer_color_space, extract_icc_profile,
    group_by_resolution, frame_to_tile_indices, map_transfer_syntax_to_compression,
};
use crate::pipeline::encode::split_jpeg_to_tables_and_tile;
use std::ffi::CString;
use std::os::raw::c_void;
use indicatif::ProgressBar;
use rayon::prelude::*;

pub(crate) fn write_flat_multipage_tiff(
    slide_level_metadata_list: &[DcmMetadata],
    output_path: &str,
    pb: Option<&ProgressBar>,
    verbose: bool,
    quality: u8,
    icc_bake: bool,
) {
    let groups = group_by_resolution(slide_level_metadata_list);

    // Extract ICC profile before activating the progress bar so the message isn't overwritten.
    // (group_idx == 0 may be skipped if tile_size is None or tiles aren't JPEG-aligned)
    let icc_profile: Option<Vec<u8>> = groups.iter().find_map(|g| {
        let meta = g[0];
        let Some((tw, th)) = meta.tile_size else { return None; };
        let dcm = dicom::object::open_file(&meta.file_path).ok()?;
        let ts  = dcm.meta().transfer_syntax();
        let cmp: u32 = match map_transfer_syntax_to_compression(ts) {
            CompressionType::JpegBaseline
            | CompressionType::JpegExtended
            | CompressionType::JpegLossless
            | CompressionType::JpegLosslessNonHierarchical => 7,
            _ => 0,
        };
        if cmp == 7 && !super::is_jpeg_tile_aligned(tw, th) { return None; }
        extract_icc_profile(&dcm)
    });
    let icc_transform = super::log_icc_and_build_transform(icc_profile.as_deref(), icc_bake, pb, verbose);

    let total_tiles: u64 = groups.iter()
        .filter(|g| g[0].tile_size.is_some())
        .map(|g| g.iter().map(|m| m.n_frames.unwrap_or(0) as u64).sum::<u64>())
        .sum();
    if let Some(p) = pb { p.set_length(total_tiles); }

    let output_path_c = CString::new(output_path).unwrap();
    let w8_mode_c     = CString::new("w8").unwrap();
    let tiff = unsafe { TIFFOpen(output_path_c.as_ptr(), w8_mode_c.as_ptr()) };
    assert!(!tiff.is_null(), "TIFFOpen failed: cannot create '{}'", output_path);

    let mut icc_written = false;
    for (group_idx, group) in groups.iter().enumerate() {
        let metadata  = group[0];
        // Skip resolution levels where the entire image fits in a single tile
        // (tile_size == None means tile dimensions equal image dimensions).
        // Such levels provide no pyramid benefit and can have non-16-aligned
        // dimensions that libtiff rejects for JPEG tiles.
        if metadata.tile_size.is_none() {
            if verbose {
                crate::vlog(pb, format!("  [skip ] lv{}  {}x{}  no tile size",
                    group_idx,
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
        let baking = icc_transform.is_some() && !matches!(color_space, ColorSpace::Grayscale);
        let photometric = if baking {
            PHOTOMETRIC_YCBCR  // turbojpeg re-encodes as YCbCr JFIF
        } else {
            match color_space {
                ColorSpace::RGB       => PHOTOMETRIC_RGB,
                ColorSpace::YCbCr     => PHOTOMETRIC_YCBCR,
                ColorSpace::Grayscale => PHOTOMETRIC_MINISBLACK,
            }
        };

        let ts_uid      = first_dcm.meta().transfer_syntax();
        let compression = tiff_compression_tag(ts_uid);

        // For JPEG passthrough, libtiff checks that the JPEG SOF header dimensions match the TIFF tile declaration.
        // If tiles are not multiples of 16, this mismatch causes an error, so we skip these levels.
        if compression == 7 && !super::is_jpeg_tile_aligned(tile_w, tile_h) {
            if verbose {
                crate::vlog(pb, format!("  [skip ] lv{}  {}x{}  tile {}x{} not 16-aligned",
                    group_idx, ifd_width, ifd_height, tile_w, tile_h));
            }
            continue;
        }

        let mpp = metadata.mpp_x.unwrap_or(0.25);

        let n_tiles: u64 = group.iter().map(|m| m.n_frames.unwrap_or(0) as u64).sum();
        if verbose {
            let tag = if baking { "bake " } else { "pass " };
            crate::vlog(pb, format!("  [{}] lv{}  {}x{}  {:.4} µm/px  tile {}x{}  ({} tiles)",
                tag, group_idx, ifd_width, ifd_height, mpp, tile_w, tile_h, n_tiles));
        }

        let subfile_type: u32 = if group_idx == 0 { 0 } else { FILETYPE_REDUCEDIMAGE };
        unsafe { super::set_tiff_ifd_tags(tiff, subfile_type,
            ifd_width, ifd_height, tile_w, tile_h,
            compression, photometric, 3,
            mpp, mpp); }

        if baking {
            // Baked tiles are re-encoded with turbojpeg Sub2x2 (4:2:0).
            unsafe { TIFFSetField(tiff, TIFFTAG_YCBCRSUBSAMPLING as u32, 2u32, 2u32); }
        } else if matches!(color_space, ColorSpace::YCbCr) {
            // For YCbCr JPEG passthrough, detect actual subsampling from the first tile so
            // the TIFF tag matches the JPEG payload (otherwise libtiff defaults
            // to [2,2] regardless of the stream content, confusing QuPath).
            let frags_tmp = super::pixel_fragments(&first_dcm);
            if let Some(first) = frags_tmp.iter().find(|f| !f.is_empty()) {
                if let Some((h, v)) = crate::detect_jpeg_subsampling(first) {
                    unsafe { TIFFSetField(tiff, TIFFTAG_YCBCRSUBSAMPLING as u32, h as u32, v as u32); }
                }
            }
        }

        if !icc_written {
            if !baking {
                if let Some(ref icc) = icc_profile {
                    unsafe { TIFFSetField(tiff, TIFFTAG_ICCPROFILE as u32,
                        icc.len() as u32, icc.as_ptr() as *const c_void); }
                }
            }
            icc_written = true;
        }

        drop(first_dcm);

        // Write tiles from every DICOM file in this resolution group.
        // When baking: encode tiles in parallel per-file, then write serially.
        // Otherwise: passthrough raw bytes with JPEGTABLES optimisation.
        let mut registered_tables: Option<Vec<u8>> = None;
        for dcm_meta in group.iter() {
            let dicom_obj    = dicom::object::open_file(&dcm_meta.file_path).unwrap();
            let tile_indices = frame_to_tile_indices(&dicom_obj, tile_w, tile_h, ifd_width);
            let fragments    = super::pixel_fragments(&dicom_obj);

            if baking && compression == 7 {
                let xf = icc_transform.as_deref().unwrap();
                let frags: Vec<(u32, &[u8])> = fragments.iter().enumerate()
                    .filter(|(_, f)| !f.is_empty())
                    .map(|(i, f)| (tile_indices.get(i).copied().unwrap_or(i as u32), f.as_slice()))
                    .collect();
                let mut encoded: Vec<(u32, Vec<u8>)> = frags.par_iter()
                    .filter_map(|(tile_num, frag)| {
                        let jpeg = super::bake_jpeg_tile(frag, xf, quality,
                            crate::tile_align(tile_w, 16) as usize, crate::tile_align(tile_h, 16) as usize)?;
                        Some((*tile_num, jpeg))
                    })
                    .collect();
                encoded.sort_unstable_by_key(|(n, _)| *n);
                for (tile_num, jpeg) in &encoded {
                    let split = split_jpeg_to_tables_and_tile(jpeg);
                    let write_bytes: &[u8] = if let Some((ref tables, ref tile_data)) = split {
                        match registered_tables {
                            None => {
                                unsafe { TIFFSetField(tiff, TIFFTAG_JPEGTABLES as u32,
                                    tables.len() as u32, tables.as_ptr()); }
                                registered_tables = Some(tables.clone());
                                tile_data.as_slice()
                            }
                            Some(ref rt) if rt == tables => tile_data.as_slice(),
                            _ => jpeg.as_slice(),
                        }
                    } else {
                        jpeg.as_slice()
                    };
                    unsafe { TIFFWriteRawTile(tiff, *tile_num,
                        write_bytes.as_ptr() as *mut c_void, write_bytes.len() as i64); }
                }
            } else {
                for (frag_idx, fragment) in fragments.iter().enumerate() {
                    if fragment.is_empty() { continue; }
                    let tile_num = tile_indices.get(frag_idx).copied()
                        .unwrap_or(frag_idx as u32);
                    let split = (compression == 7)
                        .then(|| split_jpeg_to_tables_and_tile(fragment))
                        .flatten();
                    let write_bytes: &[u8] = if let Some((ref tables, ref tile_data)) = split {
                        match registered_tables {
                            None => {
                                unsafe { TIFFSetField(tiff, TIFFTAG_JPEGTABLES as u32,
                                    tables.len() as u32, tables.as_ptr()); }
                                registered_tables = Some(tables.clone());
                                tile_data.as_slice()
                            }
                            Some(ref rt) if rt == tables => tile_data.as_slice(),
                            // Different DQT/DHT tables — write self-contained JPEG
                            _ => fragment.as_slice(),
                        }
                    } else {
                        fragment.as_slice()
                    };
                    unsafe { TIFFWriteRawTile(tiff, tile_num,
                        write_bytes.as_ptr() as *mut c_void, write_bytes.len() as i64); }
                }
            }
        }

        unsafe { TIFFWriteDirectory(tiff); }
        let group_tiles: u64 = group.iter().map(|m| m.n_frames.unwrap_or(0) as u64).sum();
        if let Some(p) = pb { p.inc(group_tiles); }
    }
    unsafe { TIFFClose(tiff); }
}
