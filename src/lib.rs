// WSI dicom to tiff/svs converter
// Convert the whole slide image dicom files to a single pyramidal OME-TIFF (default) or
// legacy format (SVS / generic BigTIFF) when --legacy is passed.

#[allow(non_upper_case_globals)]
#[allow(non_camel_case_types)]
#[allow(non_snake_case)]
pub mod bindings;
pub mod args;
pub mod pipeline;
pub mod source;
pub use args::Args;
pub(crate) use pipeline::icc::{IccTransform, build_icc_transform, apply_icc};
pub(crate) use pipeline::encode::{ycbcr_to_rgb, jp2k_assemble_pixels, compose_and_encode, compute_thread, write_enc_chunk};
pub use pipeline::encode::split_jpeg_to_tables_and_tile;
pub(crate) use pipeline::writer::set_tiff_ifd_tags;
pub(crate) use pipeline::ome::generate_dicom_ome_xml;
pub use pipeline::ome::xml_escape;
pub use pipeline::run;
pub(crate) use source::tiff::{COMPRESSION_APERIO_JP2_YCBCR, COMPRESSION_APERIO_JP2_RGB};
pub(crate) use source::dicom::{
    DcmMetadata, CompressionType, ColorSpace,
    is_jpeg2000, map_transfer_syntax_to_compression, tiff_compression_tag,
    infer_color_space, extract_icc_profile,
    frame_to_tile_indices, group_by_resolution,
};
mod tiffds;
use bindings::{
    TIFF, TIFFOpen, TIFFSetField, TIFFWriteRawTile, TIFFWriteDirectory, TIFFClose,
    TIFFTAG_SUBFILETYPE, TIFFTAG_IMAGEWIDTH, TIFFTAG_IMAGELENGTH,
    TIFFTAG_COMPRESSION, TIFFTAG_PHOTOMETRIC, TIFFTAG_SAMPLESPERPIXEL,
    TIFFTAG_BITSPERSAMPLE, TIFFTAG_PLANARCONFIG,
    PHOTOMETRIC_RGB, PHOTOMETRIC_YCBCR, PHOTOMETRIC_MINISBLACK,
    TIFFTAG_IMAGEDESCRIPTION, TIFFTAG_YCBCRSUBSAMPLING,
    PLANARCONFIG_CONTIG, TIFFTAG_SUBIFD, TIFFWriteRawStrip,
    FILETYPE_REDUCEDIMAGE,
    TIFFTAG_ROWSPERSTRIP,
    TIFFTAG_ICCPROFILE,
    TIFFTAG_JPEGTABLES,
};

use std::ffi::CString;
use std::os::raw::c_void;
use std::sync::Arc;
use std::sync::mpsc;
use dicom_pixeldata::PixelDecoder;
use image::imageops::FilterType;
use indicatif::ProgressBar;
use fast_image_resize as fir;
use rayon::prelude::*;

pub(crate) fn vlog(pb: Option<&ProgressBar>, msg: impl AsRef<str>) {
    if let Some(p) = pb { p.println(msg.as_ref()); }
    else { eprintln!("{}", msg.as_ref()); }
}

fn pixel_fragments(dcm: &dicom::object::DefaultDicomObject) -> &[Vec<u8>] {
    dcm.element_by_name("PixelData")
        .expect("No PixelData")
        .fragments()
        .expect("Not encapsulated pixel data")
}

fn bake_jpeg_tile(fragment: &[u8], xform: &IccTransform, quality: u8, out_w: usize, out_h: usize) -> Option<Vec<u8>> {
    let img = turbojpeg::decompress(fragment, turbojpeg::PixelFormat::RGB).ok()?;
    let (w, h) = (img.width, img.height);
    let src_pitch = w * 3;
    let src_pix: Vec<u8> = if img.pitch == src_pitch {
        img.pixels
    } else {
        (0..h).flat_map(|r| img.pixels[r*img.pitch..r*img.pitch+src_pitch].iter().copied()).collect()
    };
    let mut dst_pix = vec![0u8; src_pix.len()];
    apply_icc(xform, &src_pix, &mut dst_pix);
    // Pad to declared tile dimensions so the JPEG SOF matches the TIFF tile tag.
    let (enc_pix, enc_pitch) = if out_w > w || out_h > h {
        let mut padded = vec![0u8; out_w * out_h * 3];
        for row in 0..h.min(out_h) {
            let copy_w = w.min(out_w) * 3;
            padded[row * out_w * 3..row * out_w * 3 + copy_w]
                .copy_from_slice(&dst_pix[row * src_pitch..row * src_pitch + copy_w]);
        }
        (padded, out_w * 3)
    } else {
        (dst_pix, src_pitch)
    };
    let tj = turbojpeg::Image::<&[u8]> {
        pixels: &enc_pix, width: out_w, pitch: enc_pitch, height: out_h,
        format: turbojpeg::PixelFormat::RGB,
    };
    turbojpeg::compress(tj, quality as i32, turbojpeg::Subsamp::Sub2x2)
        .map(|b| b.to_vec()).ok()
}

fn decode_jp2k_to_rgb(fragment: &[u8], has_ict_rct: bool) -> Option<(Vec<u8>, u32, u32)> {
    let j2k = jpeg2k::Image::from_bytes_with(fragment, jpeg2k::DecodeParameters::default()).ok()?;
    let n_comps = j2k.components().len();
    let (mut pixels, w, h) = jp2k_assemble_pixels(&j2k, 3)?;
    let cs = j2k.color_space();
    if n_comps >= 3
        && !matches!(cs, jpeg2k::ColorSpace::SRGB)
        && (has_ict_rct || matches!(cs, jpeg2k::ColorSpace::SYCC))
    {
        ycbcr_to_rgb(&mut pixels);
    }
    Some((pixels, w as u32, h as u32))
}

fn bake_jp2k_tile(fragment: &[u8], has_ict_rct: bool, xform: &IccTransform, quality: u8) -> Option<Vec<u8>> {
    let (src, w, h) = decode_jp2k_to_rgb(fragment, has_ict_rct)?;
    let mut dst = vec![0u8; src.len()];
    apply_icc(xform, &src, &mut dst);
    let tj = turbojpeg::Image::<&[u8]> {
        pixels: &dst, width: w as usize, pitch: w as usize * 3, height: h as usize,
        format: turbojpeg::PixelFormat::RGB,
    };
    turbojpeg::compress(tj, quality as i32, turbojpeg::Subsamp::Sub2x2)
        .map(|b| b.to_vec()).ok()
}

/// Minimum length of the longer image side (pixels) required to include a
/// pyramid level in the resampled output.  Levels below this threshold are
/// skipped because they add no useful detail at typical screen resolutions.
pub const MIN_PYRAMID_SIDE: u32 = 512;

