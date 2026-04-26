use std::os::raw::c_void;
use std::sync::mpsc;
use rayon::prelude::*;
use fast_image_resize as fir;

use crate::bindings::{TIFF, TIFFSetField, TIFFWriteRawTile, TIFFTAG_JPEGTABLES};
use super::icc::{IccTransform, apply_icc};

pub(crate) fn ycbcr_to_rgb(pixels: &mut [u8]) {
    for c in pixels.chunks_mut(3) {
        let y  = c[0] as f32;
        let cb = c[1] as f32 - 128.0;
        let cr = c[2] as f32 - 128.0;
        c[0] = (y + 1.40200 * cr).clamp(0.0, 255.0) as u8;
        c[1] = (y - 0.34414 * cb - 0.71414 * cr).clamp(0.0, 255.0) as u8;
        c[2] = (y + 1.77200 * cb).clamp(0.0, 255.0) as u8;
    }
}

pub fn split_jpeg_to_tables_and_tile(jpeg: &[u8]) -> Option<(Vec<u8>, Vec<u8>)> {
    if jpeg.len() < 4 || jpeg[0] != 0xFF || jpeg[1] != 0xD8 {
        return None;
    }
    let mut tables  = vec![0xFF, 0xD8u8];  // SOI
    let mut tile    = vec![0xFF, 0xD8u8];  // SOI
    let mut i = 2;
    while i + 1 < jpeg.len() {
        if jpeg[i] != 0xFF {
            // Not a marker — treat rest as scan data (shouldn't happen outside SOS)
            tile.extend_from_slice(&jpeg[i..]);
            break;
        }
        let marker = jpeg[i + 1];
        match marker {
            0xD8 => { i += 2; }  // Extra SOI — skip
            0xD9 => break,        // EOI — stop
            0xDA => {
                // SOS: copy the SOS segment and all remaining bytes (entropy-coded data + EOI)
                tile.extend_from_slice(&jpeg[i..]);
                break;
            }
            0xDB | 0xC4 => {
                // DQT / DHT → go into JPEGTABLES
                if i + 3 >= jpeg.len() { return None; }
                let seg_len = u16::from_be_bytes([jpeg[i + 2], jpeg[i + 3]]) as usize + 2;
                if i + seg_len > jpeg.len() { return None; }
                tables.extend_from_slice(&jpeg[i..i + seg_len]);
                i += seg_len;
            }
            0xE0..=0xEF => {
                // APP markers (JFIF, Adobe, Exif, …) — drop from both parts
                if i + 3 >= jpeg.len() { return None; }
                let seg_len = u16::from_be_bytes([jpeg[i + 2], jpeg[i + 3]]) as usize + 2;
                if i + seg_len > jpeg.len() { return None; }
                i += seg_len;
            }
            _ => {
                // SOF, COM, DRI, etc. → keep in stripped tile
                if i + 3 >= jpeg.len() { return None; }
                let seg_len = u16::from_be_bytes([jpeg[i + 2], jpeg[i + 3]]) as usize + 2;
                if i + seg_len > jpeg.len() { return None; }
                tile.extend_from_slice(&jpeg[i..i + seg_len]);
                i += seg_len;
            }
        }
    }
    tables.extend_from_slice(&[0xFF, 0xD9]);  // EOI for JPEGTABLES
    Some((tables, tile))
}

