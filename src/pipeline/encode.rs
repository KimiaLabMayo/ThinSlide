use std::os::raw::c_void;
use std::sync::mpsc;
use rayon::prelude::*;
use fast_image_resize as fir;

use crate::bindings::{TIFF, TIFFWriteRawTile};
use super::icc::{IccTransform, apply_icc};

/// Assemble a pixel buffer from decoded JP2K components with nearest-neighbor chroma upsampling.
/// Returns (pixels, width, height).  Does NOT apply YCbCr→RGB conversion; callers handle that.
pub(crate) fn jp2k_assemble_pixels(img: &jpeg2k::Image, spp: usize) -> Option<(Vec<u8>, usize, usize)> {
    let comps = img.components();
    if comps.is_empty() { return None; }
    let w = comps[0].width() as usize;
    let h = comps[0].height() as usize;
    if w == 0 || h == 0 { return None; }
    let pixels = if spp == 1 || comps.len() < 3 {
        comps[0].data_u8().collect()
    } else {
        let y_u8:  Vec<u8> = comps[0].data_u8().collect();
        let cb_u8: Vec<u8> = comps[1].data_u8().collect();
        let cr_u8: Vec<u8> = comps[2].data_u8().collect();
        let cb_w = comps[1].width() as usize;
        let cb_h = comps[1].height() as usize;
        let cr_w = comps[2].width() as usize;
        let cr_h = comps[2].height() as usize;
        let mut buf = Vec::with_capacity(w * h * 3);
        for row in 0..h {
            for col in 0..w {
                let y      = y_u8[row * w + col];
                let cb_col = (col * cb_w / w).min(cb_w.saturating_sub(1));
                let cb_row = (row * cb_h / h).min(cb_h.saturating_sub(1));
                let cb     = cb_u8[cb_row * cb_w + cb_col];
                let cr_col = (col * cr_w / w).min(cr_w.saturating_sub(1));
                let cr_row = (row * cr_h / h).min(cr_h.saturating_sub(1));
                let cr     = cr_u8[cr_row * cr_w + cr_col];
                buf.extend_from_slice(&[y, cb, cr]);
            }
        }
        buf
    };
    Some((pixels, w, h))
}

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

// ─── White fill tiles for --roi ──────────────────────────────────────────────

/// Uniform white JPEG tile used for tiles outside --roi.
/// Component values are written directly (YCbCr 255/128/128, or 255/255/255 for an
/// RGB-photometric destination, flagged with an Adobe APP14 marker), so the tile can
/// sit in a raw-copied level. DQT/DHT stay inside the stream so it decodes regardless
/// of the level's JPEGTABLES. `subsamp` is the TIFF YCbCrSubSampling (h, v).
pub(crate) fn white_jpeg_tile(w: u32, h: u32, spp: u32, rgb: bool, subsamp: (u16, u16), quality: u8) -> Vec<u8> {
    const APP14_ADOBE_RGB: [u8; 16] = [
        0xFF, 0xEE, 0x00, 0x0E,
        b'A', b'd', b'o', b'b', b'e',
        0x00, 0x64, 0x00, 0x00, 0x00, 0x00, 0x00,
    ];
    let (w, h) = (w as usize, h as usize);
    let jpeg = if spp == 1 {
        turbojpeg::compress(turbojpeg::Image::<&[u8]> {
            pixels: &vec![255u8; w * h], width: w, pitch: w, height: h,
            format: turbojpeg::PixelFormat::GRAY,
        }, quality as i32, turbojpeg::Subsamp::Gray)
    } else {
        let (sub, sh, sv) = match (rgb, subsamp) {
            (true, _) | (false, (1, 1)) => (turbojpeg::Subsamp::None, 1, 1),
            (false, (2, 1))             => (turbojpeg::Subsamp::Sub2x1, 2, 1),
            _                           => (turbojpeg::Subsamp::Sub2x2, 2, 2),
        };
        let (cw, ch) = (w.div_ceil(sh), h.div_ceil(sv));
        let chroma = if rgb { 255u8 } else { 128u8 };
        let y = vec![255u8; w * h];
        let c = vec![chroma; cw * ch];
        turbojpeg::compress_yuv_planes(&turbojpeg::YuvPlanesImage::<&[u8]> {
            y_plane: &y, u_plane: &c, v_plane: &c,
            width: w, height: h, y_stride: w, u_stride: cw, v_stride: cw, subsamp: sub,
        }, quality as i32)
    }.expect("white tile encode failed");

    // Rebuild as SOI [APP14] DQT/DHT SOF.. SOS.. EOI, dropping the JFIF APP0 marker.
    let (tables, tile) = split_jpeg_to_tables_and_tile(&jpeg).expect("white tile split failed");
    let mut out = vec![0xFF, 0xD8];
    if rgb && spp == 3 { out.extend_from_slice(&APP14_ADOBE_RGB); }
    out.extend_from_slice(&tables[2..tables.len() - 2]);
    out.extend_from_slice(&tile[2..]);
    out
}