// ── Producer-Consumer pipeline types for lib.rs ──────────────────────────────

type LibRawChunk = Vec<(u32, [Option<Vec<u8>>; 4])>;
type LibEncChunk = Vec<(u32, Vec<u8>)>;

struct LibEncodeParams {
    spp:              u32,
    src_tile_w:       u32,
    src_tile_h:       u32,
    out_tile_w:       u32,
    out_tile_h:       u32,
    quality:          u8,
    fir_pixel_type:   fir::PixelType,
    resize_opts:      fir::ResizeOptions,
    is_jpeg_src:      bool,
    is_jp2k_src:      bool,
    jp2k_has_ict_rct: bool,
    color_space:      ColorSpace,
    n_reduce:         u32,
    half:             bool,
    icc_transform:    Option<Arc<IccTransform>>,
}

fn encode_one_tile_lib(
    out_id: u32,
    quads:  &[Option<Vec<u8>>; 4],
    p:      &LibEncodeParams,
) -> Option<(u32, Vec<u8>)> {
    let ch = p.spp as usize;

    let decoded: [Option<(Vec<u8>, u32, u32)>; 4] = std::array::from_fn(|qi| {
        let data = quads[qi].as_ref()?;
        if p.is_jpeg_src {
            let fmt = if p.spp == 1 { turbojpeg::PixelFormat::GRAY } else { turbojpeg::PixelFormat::RGB };
            if p.half {
                let mut dec = turbojpeg::Decompressor::new().ok()?;
                dec.set_scaling_factor(turbojpeg::ScalingFactor::ONE_HALF).ok()?;
                let header = dec.read_header(data).ok()?;
                let scaled = header.scaled(turbojpeg::ScalingFactor::ONE_HALF);
                let (w, h) = (scaled.width, scaled.height);
                let pitch = w * ch;
                let mut pixels = vec![0u8; h * pitch];
                dec.decompress(data, turbojpeg::Image {
                    pixels: pixels.as_mut_slice(), width: w, pitch, height: h, format: fmt,
                }).ok()?;
                Some((pixels, w as u32, h as u32))
            } else {
                let dec = turbojpeg::decompress(data, fmt).ok()?;
                let (w, h) = (dec.width as u32, dec.height as u32);
                let pitch = w as usize * ch;
                let pix = if dec.pitch == pitch {
                    dec.pixels
                } else {
                    (0..h as usize).flat_map(|r| {
                        dec.pixels[r*dec.pitch..r*dec.pitch+pitch].iter().copied()
                    }).collect()
                };
                Some((pix, w, h))
            }
        } else if p.is_jp2k_src {
            let params = jpeg2k::DecodeParameters::default().reduce(p.n_reduce);
            let j2k_img = jpeg2k::Image::from_bytes_with(data, params).ok()?;
            let (mut pixels, luma_w, luma_h) = jp2k_assemble_pixels(&j2k_img, p.spp as usize)?;
            let cs = j2k_img.color_space();
            if p.spp == 3
                && !matches!(cs, jpeg2k::ColorSpace::SRGB)
                && (p.jp2k_has_ict_rct || p.color_space == ColorSpace::YCbCr || matches!(cs, jpeg2k::ColorSpace::SYCC))
            {
                ycbcr_to_rgb(&mut pixels);
            }
            Some((pixels, luma_w as u32, luma_h as u32))
        } else {
            Some((data.clone(), p.src_tile_w, p.src_tile_h))
        }
    });

    compose_and_encode(out_id, decoded, ch, p.out_tile_w, p.out_tile_h,
        p.icc_transform.as_deref(), p.fir_pixel_type, &p.resize_opts, p.quality, p.spp)
}

/// Decode a single DICOM frame and encode it as a JPEG byte stream.
/// Returns (jpeg_bytes, width, height).
/// Only used for SVS thumbnail/label/overview IFDs, which require a self-contained JPEG stream (no raw tiles).
fn decode_frame_as_jpeg(dcm_path: &str, frame: u32, _quality: u8) -> Option<(Vec<u8>, u32, u32)> {
    let dcm = dicom::object::open_file(dcm_path).ok()?;
    let decoded = dcm.decode_pixel_data_frame(frame).ok()?;
    let img = decoded.to_dynamic_image(0).ok()?;
    let w = img.width();
    let h = img.height();
    let mut bytes = Vec::new();
    img.write_to(
        &mut std::io::Cursor::new(&mut bytes),
        image::ImageFormat::Jpeg,
    ).ok()?;
    Some((bytes, w, h))
}

// ─── JPEG subsampling detection ──────────────────────────────────────────────

/// Parse a JPEG byte stream and return the YCbCr chroma subsampling factors
/// as (horiz, vert) for TIFF's YCbCrSubSampling tag.
/// E.g. 4:2:2 → (2, 1), 4:2:0 → (2, 2), 4:4:4 → (1, 1).
pub fn detect_jpeg_subsampling(data: &[u8]) -> Option<(u16, u16)> {
    if data.len() < 2 || data[0] != 0xFF || data[1] != 0xD8 {
        return None;
    }
    let mut i = 2usize;
    while i + 1 < data.len() {
        if data[i] != 0xFF {
            return None;
        }
        let marker = data[i + 1];
        i += 2;
        // SOF0 (baseline), SOF1 (extended sequential), SOF2 (progressive)
        if matches!(marker, 0xC0 | 0xC1 | 0xC2) {
            // Segment layout at i:
            //   [0..2]  length (includes these 2 bytes)
            //   [2]     sample precision
            //   [3..5]  height
            //   [5..7]  width
            //   [7]     number of components
            //   [8..]   3 bytes per component: id, sampling_factors (H<<4|V), qt_selector
            if i + 13 > data.len() {
                return None;
            }
            let ncomp = data[i + 7] as usize;
            if ncomp < 2 || i + 8 + ncomp * 3 > data.len() {
                return None;
            }
            let y_h  = (data[i + 9] >> 4) as u16;
            let y_v  = (data[i + 9] & 0x0F) as u16;
            let cb_h = (data[i + 12] >> 4) as u16;
            let cb_v = (data[i + 12] & 0x0F) as u16;
            if cb_h == 0 || cb_v == 0 {
                return None;
            }
            return Some((y_h / cb_h, y_v / cb_v));
        } else if marker == 0xD9 {
            break; // EOI
        } else if matches!(marker, 0xD0..=0xD8 | 0x01) {
            continue; // standalone markers (no length field)
        } else {
            if i + 2 > data.len() {
                return None;
            }
            let len = u16::from_be_bytes([data[i], data[i + 1]]) as usize;
            if len < 2 {
                return None;
            }
            i += len;
        }
    }
    None
}