pub(crate) fn compose_and_encode(
    out_id: u32,
    decoded: [Option<(Vec<u8>, u32, u32)>; 4],
    ch: usize,
    out_tile_w: u32,
    out_tile_h: u32,
    icc_transform: Option<&IccTransform>,
    fir_pixel_type: fir::PixelType,
    resize_opts: &fir::ResizeOptions,
    quality: u8,
    spp: u32,
) -> Option<(u32, Vec<u8>)> {
    if decoded.iter().all(|d| d.is_none()) { return None; }

    let (slot_w, slot_h) = decoded.iter()
        .filter_map(|d| d.as_ref().map(|(_, pw, ph)| (*pw, *ph)))
        .fold((1u32, 1u32), |(mw, mh), (w, h)| (mw.max(w), mh.max(h)));
    let canvas_w = slot_w * 2;
    let canvas_h = slot_h * 2;
    let mut canvas = vec![0u8; canvas_w as usize * canvas_h as usize * ch];

    for qi in 0..4usize {
        let Some((pixels, pw, ph)) = &decoded[qi] else { continue; };
        let ox = (qi % 2) * slot_w as usize;
        let oy = (qi / 2) * slot_h as usize;
        for row in 0..(*ph as usize) {
            let src_start = row * *pw as usize * ch;
            let dst_start = (oy + row) * canvas_w as usize * ch + ox * ch;
            canvas[dst_start..dst_start + *pw as usize * ch]
                .copy_from_slice(&pixels[src_start..src_start + *pw as usize * ch]);
        }
    }

    if let Some(xform) = icc_transform {
        if ch == 3 {
            let mut dst = vec![0u8; canvas.len()];
            apply_icc(xform, &canvas, &mut dst);
            canvas = dst;
        }
    }

    let resized: Vec<u8> = if canvas_w == out_tile_w && canvas_h == out_tile_h {
        canvas
    } else {
        let src_fir = fir::images::Image::from_vec_u8(canvas_w, canvas_h, canvas, fir_pixel_type).ok()?;
        let mut dst_fir = fir::images::Image::new(out_tile_w, out_tile_h, fir_pixel_type);
        fir::Resizer::new().resize(&src_fir, &mut dst_fir, resize_opts).ok()?;
        dst_fir.into_vec()
    };

    let jpeg = if spp == 1 {
        turbojpeg::compress(turbojpeg::Image::<&[u8]> {
            pixels: &resized, width: out_tile_w as usize,
            pitch: out_tile_w as usize, height: out_tile_h as usize,
            format: turbojpeg::PixelFormat::GRAY,
        }, quality as i32, turbojpeg::Subsamp::Gray).ok()?.to_vec()
    } else {
        turbojpeg::compress(turbojpeg::Image::<&[u8]> {
            pixels: &resized, width: out_tile_w as usize,
            pitch: out_tile_w as usize * 3, height: out_tile_h as usize,
            format: turbojpeg::PixelFormat::RGB,
        }, quality as i32, turbojpeg::Subsamp::Sub2x2).ok()?.to_vec()
    };

    Some((out_id, jpeg))
}

pub(crate) fn compute_thread<R, F>(
    raw_rx: mpsc::Receiver<Vec<(u32, R)>>,
    enc_tx: mpsc::SyncSender<Vec<(u32, Vec<u8>)>>,
    encode: F,
)
where
    R: Send + Sync,
    F: Fn(u32, &R) -> Option<(u32, Vec<u8>)> + Send + Sync,
{
    for raw_chunk in raw_rx {
        let mut encoded: Vec<(u32, Vec<u8>)> = raw_chunk
            .par_iter()
            .filter_map(|(id, quad)| encode(*id, quad))
            .collect();
        encoded.sort_unstable_by_key(|(n, _)| *n);
        if enc_tx.send(encoded).is_err() { break; }
    }
}

pub(crate) unsafe fn write_enc_chunk(
    tiff: *mut TIFF,
    chunk: &[(u32, Vec<u8>)],
    jpegtables_registered: &mut bool,
) {
    for (id, jpeg) in chunk {
        let split = split_jpeg_to_tables_and_tile(jpeg);
        if !*jpegtables_registered {
            if let Some((ref tables, _)) = split {
                unsafe {
                    TIFFSetField(tiff, TIFFTAG_JPEGTABLES as u32,
                        tables.len() as u32, tables.as_ptr());
                }
                *jpegtables_registered = true;
            }
        }
        let write_bytes = split.as_ref().map(|(_, t)| t.as_slice()).unwrap_or(jpeg.as_slice());
        unsafe {
            TIFFWriteRawTile(tiff, *id,
                write_bytes.as_ptr() as *mut c_void,
                write_bytes.len() as i64);
        }
    }
}