const JP2_SIGNATURE: [u8; 12] = [0, 0, 0, 0x0c, b'j', b'P', b' ', b' ', 0x0d, 0x0a, 0x87, 0x0a];

/// Some DICOM frames are JP2-wrapped (ISO 15444-1 boxes) instead of raw J2K codestreams.
/// Aperio SVS readers such as OpenSlide only accept raw codestreams, so strip the wrapper.
/// Returns `data` unchanged (borrowed) unless it starts with the JP2 signature box and
/// contains a well-formed `jp2c` box.
pub(crate) fn unwrap_jp2_to_codestream(data: &[u8]) -> std::borrow::Cow<'_, [u8]> {
    if !data.starts_with(&JP2_SIGNATURE) {
        return std::borrow::Cow::Borrowed(data);
    }
    let mut pos = 0usize;
    while pos + 8 <= data.len() {
        let lbox = u32::from_be_bytes(data[pos..pos + 4].try_into().unwrap()) as u64;
        let tbox = &data[pos + 4..pos + 8];
        let (hdr, len) = match lbox {
            0 => (8u64, (data.len() - pos) as u64),
            1 => {
                if pos + 16 > data.len() { break; }
                (16, u64::from_be_bytes(data[pos + 8..pos + 16].try_into().unwrap()))
            }
            n => (8, n),
        };
        if len < hdr || pos as u64 + len > data.len() as u64 { break; }
        if tbox == b"jp2c" {
            return std::borrow::Cow::Borrowed(&data[pos + hdr as usize..pos + len as usize]);
        }
        pos += len as usize;
    }
    std::borrow::Cow::Borrowed(data)
}

fn is_jp2_wrapped(data: &[u8]) -> bool { data.starts_with(&JP2_SIGNATURE) }

/// Prepare one DICOM JPEG 2000 frame for an Aperio SVS tile (raw J2K codestream).
///
/// Regular frames pass through untouched. JP2-wrapped frames come from a different encoder
/// (RCT, colour space declared in the `colr` box, here sRGB), while the SVS level stores
/// components as written (YCbCr when `ycbcr`). Stripping the wrapper alone would make
/// OpenSlide read RGB samples as YCbCr (black becomes green), so decode the frame and
/// re-encode it in the level's convention. Falls back to the bare codestream if that fails.
pub(crate) fn normalize_jp2_tile(data: &[u8], ycbcr: bool) -> std::borrow::Cow<'_, [u8]> {
    if !is_jp2_wrapped(data) {
        return std::borrow::Cow::Borrowed(data);
    }
    match recode_wrapped_tile(data, ycbcr) {
        Some(tile) => std::borrow::Cow::Owned(tile),
        None => {
            eprintln!("  [warn] JP2-wrapped tile could not be re-encoded; writing bare codestream");
            unwrap_jp2_to_codestream(data)
        }
    }
}