/// Log ICC profile status and optionally build an ICC transform.
fn log_icc_and_build_transform(
    icc_profile: Option<&[u8]>,
    icc_bake: bool,
    pb: Option<&ProgressBar>,
    verbose: bool,
) -> Option<Arc<IccTransform>> {
    if verbose {
        let msg = match icc_profile {
            Some(icc) => format!("  [icc  ] {} bytes", icc.len()),
            None      => "  [icc  ] not found".to_string(),
        };
        vlog(pb, &msg);
    }
    let xf = if icc_bake { icc_profile.and_then(build_icc_transform) } else { None };
    if icc_bake && verbose {
        vlog(pb, if xf.is_some() {
            "  [icc  ] baking → sRGB"
        } else {
            "  [icc  ] bake skipped (no profile or build failed)"
        });
    }
    xf
}

// ─── Generic pyramidal TIFF writer (JPEG-compressed DICOM) ───────────────────

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
        if cmp == 7 && !is_jpeg_tile_aligned(tw, th) { return None; }
        extract_icc_profile(&dcm)
    });
    let icc_transform = log_icc_and_build_transform(icc_profile.as_deref(), icc_bake, pb, verbose);

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
                vlog(pb, format!("  [skip ] lv{}  {}x{}  no tile size",
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
        if compression == 7 && !is_jpeg_tile_aligned(tile_w, tile_h) {
            if verbose {
                vlog(pb, format!("  [skip ] lv{}  {}x{}  tile {}x{} not 16-aligned",
                    group_idx, ifd_width, ifd_height, tile_w, tile_h));
            }
            continue;
        }

        let mpp = metadata.mpp_x.unwrap_or(0.25);

        let n_tiles: u64 = group.iter().map(|m| m.n_frames.unwrap_or(0) as u64).sum();
        if verbose {
            let tag = if baking { "bake " } else { "pass " };
            vlog(pb, format!("  [{}] lv{}  {}x{}  {:.4} µm/px  tile {}x{}  ({} tiles)",
                tag, group_idx, ifd_width, ifd_height, mpp, tile_w, tile_h, n_tiles));
        }

        let subfile_type: u32 = if group_idx == 0 { 0 } else { FILETYPE_REDUCEDIMAGE };
        unsafe { set_tiff_ifd_tags(tiff, subfile_type,
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
            let frags_tmp = pixel_fragments(&first_dcm);
            if let Some(first) = frags_tmp.iter().find(|f| !f.is_empty()) {
                if let Some((h, v)) = detect_jpeg_subsampling(first) {
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
            let fragments    = pixel_fragments(&dicom_obj);

            if baking && compression == 7 {
                let xf = icc_transform.as_deref().unwrap();
                let frags: Vec<(u32, &[u8])> = fragments.iter().enumerate()
                    .filter(|(_, f)| !f.is_empty())
                    .map(|(i, f)| (tile_indices.get(i).copied().unwrap_or(i as u32), f.as_slice()))
                    .collect();
                let mut encoded: Vec<(u32, Vec<u8>)> = frags.par_iter()
                    .filter_map(|(tile_num, frag)| {
                        let jpeg = bake_jpeg_tile(frag, xf, quality,
                            tile_align(tile_w, 16) as usize, tile_align(tile_h, 16) as usize)?;
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

/// Round `v` up to the nearest multiple of `align`.
/// libtiff requires JPEG tile dimensions to be multiples of 16 (YCbCr MCU boundary).
pub fn tile_align(v: u32, align: u32) -> u32 {
    (v + align - 1) / align * align
}

fn is_jpeg_tile_aligned(tile_w: u32, tile_h: u32) -> bool {
    tile_w % 16 == 0 && tile_h % 16 == 0
}

/// Round `v` to the nearest multiple of 16, with a minimum of 16.
pub fn nearest_16(v: f64) -> u32 {
    ((v / 16.0).round() as u32).max(1) * 16
}


// ─── OME-TIFF writer ─────────────────────────────────────────────────────────
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
        let icc_transform = log_icc_and_build_transform(icc_profile.as_deref(), icc_bake, pb, verbose);
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
            vlog(pb, format!("  [{}] lv0  {}x{}  {:.4} µm/px  tile {}x{}  ({} tiles)",
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
            set_tiff_ifd_tags(tiff, 0,
                ifd_width, ifd_height, tile_w, tile_h,
                if baking { 7u32 } else { compr }, photometric, spp,
                mpp, mpp);
            // OME-XML in ImageDescription marks this as an OME-TIFF.
            TIFFSetField(tiff, TIFFTAG_IMAGEDESCRIPTION as u32, image_desc_c.as_ptr());
        }

        if baking {
            unsafe { TIFFSetField(tiff, TIFFTAG_YCBCRSUBSAMPLING as u32, 2u32, 2u32); }
        } else if matches!(color_space, ColorSpace::YCbCr) {
            let frags_tmp = pixel_fragments(&first_dcm);
            if let Some(frag) = frags_tmp.iter().find(|f| !f.is_empty()) {
                if let Some((h, v)) = detect_jpeg_subsampling(frag) {
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
            let fragments    = pixel_fragments(&dicom_obj);
            for (fi, fragment) in fragments.iter().enumerate() {
                if !fragment.is_empty() {
                    let tile_num = tile_indices.get(fi).copied().unwrap_or(fi as u32);
                    let baked: Option<Vec<u8>> = if baking {
                        if compr == 7 {
                            icc_transform.as_deref().and_then(|xf| bake_jpeg_tile(fragment, xf, quality, tile_align(tile_w, 16) as usize, tile_align(tile_h, 16) as usize))
                        } else {
                            icc_transform.as_deref().and_then(|xf| bake_jp2k_tile(fragment, jp2k_has_ict_rct, xf, quality))
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
                vlog(pb, format!("  [skip ] lv{}  {}x{}  no tile size",
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
        if !baking_sub && compr == 7 && !is_jpeg_tile_aligned(tile_w, tile_h) {
            if verbose {
                vlog(pb, format!("  [skip ] lv{}  {}x{}  tile {}x{} not 16-aligned",
                    sub_idx + 1, ifd_width, ifd_height, tile_w, tile_h));
            }
            drop(first_dcm);
            continue;
        }

        let mpp = metadata.mpp_x.unwrap_or(0.25);

        if verbose {
            let n_tiles: u64 = group.iter().map(|m| m.n_frames.unwrap_or(0) as u64).sum();
            let tag = if baking_sub { "bake " } else { "pass " };
            vlog(pb, format!("  [{}] lv{}  {}x{}  {:.4} µm/px  tile {}x{}  ({} tiles)",
                tag, sub_idx + 1, ifd_width, ifd_height, mpp, tile_w, tile_h, n_tiles));
        }

        unsafe { set_tiff_ifd_tags(tiff, FILETYPE_REDUCEDIMAGE,
            ifd_width, ifd_height, tile_w, tile_h,
            if baking_sub { 7u32 } else { compr }, photometric, spp,
            mpp, mpp); }

        if baking_sub {
            unsafe { TIFFSetField(tiff, TIFFTAG_YCBCRSUBSAMPLING as u32, 2u32, 2u32); }
        } else if matches!(color_space, ColorSpace::YCbCr) {
            let frags_tmp = pixel_fragments(&first_dcm);
            if let Some(frag) = frags_tmp.iter().find(|f| !f.is_empty()) {
                if let Some((h, v)) = detect_jpeg_subsampling(frag) {
                    unsafe { TIFFSetField(tiff, TIFFTAG_YCBCRSUBSAMPLING as u32, h as u32, v as u32); }
                }
            }
        }
        drop(first_dcm);

        let mut registered_tables_sub: Option<Vec<u8>> = None;
        for dcm_meta in group.iter() {
            let dicom_obj    = dicom::object::open_file(&dcm_meta.file_path).unwrap();
            let tile_indices = frame_to_tile_indices(&dicom_obj, tile_w, tile_h, ifd_width);
            let fragments    = pixel_fragments(&dicom_obj);
            for (fi, fragment) in fragments.iter().enumerate() {
                if !fragment.is_empty() {
                    let tile_num = tile_indices.get(fi).copied().unwrap_or(fi as u32);
                    let baked: Option<Vec<u8>> = if baking_sub {
                        if compr == 7 {
                            sub_icc_transform.as_deref().and_then(|xf| bake_jpeg_tile(fragment, xf, quality, tile_align(tile_w, 16) as usize, tile_align(tile_h, 16) as usize))
                        } else {
                            sub_icc_transform.as_deref().and_then(|xf| bake_jp2k_tile(fragment, jp2k_has_ict_rct_sub, xf, quality))
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

// ─── SVS writer (JPEG 2000-compressed DICOM → Aperio SVS) ───────────────────
//
// SVS IFD order (required by OpenSlide):
//   IFD 0:         Full resolution pyramid level (largest), tiled, SubFileType=0
//   IFD 1:         Thumbnail (stripped JPEG, small), SubFileType=1
//   IFDs 2..N:     Remaining pyramid levels (descending), tiled, SubFileType=1
//   IFD N+1:       Label image (stripped JPEG), SubFileType=1   [optional]
//   IFD N+2:       Macro/Overview image (stripped JPEG), SubFileType=9 [optional]

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
    icc_transform: Option<&IccTransform>,
    quality: u8,
    jp2k_has_ict_rct: bool,
) { unsafe {
    let dicom_obj  = dicom::object::open_file(&metadata.file_path).unwrap();
    let fragments  = pixel_fragments(&dicom_obj);

    let ifd_width  = metadata.px_columns.unwrap_or(0);
    let ifd_height = metadata.px_rows.unwrap_or(0);
    let (tile_w, tile_h) = metadata.tile_size.unwrap_or((ifd_width, ifd_height));

    // When ICC baking, override compression to standard JPEG (7) and photometric to YCBCR.
    let baking = icc_transform.is_some();
    let out_compression = if baking { 7u32 } else { svs_compression };
    let out_photometric = if baking { PHOTOMETRIC_YCBCR as u32 } else { photometric };

    set_tiff_ifd_tags(tiff, subfile_type,
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
            if let Some((h, v)) = detect_jpeg_subsampling(first) {
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
                    bake_jpeg_tile(frag, xf, quality,
                        tile_align(tile_w, 16) as usize, tile_align(tile_h, 16) as usize)
                } else {
                    bake_jp2k_tile(frag, jp2k_has_ict_rct, xf, quality)
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
    let icc_transform = log_icc_and_build_transform(icc_profile.as_deref(), icc_bake, pb, verbose);
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
            if svs_compression == 7 && !is_jpeg_tile_aligned(tile_w, tile_h) { return 0; }
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
        vlog(pb, format!("  [pass ] lv0  {}x{}  {:.4} µm/px  tile {}x{}  ({} tiles)",
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
        .and_then(|m| decode_frame_as_jpeg(&m.file_path, 0, 90))
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
                vlog(pb, format!("  [skip ] lv{}  {}x{}  no tile size",
                    _i + 1,
                    level.px_columns.unwrap_or(0),
                    level.px_rows.unwrap_or(0)));
            }
            continue;
        }

        let (lvl_tile_w, lvl_tile_h) = level.tile_size.unwrap();
        // For JPEG passthrough without baking, libtiff rejects non-16-aligned tiles.
        if !icc_bake && svs_compression == 7 && !is_jpeg_tile_aligned(lvl_tile_w, lvl_tile_h) {
            if verbose {
                vlog(pb, format!("  [skip ] lv{}  {}x{}  tile {}x{} not 16-aligned",
                    _i + 1, level.px_columns.unwrap_or(0), level.px_rows.unwrap_or(0),
                    lvl_tile_w, lvl_tile_h));
            }
            continue;
        }
        let ds = base_cols / level.px_columns.unwrap_or(1) as f64;
        let lv_mpp = base_mpp_x * ds;
        if verbose {
            let tag = if icc_bake { "bake " } else { "pass " };
            vlog(pb, format!("  [{}] lv{}  {}x{}  {:.4} µm/px  tile {}x{}  ({} tiles)",
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
        if let Some((jpeg, w, h)) = decode_frame_as_jpeg(&m.file_path, 0, 90) {
            unsafe { write_svs_stripped_jpeg(tiff, &jpeg, w, h, FILETYPE_REDUCEDIMAGE); }
            unsafe { TIFFWriteDirectory(tiff); }
            if let Some(p) = pb { p.inc(1); }
        }
    }

    // ── Macro / Overview image ────────────────────────────────────────
    // SubFileType 9 = macro image (Aperio convention, SubFileType bit 3 = 0x8)
    if let Some(m) = overview_meta {
        if let Some((jpeg, w, h)) = decode_frame_as_jpeg(&m.file_path, 0, 90) {
            unsafe { write_svs_stripped_jpeg(tiff, &jpeg, w, h, 9); }
            unsafe { TIFFWriteDirectory(tiff); }
            if let Some(p) = pb { p.inc(1); }
        }
    }

    unsafe { TIFFClose(tiff); }
}

// ─── Resampled TIFF writer ────────────────────────────────────────────────────
//
// Decodes each DICOM resolution level, resizes every tile by the same scale
// factor (derived from the base level), and writes a pyramidal OME-TIFF using
// libtiff's built-in JPEG encoder (TIFFWriteTile + JPEGCOLORMODE_RGB).
//
// Output tile size is fixed across all levels (computed once from the base
// level and rounded to the nearest multiple of 16 for JPEG MCU compliance).
// Pyramid levels whose longer side falls below MIN_PYRAMID_SIDE are skipped.
// If the source has only one DICOM level the output is a single-IFD TIFF.
pub(crate) fn write_resampled_tiff(
    slide_level_metadata_list: &[DcmMetadata],
    output_path: &str,
    target_mpp: f64,
    quality: u8,
    filter: FilterType,
    // When true: OME-TIFF (SubIFD pyramid + OME-XML ImageDescription).
    // When false: flat pyramidal BigTIFF (sequential IFDs, no OME-XML).
    ome: bool,
    pb: Option<&ProgressBar>,
    verbose: bool,
    half: bool,
    icc_bake: bool,
) {

    // ── Group DICOM files by resolution level ─────────────────────────────
    let groups = group_by_resolution(slide_level_metadata_list);

    // ── Scale parameters derived from base level ───────────────────────────
    let base      = groups[0][0];
    let base_w    = base.px_columns.unwrap_or(0);
    let base_h    = base.px_rows.unwrap_or(0);
    let (_in_tile_w, _in_tile_h) = base.tile_size.unwrap_or((base_w, base_h));
    let src_mpp_x = base.mpp_x.filter(|&v| v > 0.0).unwrap_or(0.0);
    let src_mpp_y = base.mpp_y.filter(|&v| v > 0.0).unwrap_or(src_mpp_x);

    // ── Determine which groups produce an active pyramid level ─────────────
    // One output level is generated per DICOM resolution group.  The target
    // output MPP for group i is scaled from target_mpp by the same ratio as
    // the group's MPP to the base level's MPP, preserving the source pyramid
    // structure (e.g. 1×/4×/8× or 1×/2×/4×/8× depending on the DICOM).
    // For each target, the source group with the closest MPP is chosen.
    // If within 10% → passthrough (raw tile copy); otherwise resample.
    struct LevelInfo<'a> {
        src_group:    &'a Vec<&'a DcmMetadata>,  // chosen source (closest by MPP)
        out_img_w:    u32,
        out_img_h:    u32,
        src_tile_w:   u32,                       // tile size in the chosen source group
        src_tile_h:   u32,
        out_tile_w:   u32,                       // tile size written to the output IFD
        out_tile_h:   u32,
        actual_mpp_x: f64,
        actual_mpp_y: f64,
        passthrough:  bool,
    }

    let active_levels: Vec<LevelInfo> = groups.iter().enumerate().filter_map(|(i, _)| {
        // When half=true and MPP is unknown (target_mpp == 0.0), fall back to
        // a normalized 1:2 ratio so scale math produces 0.5× without dividing by zero.
        let half_unknown = half && target_mpp <= 0.0;

        // Target MPP for this output level: scale target_mpp by the ratio of
        // this group's MPP to the base level's MPP.
        let group_mpp_x = groups[i][0].mpp_x.filter(|&v| v > 0.0).unwrap_or(src_mpp_x);
        let group_mpp_y = groups[i][0].mpp_y.filter(|&v| v > 0.0).unwrap_or(src_mpp_y);
        let target_lv_mpp_x = if half_unknown { 2.0 } else { target_mpp * (group_mpp_x / src_mpp_x) };
        let target_lv_mpp_y = if half_unknown { 2.0 } else { target_mpp * (group_mpp_y / src_mpp_y) };

        // When MPP is unknown, pick source group by index (no MPP to compare).
        // Otherwise find the input group whose MPP is closest to target_lv_mpp_x.
        let chosen = if half_unknown {
            &groups[i]
        } else {
            groups.iter()
                .min_by(|a, b| {
                    let ma = a[0].mpp_x.unwrap_or(src_mpp_x);
                    let mb = b[0].mpp_x.unwrap_or(src_mpp_x);
                    (ma - target_lv_mpp_x).abs()
                        .partial_cmp(&(mb - target_lv_mpp_x).abs()).unwrap()
                })
                .unwrap()
        };

        let chosen_meta  = chosen[0];
        // Use 1.0 as normalized dummy for scale math when MPP is unknown.
        let chosen_mpp_x = if half_unknown { 1.0 } else { chosen_meta.mpp_x.unwrap_or(src_mpp_x) };
        let chosen_mpp_y = if half_unknown { 1.0 } else { chosen_meta.mpp_y.unwrap_or(src_mpp_y) };
        let chosen_w     = chosen_meta.px_columns.unwrap_or(0);
        let chosen_h     = chosen_meta.px_rows.unwrap_or(0);
        let (chosen_tw, chosen_th) = chosen_meta.tile_size.unwrap_or((chosen_w, chosen_h));

        // Passthrough if the closest group's MPP is within 10 % of target.
        // ICC baking requires pixel decoding, so passthrough is always disabled when baking.
        let diff = (chosen_mpp_x - target_lv_mpp_x).abs() / target_lv_mpp_x;
        let mut passthrough = diff < 0.1 && !icc_bake;

        if passthrough {
            let compr = tiff_compression_tag(&chosen_meta.transfer_syntax_uid);
            // In --mpp resampling mode the output is always JPEG.  Passing through
            // a non-JPEG source level (e.g. JP2K) would embed a different compression
            // in the pyramid, which confuses readers such as QuPath.
            // Force resample for any non-JPEG source so the pyramid stays uniformly JPEG.
            if compr != 7 {
                passthrough = false;
            }
            // For JPEG sources with non-16-aligned tiles, raw copy is rejected by
            // libtiff → fall back to resample so the level is still produced.
            if compr == 7 && !is_jpeg_tile_aligned(chosen_tw, chosen_th) {
                passthrough = false;
            }
        }

        let (out_img_w, out_img_h, out_tile_w, out_tile_h, actual_mpp_x, actual_mpp_y) =
            if passthrough {
                // No scaling: output dimensions equal the source dimensions.
                (chosen_w, chosen_h, chosen_tw, chosen_th, chosen_mpp_x, chosen_mpp_y)
            } else {
                // Output tile covers a 2×2 block of source tiles (one source tile maps to
                // nat_otw_f × nat_oth_f output pixels); apply nearest_16 to the full output
                // tile size so that rounding on the doubled value is more accurate than
                // rounding each half and doubling (e.g. nearest_16(120)*2=256 vs nearest_16(240)=240).
                let nat_otw_f = chosen_tw as f64 * chosen_mpp_x / target_lv_mpp_x;
                let nat_oth_f = chosen_th as f64 * chosen_mpp_y / target_lv_mpp_y;
                let otw = nearest_16(nat_otw_f * 2.0);
                let oth = nearest_16(nat_oth_f * 2.0);
                let nat_otw = (otw / 2).max(1);
                let nat_oth = (oth / 2).max(1);
                // Image dimensions and actual MPP derived from the natural tile scale.
                let scale_x = if chosen_tw > 0 { nat_otw as f64 / chosen_tw as f64 } else { 1.0 };
                let scale_y = if chosen_th > 0 { nat_oth as f64 / chosen_th as f64 } else { 1.0 };
                let oiw = (chosen_w as f64 * scale_x).round() as u32;
                let oih = (chosen_h as f64 * scale_y).round() as u32;
                // When MPP is unknown, leave actual_mpp as 0.0 (written as no resolution tag).
                let amx = if half_unknown { 0.0 }
                          else if nat_otw > 0 { chosen_mpp_x * chosen_tw as f64 / nat_otw as f64 }
                          else { chosen_mpp_x };
                let amy = if half_unknown { 0.0 }
                          else if nat_oth > 0 { chosen_mpp_y * chosen_th as f64 / nat_oth as f64 }
                          else { chosen_mpp_y };
                (oiw, oih, otw, oth, amx, amy)
            };

        if out_img_w.max(out_img_h) < MIN_PYRAMID_SIDE {
            if verbose {
                eprintln!("  [skip ] lv{}  {}x{}  below MIN_PYRAMID_SIDE ({})",
                    i, out_img_w, out_img_h, MIN_PYRAMID_SIDE);
            }
            return None;
        }

        if verbose {
            let tag = if passthrough { "[pass ]" } else { "[resamp]" };
            let n_tiles: u64 = if passthrough {
                chosen.iter().map(|m| m.n_frames.unwrap_or(0) as u64).sum()
            } else {
                let out_ntx = (out_img_w + out_tile_w - 1) / out_tile_w;
                let out_nty = (out_img_h + out_tile_h - 1) / out_tile_h;
                (out_ntx * out_nty) as u64
            };
            if passthrough {
                eprintln!("  {} lv{}  {}x{}  {:.4} µm/px  tile {}x{}  ({} tiles)",
                    tag, i, out_img_w, out_img_h, actual_mpp_x,
                    out_tile_w, out_tile_h, n_tiles);
            } else {
                eprintln!("  {} lv{}  {}x{}  {:.4} µm/px  src {}x{}→tile {}x{}  ({} tiles)",
                    tag, i, out_img_w, out_img_h, actual_mpp_x,
                    chosen_tw, chosen_th, out_tile_w, out_tile_h, n_tiles);
            }
        }

        Some(LevelInfo {
            src_group: chosen,
            out_img_w, out_img_h,
            src_tile_w: chosen_tw, src_tile_h: chosen_th,
            out_tile_w, out_tile_h,
            actual_mpp_x, actual_mpp_y,
            passthrough,
        })
    }).collect();

    if active_levels.is_empty() { return; }

    // n_subifds: pyramid levels stored as SubIFDs (all active levels except base).
    let n_subifds = active_levels.len() - 1;

    // Total tile count across all active levels for the progress bar.
    let total_tiles: u64 = active_levels.iter().map(|lv| {
        if lv.passthrough {
            lv.src_group.iter().map(|m| m.n_frames.unwrap_or(0) as u64).sum::<u64>()
        } else {
            let out_ntx = (lv.out_img_w + lv.out_tile_w - 1) / lv.out_tile_w;
            let out_nty = (lv.out_img_h + lv.out_tile_h - 1) / lv.out_tile_h;
            (out_ntx * out_nty) as u64
        }
    }).sum();
    if let Some(p) = pb { p.set_length(total_tiles); }

    // ── Color space from the base level ───────────────────────────────────
    let first_dcm   = dicom::object::open_file(&base.file_path).unwrap();
    let color_space = infer_color_space(&first_dcm);

    let icc_profile   = extract_icc_profile(&first_dcm);
    let icc_transform = log_icc_and_build_transform(icc_profile.as_deref(), icc_bake, pb, verbose);

    // The jp2k crate (OpenJPEG) does NOT automatically reverse the JPEG 2000
    // Irreversible/Reversible Color Transform for DICOM tiles.  When DICOM
    // PhotometricInterpretation is YBR_ICT or YBR_RCT the decoded component
    // values are still in the transformed YCbCr-like space; we must apply the
    // inverse transform manually before feeding pixels to turbojpeg.
    let src_photometric_interp = first_dcm.element_by_name("PhotometricInterpretation")
        .ok()
        .and_then(|e| e.to_str().ok().map(|s| s.trim().to_string()))
        .unwrap_or_default();

    let jp2k_has_ict_rct = matches!(src_photometric_interp.as_str(), "YBR_ICT" | "YBR_RCT" | "YBR_FULL" | "YBR_FULL_422");

    // turbojpeg always produces JFIF JPEG (YCbCr encoded internally), regardless
    // of the source color space.  PHOTOMETRIC_YCBCR is the standard convention
    // for JFIF JPEG tiles in TIFF (used by libvips and other WSI tools) and is
    // correctly handled by OpenSlide / Bio-Formats without double-conversion.
    let (photometric, spp): (u32, u32) = match color_space {
        ColorSpace::Grayscale => (PHOTOMETRIC_MINISBLACK as u32, 1),
        _                     => (PHOTOMETRIC_YCBCR      as u32, 3),
    };
    drop(first_dcm);

    // ── OME-XML (only for OME-TIFF output) ───────────────────────────────
    let base_lv = &active_levels[0];
    let image_desc_c: Option<CString> = if ome {
        let mut resampled_meta = base.clone();
        resampled_meta.px_columns = Some(base_lv.out_img_w);
        resampled_meta.px_rows    = Some(base_lv.out_img_h);
        resampled_meta.mpp_x      = if base_lv.actual_mpp_x > 0.0 { Some(base_lv.actual_mpp_x) } else { None };
        resampled_meta.mpp_y      = if base_lv.actual_mpp_y > 0.0 { Some(base_lv.actual_mpp_y) } else { None };
        resampled_meta.tile_size  = Some((base_lv.out_tile_w, base_lv.out_tile_h));
        Some(CString::new(generate_dicom_ome_xml(&[resampled_meta])).unwrap())
    } else {
        None
    };

    // ── Helper: write all tiles for one level ─────────────────────────────
    // Pipeline per tile (fully parallel within each chunk):
    //   1. decode  — turbojpeg::decompress (SIMD) for JPEG sources;
    //                decode_pixel_data_frame for JPEG2000/other
    //   2. resize  — fast_image_resize (SIMD via NEON/AVX2)
    //   3. encode  — turbojpeg::compress (SIMD)
    //   4. write   — TIFFWriteRawTile (~memcpy, sequential)
    //
    // For JPEG sources, raw fragments are pre-extracted as Vec<Vec<u8>> so each
    // worker decodes its own independent bytes (no shared dicom_obj in the hot path).

    // Map image::FilterType → fast_image_resize algorithm once.
    // fast_image_resize has no Gaussian; Lanczos3 is the closest high-quality substitute.
    let fir_alg = match filter {
        image::imageops::FilterType::Nearest    => fir::ResizeAlg::Nearest,
        image::imageops::FilterType::Triangle   => fir::ResizeAlg::Convolution(fir::FilterType::Bilinear),
        image::imageops::FilterType::CatmullRom => fir::ResizeAlg::Convolution(fir::FilterType::CatmullRom),
        image::imageops::FilterType::Gaussian   => fir::ResizeAlg::Convolution(fir::FilterType::Lanczos3),
        image::imageops::FilterType::Lanczos3   => fir::ResizeAlg::Convolution(fir::FilterType::Lanczos3),
    };
    let fir_pixel_type = if spp == 1 { fir::PixelType::U8 } else { fir::PixelType::U8x3 };
    let resize_opts = fir::ResizeOptions::new().resize_alg(fir_alg);

    let write_level_tiles = |tiff: *mut TIFF, lv: &LevelInfo, half: bool| {
        let chunk_size = (rayon::current_num_threads() * 4).max(1);

        // n_reduce for JP2K DWT: each source tile is scaled to nat_otw = out_tile_w/2
        // (the 2×2 canvas is then resized to out_tile_w). Use nat_otw for the ratio.
        let nat_otw = (lv.out_tile_w / 2).max(1);
        let nat_oth = (lv.out_tile_h / 2).max(1);
        let scale_down = (lv.src_tile_w as f64 / nat_otw as f64)
            .min(lv.src_tile_h as f64 / nat_oth as f64);
        let n_reduce: u32 = if scale_down > 1.0 { scale_down.log2().floor() as u32 } else { 0 };

        // Source image dimensions for grid computation (shared across group files).
        let src_img_w = lv.src_group[0].px_columns.unwrap_or(0);
        let src_img_h = lv.src_group[0].px_rows.unwrap_or(0);

        let is_jpeg_src = matches!(
            map_transfer_syntax_to_compression(&lv.src_group[0].transfer_syntax_uid),
            CompressionType::JpegBaseline | CompressionType::JpegExtended
        );
        let is_jp2k_src = matches!(
            map_transfer_syntax_to_compression(&lv.src_group[0].transfer_syntax_uid),
            CompressionType::Jpeg2000Lossless
            | CompressionType::Jpeg2000
            | CompressionType::Jpeg2000Part2MulticomponentLossless
            | CompressionType::Jpeg2000Part2Multicomponent
        );

        // Phase 1: aggregate all source tile data keyed by TIFF tile_num.
        // JPEG / JP2K: store raw encoded bytes (decoded in parallel during stitch).
        // Other compressions: pre-decode to raw pixels (must be sequential).
        let mut src_tile_data: std::collections::HashMap<u32, Vec<u8>> =
            std::collections::HashMap::new();

        for dcm_meta in lv.src_group.iter() {
            let dicom_obj = dicom::object::open_file(&dcm_meta.file_path).unwrap();
            let src_ifd_w = dcm_meta.px_columns.unwrap_or(0);
            let tile_indices = frame_to_tile_indices(
                &dicom_obj, lv.src_tile_w, lv.src_tile_h, src_ifd_w,
            );
            let n_frames = dcm_meta.n_frames.unwrap_or(0);

            if is_jpeg_src || is_jp2k_src {
                let fragments = pixel_fragments(&dicom_obj);
                for (fi, frag) in fragments.iter().enumerate() {
                    if frag.is_empty() { continue; }
                    let tile_num = tile_indices.get(fi).copied().unwrap_or(fi as u32);
                    src_tile_data.insert(tile_num, frag.to_vec());
                }
            } else {
                for fi in 0..n_frames {
                    let tile_num = tile_indices.get(fi as usize).copied().unwrap_or(fi);
                    let decoded = match dicom_obj.decode_pixel_data_frame(fi) {
                        Ok(d) => d,
                        Err(e) => { eprintln!("  [warn] frame {fi}: decode failed: {e}"); continue; }
                    };
                    let img = match decoded.to_dynamic_image(0) {
                        Ok(i) => i,
                        Err(e) => { eprintln!("  [warn] frame {fi}: to_dynamic_image: {e}"); continue; }
                    };
                    let pixels = if spp == 1 {
                        img.to_luma8().into_raw()
                    } else {
                        img.to_rgb8().into_raw()
                    };
                    src_tile_data.insert(tile_num, pixels);
                }
            }
        }


        // Phase 2: iterate over output tiles via producer-consumer pipeline.
        let src_ntx = (src_img_w + lv.src_tile_w - 1) / lv.src_tile_w;
        let src_nty = (src_img_h + lv.src_tile_h - 1) / lv.src_tile_h;
        let out_ntx = (lv.out_img_w + lv.out_tile_w - 1) / lv.out_tile_w;
        let out_nty = (lv.out_img_h + lv.out_tile_h - 1) / lv.out_tile_h;
        let out_tile_ids: Vec<u32> = (0..out_ntx * out_nty).collect();

        let enc_params = std::sync::Arc::new(LibEncodeParams {
            spp,
            src_tile_w:       lv.src_tile_w,
            src_tile_h:       lv.src_tile_h,
            out_tile_w:       lv.out_tile_w,
            out_tile_h:       lv.out_tile_h,
            quality,
            fir_pixel_type,
            resize_opts:      resize_opts.clone(),
            is_jpeg_src,
            is_jp2k_src,
            jp2k_has_ict_rct,
            color_space,
            n_reduce,
            half,
            icc_transform:    icc_transform.clone(),
        });
        let (raw_tx, raw_rx) = mpsc::sync_channel::<LibRawChunk>(2);
        let (enc_tx, enc_rx) = mpsc::sync_channel::<LibEncChunk>(2);
        let params_t = std::sync::Arc::clone(&enc_params);
        let compute_handle = std::thread::spawn(move || {
            compute_thread(raw_rx, enc_tx, |id, quads| encode_one_tile_lib(id, quads, &params_t));
        });

        let mut jpegtables_registered = false;
        let mut pending_write: Option<LibEncChunk> = None;

        for chunk in out_tile_ids.chunks(chunk_size) {
            let raw_chunk: LibRawChunk = chunk.iter()
                .map(|&out_id| {
                    let oc  = out_id % out_ntx;
                    let or_ = out_id / out_ntx;
                    let mut quads: [Option<Vec<u8>>; 4] = [None, None, None, None];
                    for qi in 0..4usize {
                        let dc = (qi % 2) as u32;
                        let dr = (qi / 2) as u32;
                        let sc = 2 * oc + dc;
                        let sr = 2 * or_ + dr;
                        if sc >= src_ntx || sr >= src_nty { continue; }
                        let src_tile_num = sr * src_ntx + sc;
                        quads[qi] = src_tile_data.get(&src_tile_num).cloned();
                    }
                    (out_id, quads)
                })
                .collect();

            raw_tx.send(raw_chunk).expect("compute thread dropped");

            if let Some(prev) = pending_write.take() {
                let n = prev.len() as u64;
                unsafe { write_enc_chunk(tiff, &prev, &mut jpegtables_registered); }
                if let Some(p) = pb { p.inc(n); }
            }
            pending_write = enc_rx.recv().ok();
        }
        drop(raw_tx);

        if let Some(last) = pending_write.take() {
            let n = last.len() as u64;
            unsafe { write_enc_chunk(tiff, &last, &mut jpegtables_registered); }
            if let Some(p) = pb { p.inc(n); }
        }
        for enc in enc_rx {
            let n = enc.len() as u64;
            unsafe { write_enc_chunk(tiff, &enc, &mut jpegtables_registered); }
            if let Some(p) = pb { p.inc(n); }
        }
        compute_handle.join().expect("compute thread panicked");
    };

    let output_path_c = CString::new(output_path).unwrap();
    let w8_mode_c     = CString::new("w8").unwrap();
    let tiff = unsafe { TIFFOpen(output_path_c.as_ptr(), w8_mode_c.as_ptr()) }; // BigTIFF
    assert!(!tiff.is_null(), "TIFFOpen failed: cannot create '{}'", output_path);

    // For OME-TIFF: register the SubIFD chain on IFD 0 before writing anything.
    if ome && n_subifds > 0 {
        let zeros: Vec<u64> = vec![0u64; n_subifds];
        unsafe { TIFFSetField(tiff, TIFFTAG_SUBIFD, n_subifds as u32, zeros.as_ptr()); }
    }

    for (lv_idx, lv) in active_levels.iter().enumerate() {
        let is_base: bool    = lv_idx == 0;
        let subfile_type: u32 = if is_base { 0 } else { FILETYPE_REDUCEDIMAGE };

        if lv.passthrough {
            // ── Passthrough: raw tile copy ────────────────────────────
            // Derive compression and photometric from the chosen source.
            let meta      = lv.src_group[0];
            let first_dcm = dicom::object::open_file(&meta.file_path).unwrap();
            let cs_lv     = infer_color_space(&first_dcm);
            let ts_uid    = first_dcm.meta().transfer_syntax();
            let compr     = tiff_compression_tag(ts_uid);
            let photo_lv: u32 = match cs_lv {
                ColorSpace::RGB       => PHOTOMETRIC_RGB      as u32,
                ColorSpace::YCbCr     => PHOTOMETRIC_YCBCR    as u32,
                ColorSpace::Grayscale => PHOTOMETRIC_MINISBLACK as u32,
            };
            let spp_lv: u32 = if matches!(cs_lv, ColorSpace::Grayscale) { 1 } else { 3 };

            unsafe { set_tiff_ifd_tags(tiff, subfile_type,
                lv.out_img_w, lv.out_img_h, lv.out_tile_w, lv.out_tile_h,
                compr, photo_lv, spp_lv,
                lv.actual_mpp_x, lv.actual_mpp_y); }
            if matches!(cs_lv, ColorSpace::YCbCr) {
                let frags_tmp = pixel_fragments(&first_dcm);
                if let Some(frag) = frags_tmp.iter().find(|f| !f.is_empty()) {
                    if let Some((h, v)) = detect_jpeg_subsampling(frag) {
                        unsafe { TIFFSetField(tiff, TIFFTAG_YCBCRSUBSAMPLING as u32, h as u32, v as u32); }
                    }
                }
            }
            if is_base {
                if let Some(ref desc) = image_desc_c {
                    unsafe { TIFFSetField(tiff, TIFFTAG_IMAGEDESCRIPTION as u32, desc.as_ptr()); }
                }
                if !icc_bake {
                    if let Some(ref icc) = icc_profile {
                        unsafe { TIFFSetField(tiff, TIFFTAG_ICCPROFILE as u32,
                            icc.len() as u32, icc.as_ptr() as *const c_void); }
                    }
                }
            }
            drop(first_dcm);

            // Write raw tiles from every DICOM file in the source group.
            // For JPEG sources, strip redundant DQT/DHT from each tile and store
            // them once in TIFFTAG_JPEGTABLES (~550 bytes saved per tile).
            // When a tile has different tables from the registered ones (e.g. blank
            // stub tiles mixed with real tissue tiles), write it as a self-contained
            // JPEG so the decoder always has the correct tables.
            let mut registered_tables: Option<Vec<u8>> = None;
            for dcm_meta in lv.src_group.iter() {
                let dicom_obj    = dicom::object::open_file(&dcm_meta.file_path).unwrap();
                let ifd_w        = dcm_meta.px_columns.unwrap_or(0);
                let tile_indices = frame_to_tile_indices(
                    &dicom_obj, lv.src_tile_w, lv.src_tile_h, ifd_w,
                );
                let fragments = pixel_fragments(&dicom_obj);
                for (fi, fragment) in fragments.iter().enumerate() {
                    if !fragment.is_empty() {
                        let tile_num = tile_indices.get(fi).copied().unwrap_or(fi as u32);
                        let split = (compr == 7)
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
                                _ => fragment.as_slice(),
                            }
                        } else {
                            fragment.as_slice()
                        };
                        unsafe { TIFFWriteRawTile(tiff, tile_num,
                            write_bytes.as_ptr() as *mut c_void, write_bytes.len() as i64); }
                    }
                }
                if let Some(p) = pb {
                    p.inc(dcm_meta.n_frames.unwrap_or(0) as u64);
                }
            }
        } else {
            // ── Resample: decode → resize → JPEG re-encode ───────────
            unsafe { set_tiff_ifd_tags(tiff, subfile_type,
                lv.out_img_w, lv.out_img_h, lv.out_tile_w, lv.out_tile_h,
                7, photometric, spp,
                lv.actual_mpp_x, lv.actual_mpp_y); }
            if photometric == PHOTOMETRIC_YCBCR as u32 {
                unsafe { TIFFSetField(tiff, TIFFTAG_YCBCRSUBSAMPLING as u32, 2u32, 2u32); }
            }
            if is_base {
                if let Some(ref desc) = image_desc_c {
                    unsafe { TIFFSetField(tiff, TIFFTAG_IMAGEDESCRIPTION as u32, desc.as_ptr()); }
                }
                if !icc_bake {
                    if let Some(ref icc) = icc_profile {
                        unsafe { TIFFSetField(tiff, TIFFTAG_ICCPROFILE as u32,
                            icc.len() as u32, icc.as_ptr() as *const c_void); }
                    }
                }
            }

            write_level_tiles(tiff, lv, half);
        }

        unsafe { TIFFWriteDirectory(tiff); }
    }

    unsafe { TIFFClose(tiff); }
}

