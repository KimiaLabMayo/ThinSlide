// TIFF writer helpers and shared codec utilities used by all writer submodules.

pub(crate) mod flat;
pub(crate) mod ome;
pub(crate) mod svs;
pub(crate) mod resampled;

pub(crate) use flat::write_flat_multipage_tiff;
pub(crate) use ome::write_ome_tiff;
pub(crate) use svs::write_svs;
pub(crate) use resampled::write_resampled_tiff;

use crate::bindings::{
    TIFF, TIFFSetField,
    TIFFTAG_SUBFILETYPE, TIFFTAG_IMAGEWIDTH, TIFFTAG_IMAGELENGTH,
    TIFFTAG_TILEWIDTH, TIFFTAG_TILELENGTH, TIFFTAG_COMPRESSION,
    TIFFTAG_PHOTOMETRIC, TIFFTAG_SAMPLESPERPIXEL, TIFFTAG_BITSPERSAMPLE,
    TIFFTAG_SAMPLEFORMAT, TIFFTAG_PLANARCONFIG, TIFFTAG_ORIENTATION,
    TIFFTAG_RESOLUTIONUNIT, TIFFTAG_XRESOLUTION, TIFFTAG_YRESOLUTION,
    SAMPLEFORMAT_UINT, PLANARCONFIG_CONTIG, ORIENTATION_TOPLEFT,
    RESUNIT_CENTIMETER,
};
use std::sync::Arc;
use indicatif::ProgressBar;
use super::icc::{IccTransform, build_icc_transform, apply_icc};
use super::encode::{jp2k_assemble_pixels, ycbcr_to_rgb};
use dicom_pixeldata::PixelDecoder;

/// Set the standard image/tile/compression/resolution TIFF tags common to every IFD.
/// Pass mpp_x == 0.0 to skip resolution tags (unknown physical size).
pub(crate) unsafe fn set_tiff_ifd_tags(
    tiff: *mut TIFF,
    subfile_type: u32,
    width: u32, height: u32,
    tile_w: u32, tile_h: u32,
    compression: u32,
    photometric: u32,
    spp: u32,
    mpp_x: f64, mpp_y: f64,
) { unsafe {
    TIFFSetField(tiff, TIFFTAG_SUBFILETYPE as u32,     subfile_type);
    TIFFSetField(tiff, TIFFTAG_IMAGEWIDTH as u32,      width);
    TIFFSetField(tiff, TIFFTAG_IMAGELENGTH as u32,     height);
    TIFFSetField(tiff, TIFFTAG_TILEWIDTH as u32,       crate::tile_align(tile_w, 16));
    TIFFSetField(tiff, TIFFTAG_TILELENGTH as u32,      crate::tile_align(tile_h, 16));
    TIFFSetField(tiff, TIFFTAG_COMPRESSION as u32,     compression);
    TIFFSetField(tiff, TIFFTAG_PHOTOMETRIC as u32,     photometric);
    TIFFSetField(tiff, TIFFTAG_SAMPLESPERPIXEL as u32, spp);
    TIFFSetField(tiff, TIFFTAG_BITSPERSAMPLE as u32,   8u32);
    TIFFSetField(tiff, TIFFTAG_SAMPLEFORMAT as u32,    SAMPLEFORMAT_UINT as u32);
    TIFFSetField(tiff, TIFFTAG_PLANARCONFIG as u32,    PLANARCONFIG_CONTIG as u32);
    TIFFSetField(tiff, TIFFTAG_ORIENTATION as u32,     ORIENTATION_TOPLEFT as u32);
    if mpp_x > 0.0 {
        TIFFSetField(tiff, TIFFTAG_RESOLUTIONUNIT as u32, RESUNIT_CENTIMETER as u32);
        TIFFSetField(tiff, TIFFTAG_XRESOLUTION as u32,   1e4 / mpp_x);
        TIFFSetField(tiff, TIFFTAG_YRESOLUTION as u32,   1e4 / mpp_y);
    }
}}

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

/// Decode a single DICOM frame and encode it as a JPEG byte stream.
/// Returns (jpeg_bytes, width, height).
/// Only used for SVS thumbnail/label/overview IFDs, which require a self-contained JPEG stream.
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
        crate::vlog(pb, &msg);
    }
    let xf = if icc_bake { icc_profile.and_then(build_icc_transform) } else { None };
    if icc_bake && verbose {
        crate::vlog(pb, if xf.is_some() {
            "  [icc  ] baking → sRGB"
        } else {
            "  [icc  ] bake skipped (no profile or build failed)"
        });
    }
    xf
}

fn is_jpeg_tile_aligned(tile_w: u32, tile_h: u32) -> bool {
    tile_w % 16 == 0 && tile_h % 16 == 0
}
