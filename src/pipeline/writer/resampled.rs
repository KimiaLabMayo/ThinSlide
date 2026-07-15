// Resampled TIFF writer.
//
// Decodes each DICOM resolution level, resizes every tile by the same scale
// factor (derived from the base level), and writes a pyramidal OME-TIFF using
// libtiff's built-in JPEG encoder (TIFFWriteTile + JPEGCOLORMODE_RGB).
//
// Output tile size is fixed across all levels (computed once from the base
// level and rounded to the nearest multiple of 16 for JPEG MCU compliance).
// Pyramid levels whose longer side falls below MIN_PYRAMID_SIDE are skipped.
// If the source has only one DICOM level the output is a single-IFD TIFF.

use crate::bindings::{
    TIFF, TIFFOpen, TIFFSetField, TIFFWriteRawTile, TIFFWriteDirectory, TIFFClose,
    TIFFTAG_YCBCRSUBSAMPLING, TIFFTAG_ICCPROFILE, TIFFTAG_JPEGTABLES,
    TIFFTAG_SUBIFD, TIFFTAG_IMAGEDESCRIPTION,
    PHOTOMETRIC_RGB, PHOTOMETRIC_YCBCR, PHOTOMETRIC_MINISBLACK,
    FILETYPE_REDUCEDIMAGE,
};
use crate::source::dicom::{
    DcmMetadata, CompressionType, ColorSpace,
    tiff_compression_tag, infer_color_space, extract_icc_profile,
    group_by_resolution, frame_to_tile_indices, map_transfer_syntax_to_compression,
};
use crate::pipeline::encode::{split_jpeg_to_tables_and_tile, compose_and_encode, compute_thread, write_enc_chunk};
use crate::pipeline::icc::IccTransform;
use crate::pipeline::ome::generate_dicom_ome_xml;
use std::ffi::CString;
use std::os::raw::c_void;
use std::sync::Arc;
use std::sync::mpsc;
use image::imageops::FilterType;
use indicatif::ProgressBar;
use fast_image_resize as fir;

