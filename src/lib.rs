// WSI dicom to tiff/svs converter
// Convert the whole slide image dicom files to a single pyramidal OME-TIFF (default) or
// legacy format (SVS / generic BigTIFF) when --legacy is passed.

#[allow(non_upper_case_globals)]
#[allow(non_camel_case_types)]
#[allow(non_snake_case)]
pub mod bindings;
pub mod args;
pub mod logger;
pub mod pipeline;
pub mod source;
pub use args::Args;

// Re-exports required by tiffds.rs (via crate:: paths)
pub(crate) use pipeline::icc::{IccTransform, build_icc_transform, apply_icc};
pub(crate) use pipeline::encode::{ycbcr_to_rgb, jp2k_assemble_pixels, compose_and_encode, compute_thread, write_enc_chunk};
pub use pipeline::encode::split_jpeg_to_tables_and_tile;
pub(crate) use pipeline::writer::set_tiff_ifd_tags;

// Public API
pub use pipeline::ome::xml_escape;
pub use pipeline::run;

mod tiffds;

use indicatif::ProgressBar;

pub(crate) fn vlog(pb: Option<&ProgressBar>, msg: impl AsRef<str>) {
    if let Some(p) = pb { p.println(msg.as_ref()); }
    else { eprintln!("{}", msg.as_ref()); }
}

/// Minimum length of the longer image side (pixels) required to include a
/// pyramid level in the resampled output.
pub const MIN_PYRAMID_SIDE: u32 = 512;

/// Round `v` up to the nearest multiple of `align`.
/// libtiff requires JPEG tile dimensions to be multiples of 16 (YCbCr MCU boundary).
pub fn tile_align(v: u32, align: u32) -> u32 {
    (v + align - 1) / align * align
}

/// Round `v` to the nearest multiple of 16, with a minimum of 16.
pub fn nearest_16(v: f64) -> u32 {
    ((v / 16.0).round() as u32).max(1) * 16
}

/// Classify a source MPP (µm/px) into a nominal scan-magnification bucket and
/// return the power-of-two downsample factor needed to reach the 20x target
/// (~0.5 µm/px):
///   mpp <  0.2            → 80x → factor 4
///   0.2 <= mpp <  0.3      → 40x → factor 2
///   0.3 <= mpp <  0.7      → 20x → factor 1 (already at target)
///   mpp >= 0.7, or unknown → None (upscaling not supported / resolution unknown)
pub fn factor_to_20x(mpp: f64) -> Option<u32> {
    if mpp <= 0.0     { None }
    else if mpp < 0.2 { Some(4) }
    else if mpp < 0.3 { Some(2) }
    else if mpp < 0.7 { Some(1) }
    else              { None }
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