fn recode_wrapped_tile(data: &[u8], ycbcr: bool) -> Option<Vec<u8>> {
    let img = jpeg2k::Image::from_bytes_with(data, jpeg2k::DecodeParameters::default()).ok()?;
    let (mut pix, w, h) = jp2k_assemble_pixels(&img, 3)?;
    if img.components().len() != 3 || pix.len() != w * h * 3 { return None; }
    // `pix` holds the components as decoded: YCbCr only when the box declares sYCC.
    let src_is_ycc = matches!(img.color_space(), jpeg2k::ColorSpace::SYCC);
    match (src_is_ycc, ycbcr) {
        (false, true) => rgb_to_ycbcr(&mut pix),
        (true, false) => ycbcr_to_rgb(&mut pix),
        _ => {}
    }
    encode_j2k_raw(w as u32, h as u32, 3, |i, plane| {
        for (dst, px) in plane.iter_mut().zip(pix.chunks_exact(3)) { *dst = px[i] as i32; }
    })
}

pub(crate) fn rgb_to_ycbcr(pixels: &mut [u8]) {
    for c in pixels.chunks_mut(3) {
        let (r, g, b) = (c[0] as f32, c[1] as f32, c[2] as f32);
        c[0] = (0.299 * r + 0.587 * g + 0.114 * b).round().clamp(0.0, 255.0) as u8;
        c[1] = (128.0 - 0.168736 * r - 0.331264 * g + 0.5 * b).round().clamp(0.0, 255.0) as u8;
        c[2] = (128.0 + 0.5 * r - 0.418688 * g - 0.081312 * b).round().clamp(0.0, 255.0) as u8;
    }
}

/// Uniform white JPEG 2000 codestream (raw J2K, lossless) with `ncomp` full-resolution
/// components: 255/128/128 for YCbCr sources, 255 in every component otherwise.
pub(crate) fn white_jp2k_tile(w: u32, h: u32, ncomp: u32, ycbcr: bool) -> Option<Vec<u8>> {
    encode_j2k_raw(w, h, ncomp, |i, comp| comp.fill(if ycbcr && i > 0 { 128 } else { 255 }))
}