// ── Producer-Consumer pipeline types ─────────────────────────────────────────

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
    decode_shift:     u32,
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
            let scaling = match p.decode_shift {
                1 => Some(turbojpeg::ScalingFactor::ONE_HALF),
                2 => Some(turbojpeg::ScalingFactor::ONE_QUARTER),
                _ => None,
            };
            if let Some(sf) = scaling {
                let mut dec = turbojpeg::Decompressor::new().ok()?;
                dec.set_scaling_factor(sf).ok()?;
                let header = dec.read_header(data).ok()?;
                let scaled = header.scaled(sf);
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
            let (mut pixels, luma_w, luma_h) = crate::jp2k_assemble_pixels(&j2k_img, p.spp as usize)?;
            let cs = j2k_img.color_space();
            if p.spp == 3
                && !matches!(cs, jpeg2k::ColorSpace::SRGB)
                && (p.jp2k_has_ict_rct || p.color_space == ColorSpace::YCbCr || matches!(cs, jpeg2k::ColorSpace::SYCC))
            {
                crate::ycbcr_to_rgb(&mut pixels);
            }
            Some((pixels, luma_w as u32, luma_h as u32))
        } else {
            Some((data.clone(), p.src_tile_w, p.src_tile_h))
        }
    });

    compose_and_encode(out_id, decoded, ch, p.out_tile_w, p.out_tile_h,
        p.icc_transform.as_deref(), p.fir_pixel_type, &p.resize_opts, p.quality, p.spp)
}

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
    // 0 = generic --mpp resampling; 1/2 = --20x DCT-domain half/quarter decode
    // (turbojpeg ONE_HALF/ONE_QUARTER, JP2K DWT level-1/level-2).
    decode_shift: u32,
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
        // Target MPP for this output level: scale target_mpp by the ratio of
        // this group's MPP to the base level's MPP.
        let group_mpp_x = groups[i][0].mpp_x.filter(|&v| v > 0.0).unwrap_or(src_mpp_x);
        let group_mpp_y = groups[i][0].mpp_y.filter(|&v| v > 0.0).unwrap_or(src_mpp_y);
        let target_lv_mpp_x = target_mpp * (group_mpp_x / src_mpp_x);
        let target_lv_mpp_y = target_mpp * (group_mpp_y / src_mpp_y);

        // Find the input group whose MPP is closest to target_lv_mpp_x.
        let chosen = groups.iter()
            .min_by(|a, b| {
                let ma = a[0].mpp_x.unwrap_or(src_mpp_x);
                let mb = b[0].mpp_x.unwrap_or(src_mpp_x);
                (ma - target_lv_mpp_x).abs()
                    .partial_cmp(&(mb - target_lv_mpp_x).abs()).unwrap()
            })
            .unwrap();

        let chosen_meta  = chosen[0];
        let chosen_mpp_x = chosen_meta.mpp_x.unwrap_or(src_mpp_x);
        let chosen_mpp_y = chosen_meta.mpp_y.unwrap_or(src_mpp_y);
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
            if compr == 7 && !super::is_jpeg_tile_aligned(chosen_tw, chosen_th) {
                passthrough = false;
            }
        }

        let (out_img_w, out_img_h, out_tile_w, out_tile_h, actual_mpp_x, actual_mpp_y) =
            if passthrough {
                // No scaling: output dimensions equal the source dimensions.
                (chosen_w, chosen_h, chosen_tw, chosen_th, chosen_mpp_x, chosen_mpp_y)
            } else {
                // For --20x + JP2K: DWT level-N yields ceil(tw/2^N) per quad; use that
                // directly so the assembled canvas matches out_tile exactly — no
                // correction resize needed.
                let is_jp2k_pow2 = decode_shift > 0 && matches!(
                    map_transfer_syntax_to_compression(&chosen_meta.transfer_syntax_uid),
                    CompressionType::Jpeg2000Lossless | CompressionType::Jpeg2000
                    | CompressionType::Jpeg2000Part2MulticomponentLossless
                    | CompressionType::Jpeg2000Part2Multicomponent
                );
                if is_jp2k_pow2 {
                    let div = 1u32 << decode_shift;
                    let nat_otw = ((chosen_tw + div - 1) / div).max(1);  // ceil(tw/div)
                    let nat_oth = ((chosen_th + div - 1) / div).max(1);
                    let scale_x = if chosen_tw > 0 { nat_otw as f64 / chosen_tw as f64 } else { 1.0 };
                    let scale_y = if chosen_th > 0 { nat_oth as f64 / chosen_th as f64 } else { 1.0 };
                    let oiw = (chosen_w as f64 * scale_x).round() as u32;
                    let oih = (chosen_h as f64 * scale_y).round() as u32;
                    let amx = if nat_otw > 0 { chosen_mpp_x * chosen_tw as f64 / nat_otw as f64 } else { chosen_mpp_x };
                    let amy = if nat_oth > 0 { chosen_mpp_y * chosen_th as f64 / nat_oth as f64 } else { chosen_mpp_y };
                    (oiw, oih, nat_otw * 2, nat_oth * 2, amx, amy)
                } else {
                    // Output tile covers a 2×2 block of source tiles (one source tile maps to
                    // nat_otw_f × nat_oth_f output pixels); apply nearest_16 to the full output
                    // tile size so that rounding on the doubled value is more accurate than
                    // rounding each half and doubling (e.g. nearest_16(120)*2=256 vs nearest_16(240)=240).
                    let nat_otw_f = chosen_tw as f64 * chosen_mpp_x / target_lv_mpp_x;
                    let nat_oth_f = chosen_th as f64 * chosen_mpp_y / target_lv_mpp_y;
                    let otw = crate::nearest_16(nat_otw_f * 2.0);
                    let oth = crate::nearest_16(nat_oth_f * 2.0);
                    let nat_otw = (otw / 2).max(1);
                    let nat_oth = (oth / 2).max(1);
                    // Image dimensions and actual MPP derived from the natural tile scale.
                    let scale_x = if chosen_tw > 0 { nat_otw as f64 / chosen_tw as f64 } else { 1.0 };
                    let scale_y = if chosen_th > 0 { nat_oth as f64 / chosen_th as f64 } else { 1.0 };
                    let oiw = (chosen_w as f64 * scale_x).round() as u32;
                    let oih = (chosen_h as f64 * scale_y).round() as u32;
                    let amx = if nat_otw > 0 { chosen_mpp_x * chosen_tw as f64 / nat_otw as f64 } else { chosen_mpp_x };
                    let amy = if nat_oth > 0 { chosen_mpp_y * chosen_th as f64 / nat_oth as f64 } else { chosen_mpp_y };
                    (oiw, oih, otw, oth, amx, amy)
                }
            };

        if out_img_w.max(out_img_h) < crate::MIN_PYRAMID_SIDE {
            if verbose {
                eprintln!("  [skip ] lv{}  {}x{}  below MIN_PYRAMID_SIDE ({})",
                    i, out_img_w, out_img_h, crate::MIN_PYRAMID_SIDE);
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
    let icc_transform = super::log_icc_and_build_transform(icc_profile.as_deref(), icc_bake, pb, verbose);

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

    let write_level_tiles = |tiff: *mut TIFF, lv: &LevelInfo, decode_shift: u32| {
        let chunk_size = (rayon::current_num_threads() * 4).max(1);

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

        // n_reduce for JP2K DWT: choose the number of resolution reduction levels.
        // For --20x the out_tile was computed from ceil(src/2^decode_shift), so
        // n_reduce=decode_shift is exact. For --mpp, derive n_reduce from the
        // scale ratio so DWT does most of the work.
        let n_reduce: u32 = if is_jp2k_src && decode_shift > 0 {
            decode_shift
        } else {
            let nat_otw = (lv.out_tile_w / 2).max(1);
            let nat_oth = (lv.out_tile_h / 2).max(1);
            let scale_down = (lv.src_tile_w as f64 / nat_otw as f64)
                .min(lv.src_tile_h as f64 / nat_oth as f64);
            if scale_down > 1.0 { scale_down.log2().floor() as u32 } else { 0 }
        };

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
                let fragments = super::pixel_fragments(&dicom_obj);
                for (fi, frag) in fragments.iter().enumerate() {
                    if frag.is_empty() { continue; }
                    let tile_num = tile_indices.get(fi).copied().unwrap_or(fi as u32);
                    src_tile_data.insert(tile_num, frag.to_vec());
                }
            } else {
                use dicom_pixeldata::PixelDecoder;
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
            decode_shift,
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

            unsafe { super::set_tiff_ifd_tags(tiff, subfile_type,
                lv.out_img_w, lv.out_img_h, lv.out_tile_w, lv.out_tile_h,
                compr, photo_lv, spp_lv,
                lv.actual_mpp_x, lv.actual_mpp_y); }
            if matches!(cs_lv, ColorSpace::YCbCr) {
                let frags_tmp = super::pixel_fragments(&first_dcm);
                if let Some(frag) = frags_tmp.iter().find(|f| !f.is_empty()) {
                    if let Some((h, v)) = crate::detect_jpeg_subsampling(frag) {
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
                let fragments = super::pixel_fragments(&dicom_obj);
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
            unsafe { super::set_tiff_ifd_tags(tiff, subfile_type,
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

            write_level_tiles(tiff, lv, decode_shift);
        }

        unsafe { TIFFWriteDirectory(tiff); }
    }

    unsafe { TIFFClose(tiff); }
}