/// JPEG 2000 codestream (raw J2K, lossless, no MCT) of `ncomp` full-resolution 8-bit
/// components; `fill(i, plane)` writes component `i` (row-major, `w * h` samples).
fn encode_j2k_raw(w: u32, h: u32, ncomp: u32, fill: impl Fn(usize, &mut [i32])) -> Option<Vec<u8>> {
    use openjp2::openjpeg::*;

    struct MemWriter { buf: Vec<u8>, pos: usize }
    unsafe extern "C" fn write_fn(src: *mut c_void, n: usize, user: *mut c_void) -> usize {
        let wr = unsafe { &mut *(user as *mut MemWriter) };
        let data = unsafe { std::slice::from_raw_parts(src as *const u8, n) };
        let end = wr.pos + n;
        if wr.buf.len() < end { wr.buf.resize(end, 0); }
        wr.buf[wr.pos..end].copy_from_slice(data);
        wr.pos = end;
        n
    }
    unsafe extern "C" fn skip_fn(n: i64, user: *mut c_void) -> i64 {
        let wr = unsafe { &mut *(user as *mut MemWriter) };
        wr.pos = (wr.pos as i64 + n).max(0) as usize;
        n
    }
    unsafe extern "C" fn seek_fn(pos: i64, user: *mut c_void) -> i32 {
        let wr = unsafe { &mut *(user as *mut MemWriter) };
        wr.pos = pos.max(0) as usize;
        1
    }

    // The DWT needs at least 2^(numresolution-1) pixels per side.
    let numres = (w.min(h).max(1).ilog2() + 1).min(6) as i32;
    let mut cmpt: Vec<openjp2::opj_image_comptparm> = (0..ncomp).map(|_| openjp2::opj_image_comptparm {
        dx: 1, dy: 1, w, h, x0: 0, y0: 0, prec: 8, bpp: 8, sgnd: 0,
    }).collect();

    unsafe {
        let image = opj_image_create(ncomp, cmpt.as_mut_ptr(), OPJ_COLOR_SPACE::OPJ_CLRSPC_UNSPECIFIED);
        if image.is_null() { return None; }
        (*image).x1 = w;
        (*image).y1 = h;
        for i in 0..ncomp as usize {
            let comp = &mut *(*image).comps.add(i);
            fill(i, std::slice::from_raw_parts_mut(comp.data, (w * h) as usize));
        }

        let mut params = std::mem::zeroed::<opj_cparameters_t>();
        opj_set_default_encoder_parameters(&mut params);
        params.tcp_numlayers = 1;
        params.tcp_rates[0] = 0.0;
        params.cp_disto_alloc = 1;
        params.numresolution = numres;
        params.tcp_mct = 0;

        let mut wr = MemWriter { buf: Vec::new(), pos: 0 };
        let codec = opj_create_compress(OPJ_CODEC_FORMAT::OPJ_CODEC_J2K);
        let stream = opj_stream_create(1 << 16, 0);
        opj_stream_set_write_function(stream, Some(write_fn));
        opj_stream_set_skip_function(stream, Some(skip_fn));
        opj_stream_set_seek_function(stream, Some(seek_fn));
        opj_stream_set_user_data(stream, &mut wr as *mut MemWriter as *mut c_void, None);
        let ok = opj_setup_encoder(codec, &mut params, image) == 1
            && opj_start_compress(codec, image, stream) == 1
            && opj_encode(codec, stream) == 1
            && opj_end_compress(codec, stream) == 1;
        opj_stream_destroy(stream);
        opj_destroy_codec(codec);
        opj_image_destroy(image);
        if ok { Some(wr.buf) } else { None }
    }
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
                super::writer::set_jpeg_tables(tiff, tables);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn white_jpeg_tile_decodes_to_white() {
        for (spp, rgb, sub) in [(3, false, (2, 2)), (3, false, (2, 1)), (3, true, (1, 1)), (1, false, (1, 1))] {
            let jpeg = white_jpeg_tile(240, 240, spp, rgb, sub, 80);
            let fmt = if spp == 1 { turbojpeg::PixelFormat::GRAY } else { turbojpeg::PixelFormat::RGB };
            let img = turbojpeg::decompress(&jpeg, fmt).unwrap();
            assert_eq!((img.width, img.height), (240, 240));
            assert!(img.pixels.iter().all(|&v| v >= 254), "spp={spp} rgb={rgb} sub={sub:?}");
        }
    }

    fn wrap_jp2(codestream: &[u8], xl: bool) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&JP2_SIGNATURE);
        out.extend_from_slice(&[0, 0, 0, 20, b'f', b't', b'y', b'p', b'j', b'p', b'2', b' ', 0, 0, 0, 0, b'j', b'p', b'2', b' ']);
        if xl {
            out.extend_from_slice(&[0, 0, 0, 1, b'j', b'p', b'2', b'c']);
            out.extend_from_slice(&(codestream.len() as u64 + 16).to_be_bytes());
        } else {
            out.extend_from_slice(&(codestream.len() as u32 + 8).to_be_bytes());
            out.extend_from_slice(b"jp2c");
        }
        out.extend_from_slice(codestream);
        out
    }

    /// JP2 file with ihdr + colr (enumerated colour space `enum_cs`) around `codestream`.
    fn wrap_jp2_with_header(codestream: &[u8], w: u32, h: u32, enum_cs: u32) -> Vec<u8> {
        let mut ihdr = Vec::new();
        ihdr.extend_from_slice(&h.to_be_bytes());
        ihdr.extend_from_slice(&w.to_be_bytes());
        ihdr.extend_from_slice(&[0, 3, 7, 7, 0, 0]);
        let mut colr = vec![1, 0, 0];
        colr.extend_from_slice(&enum_cs.to_be_bytes());
        let bx = |t: &[u8; 4], body: &[u8]| {
            let mut b = ((body.len() + 8) as u32).to_be_bytes().to_vec();
            b.extend_from_slice(t);
            b.extend_from_slice(body);
            b
        };
        let jp2h = bx(b"jp2h", &[bx(b"ihdr", &ihdr), bx(b"colr", &colr)].concat());
        let mut out = JP2_SIGNATURE.to_vec();
        out.extend(bx(b"ftyp", b"jp2 \0\0\0\0jp2 "));
        out.extend(jp2h);
        out.extend(bx(b"jp2c", codestream));
        out
    }

    fn decoded_planes(codestream: &[u8]) -> Vec<Vec<u8>> {
        let img = jpeg2k::Image::from_bytes_with(codestream, jpeg2k::DecodeParameters::default()).unwrap();
        img.components().iter().map(|c| c.data_u8().collect()).collect()
    }

    #[test]
    fn normalize_recodes_wrapped_srgb_tile_to_level_convention() {
        // Black RGB tile as written by the sRGB/RCT encoder (cf. CPTAC edge tiles).
        let rgb = encode_j2k_raw(64, 64, 3, |_, p| p.fill(0)).unwrap();
        let wrapped = wrap_jp2_with_header(&rgb, 64, 64, 16);
        for (ycbcr, expect) in [(true, [0u8, 128, 128]), (false, [0, 0, 0])] {
            let out = normalize_jp2_tile(&wrapped, ycbcr);
            assert!(out.starts_with(&[0xFF, 0x4F]), "raw codestream expected");
            for (plane, e) in decoded_planes(&out).iter().zip(expect) {
                assert_eq!(plane.len(), 64 * 64);
                assert!(plane.iter().all(|&v| v == e), "ycbcr={ycbcr}");
            }
        }
    }

    #[test]
    fn normalize_leaves_raw_codestream_untouched() {
        let j2k = white_jp2k_tile(64, 64, 3, true).unwrap();
        assert!(matches!(normalize_jp2_tile(&j2k, true), std::borrow::Cow::Borrowed(_)));
    }

    #[test]
    fn rgb_ycbcr_roundtrip() {
        let orig: Vec<u8> = vec![0, 0, 0, 255, 255, 255, 200, 30, 90, 10, 220, 40];
        let mut px = orig.clone();
        rgb_to_ycbcr(&mut px);
        ycbcr_to_rgb(&mut px);
        assert!(px.iter().zip(&orig).all(|(a, b)| (*a as i32 - *b as i32).abs() <= 2), "{px:?}");
    }

    #[test]
    fn unwrap_jp2_returns_raw_codestream() {
        let j2k = white_jp2k_tile(64, 64, 3, true).unwrap();
        for xl in [false, true] {
            let wrapped = wrap_jp2(&j2k, xl);
            assert_eq!(unwrap_jp2_to_codestream(&wrapped).as_ref(), j2k.as_slice(), "xl={xl}");
        }
    }

    #[test]
    fn unwrap_jp2_leaves_other_data_untouched() {
        let j2k = white_jp2k_tile(64, 64, 3, false).unwrap();
        assert!(matches!(unwrap_jp2_to_codestream(&j2k), std::borrow::Cow::Borrowed(_)));
        let mut truncated = wrap_jp2(&j2k, false);
        truncated.truncate(truncated.len() - 4);
        assert_eq!(unwrap_jp2_to_codestream(&truncated).as_ref(), truncated.as_slice());
        assert!(unwrap_jp2_to_codestream(&[]).is_empty());
    }

    #[test]
    fn white_jp2k_tile_component_values() {
        for (ycbcr, expect) in [(false, [255, 255, 255]), (true, [255, 128, 128])] {
            let j2k = white_jp2k_tile(240, 240, 3, ycbcr).unwrap();
            let img = jpeg2k::Image::from_bytes_with(&j2k, jpeg2k::DecodeParameters::default()).unwrap();
            let comps = img.components();
            assert_eq!(comps.len(), 3);
            for (c, e) in comps.iter().zip(expect) {
                assert_eq!((c.width(), c.height()), (240, 240));
                assert!(c.data_u8().all(|v| v == e));
            }
        }
    }
}
