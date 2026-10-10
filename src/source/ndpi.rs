// Hamamatsu NDPI (.ndpi) reader and converter.
//
// The reader is adapted from the wsi2dz project's NDPI reader.
//
// NDPI is a little-endian classic TIFF (magic 42) with 64-bit extensions:
//   - bytes 4..12 hold the first IFD offset as a u64
//   - each IFD is: u16 count, count x 12-byte entries, u64 next-IFD offset,
//     then count x u32 high halves of the entries' 64-bit values/offsets
//   - every pyramid level is one JPEG strip (RowsPerStrip = ImageLength);
//     tag 65426 (McuStarts) lists the offset of each restart interval inside
//     the strip, so any rectangle of restart intervals ("tiles", e.g. 2048x8)
//     can be decoded on its own by splicing them under a patched SOF header.
//
// The strip cannot be re-tiled losslessly, so NDPI is always decoded and
// re-encoded. The output base level is rendered band by band (one output tile
// row at a time) from the coarsest stored level that still meets the target
// resolution: the decoder's DCT scaling covers the power-of-two part of the
// remaining factor, and a resize covers the rest, so the number of source rows
// read per band follows the target MPP. Reduced levels are then built at 1/4
// steps from the base tiles. Only one focal plane (Z=0) is read.

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::ffi::CString;
use std::ops::Range;
use std::os::raw::c_void;
use std::path::Path;
use std::sync::Arc;

use fast_image_resize as fir;
use image::imageops::FilterType;
use indicatif::ProgressBar;
use rayon::prelude::*;

use crate::args::Scale;
use crate::bindings::{
    TIFFOpen, TIFFClose, TIFFSetField, TIFFWriteDirectory,
    TIFFTAG_IMAGEDESCRIPTION, TIFFTAG_ICCPROFILE, TIFFTAG_SUBIFD, TIFFTAG_YCBCRSUBSAMPLING,
    PHOTOMETRIC_YCBCR, COMPRESSION_JPEG,
};
use crate::{set_tiff_ifd_tags, write_enc_chunk, vlog, IccTransform, apply_icc};

// ─── TIFF / NDPI tags ─────────────────────────────────────────────────────────

const TYPE_LONG: u16 = 4;

const TAG_IMAGE_WIDTH:       u16 = 256;
const TAG_IMAGE_LENGTH:      u16 = 257;
const TAG_COMPRESSION:       u16 = 259;
const TAG_STRIP_OFFSETS:     u16 = 273;
const TAG_ROWS_PER_STRIP:    u16 = 278;
const TAG_STRIP_BYTE_COUNTS: u16 = 279;
const TAG_X_RESOLUTION:      u16 = 282;
const TAG_Y_RESOLUTION:      u16 = 283;
const TAG_RESOLUTION_UNIT:   u16 = 296;
const TAG_ICC_PROFILE:       u16 = 34675;
const TAG_FILE_FORMAT:       u16 = 65420; // present ⇔ NDPI
const TAG_MAGNIFICATION:     u16 = 65421; // < 0 for macro / map images
const TAG_Z_OFFSET:          u16 = 65424;
const TAG_MCU_STARTS:        u16 = 65426;
const TAG_MCU_STARTS_HIGH:   u16 = 65432;

// Output tile size (multiple of 16 for JPEG YCbCr MCU compliance, and of 4 for
// the 1/4-step reducer).
const OUT_TILE: u32 = 512;
// Restart-interval rows decoded per task: small enough to spread one band over
// all threads even when the level has only a few interval columns.
const CHUNK_ROWS: u32 = 16;
const NCH: usize = 3; // NDPI pyramid levels are RGB (YCbCr JPEG)

// ─── Little-endian readers ────────────────────────────────────────────────────

fn rd_u16(d: &[u8], o: usize) -> Option<u16> {
    d.get(o..o + 2).map(|b| u16::from_le_bytes([b[0], b[1]]))
}
fn rd_u32(d: &[u8], o: usize) -> Option<u32> {
    d.get(o..o + 4).map(|b| u32::from_le_bytes(b.try_into().unwrap()))
}
fn rd_u64(d: &[u8], o: usize) -> Option<u64> {
    d.get(o..o + 8).map(|b| u64::from_le_bytes(b.try_into().unwrap()))
}

// ─── IFD ──────────────────────────────────────────────────────────────────────

struct Tag {
    typ:   u16,
    count: u64,
    value: u64, // low 32 bits from the entry, high 32 bits from the extension area
}

struct Ifd(HashMap<u16, Tag>);

impl Ifd {
    fn read(d: &[u8], off: u64) -> Option<(Ifd, u64)> {
        let off = off as usize;
        let n = rd_u16(d, off)? as usize;
        let next = rd_u64(d, off + 2 + n * 12)?;
        let high = off + 2 + n * 12 + 8;
        let tags = (0..n).map(|i| {
            let e = off + 2 + i * 12;
            let tag = Tag {
                typ:   rd_u16(d, e + 2)?,
                count: rd_u32(d, e + 4)? as u64,
                value: (rd_u32(d, high + i * 4)? as u64) << 32 | rd_u32(d, e + 8)? as u64,
            };
            Some((rd_u16(d, e)?, tag))
        }).collect::<Option<HashMap<_, _>>>()?;
        Some((Ifd(tags), next))
    }

    fn value(&self, id: u16) -> Option<u64> { self.0.get(&id).map(|t| t.value) }

    fn f32(&self, id: u16) -> Option<f32> { self.value(id).map(|v| f32::from_bits(v as u32)) }

    // Out-of-line payload of `len` bytes (only for values larger than 4 bytes).
    fn payload<'a>(&self, id: u16, d: &'a [u8], len: u64) -> Option<&'a [u8]> {
        let t = self.0.get(&id)?;
        if len <= 4 { return None; }
        d.get(t.value as usize..(t.value + len) as usize)
    }

    fn bytes<'a>(&self, id: u16, d: &'a [u8]) -> Option<&'a [u8]> {
        self.payload(id, d, self.0.get(&id)?.count)
    }

    fn u32s(&self, id: u16, d: &[u8]) -> Option<Vec<u32>> {
        let t = self.0.get(&id)?;
        if t.typ != TYPE_LONG { return None; }
        if t.count == 1 { return Some(vec![t.value as u32]); }
        let raw = self.payload(id, d, t.count * 4)?;
        Some(raw.chunks_exact(4).map(|b| u32::from_le_bytes(b.try_into().unwrap())).collect())
    }

    fn rational(&self, id: u16, d: &[u8]) -> Option<f64> {
        let raw = self.payload(id, d, 8)?;
        let (num, den) = (rd_u32(raw, 0)? as f64, rd_u32(raw, 4)? as f64);
        (den != 0.0).then(|| num / den)
    }

    // µm/px from XResolution/YResolution; (0, 0) when absent.
    fn mpp(&self, d: &[u8]) -> (f64, f64) {
        let per_unit = match self.value(TAG_RESOLUTION_UNIT).unwrap_or(2) as u16 {
            3 => 10_000.0, // centimeter
            2 => 25_400.0, // inch
            _ => return (0.0, 0.0),
        };
        match (self.rational(TAG_X_RESOLUTION, d), self.rational(TAG_Y_RESOLUTION, d)) {
            (Some(x), Some(y)) if x > 0.0 && y > 0.0 => (per_unit / x, per_unit / y),
            _ => (0.0, 0.0),
        }
    }
}

// ─── Pyramid level ────────────────────────────────────────────────────────────

struct Level {
    w:          u32,
    h:          u32,
    strip:      (usize, usize), // (file offset, byte count) of the JPEG strip
    header:     Vec<u8>,        // SOI .. end of the SOS header (DRI included)
    sof_off:    usize,          // offset of the SOF height field in `header`
    mcu_starts: Vec<u64>,       // restart-interval offsets relative to the strip
    tile_w:     u32,            // pixel size of one restart interval
    tile_h:     u32,
    cols:       u32,            // restart-interval grid
    rows:       u32,
    mpp:        (f64, f64),
}

// JPEG header of a strip: (header bytes, SOF height offset, MCU size, restart interval).
fn parse_jpeg_header(s: &[u8]) -> Result<(Vec<u8>, usize, (u32, u32), u32), String> {
    if !s.starts_with(&[0xFF, 0xD8]) { return Err("strip is not a JPEG stream".into()); }
    let (mut pos, mut ri, mut sof, mut mcu) = (2usize, 0u32, None, (8u32, 8u32));
    loop {
        let (Some(&0xFF), Some(&marker)) = (s.get(pos), s.get(pos + 1)) else {
            return Err("truncated JPEG header".into());
        };
        let len = u16::from_be_bytes([*s.get(pos + 2).ok_or("truncated JPEG header")?,
                                      *s.get(pos + 3).ok_or("truncated JPEG header")?]) as usize;
        let seg = s.get(pos + 4..pos + 2 + len).ok_or("truncated JPEG header")?;
        match marker {
            0xC0 | 0xC1 => {
                // precision(1) height(2) width(2) ncomp(1) then (id, HV, tq) per component
                let ncomp = *seg.get(5).ok_or("bad SOF")? as usize;
                let (h, v) = (0..ncomp).filter_map(|c| seg.get(7 + c * 3))
                    .fold((1u32, 1u32), |(h, v), &hv| (h.max((hv >> 4) as u32), v.max((hv & 0x0F) as u32)));
                mcu = (h * 8, v * 8);
                sof = Some(pos + 5);
            }
            0xC2 | 0xC3 | 0xC5..=0xC7 | 0xC9..=0xCB | 0xCD..=0xCF => {
                return Err(format!("unsupported JPEG process (SOF marker 0x{marker:02X})"));
            }
            0xDD => ri = u16::from_be_bytes([*seg.first().ok_or("bad DRI")?, *seg.get(1).ok_or("bad DRI")?]) as u32,
            0xDA => {
                let sof = sof.ok_or("JPEG strip has no SOF marker")?;
                return Ok((s[..pos + 2 + len].to_vec(), sof, mcu, ri));
            }
            _ => {}
        }
        pos += 2 + len;
    }
}

// Builds a level from one IFD; None for non-pyramid images (macro, map) and
// levels without restart markers, which cannot be decoded piecewise.
fn parse_level(ifd: &Ifd, d: &[u8]) -> Result<Option<Level>, String> {
    if ifd.f32(TAG_MAGNIFICATION).unwrap_or(1.0) <= 0.0 { return Ok(None); }
    let (Some(w), Some(h)) = (ifd.value(TAG_IMAGE_WIDTH), ifd.value(TAG_IMAGE_LENGTH)) else {
        return Ok(None);
    };
    let (w, h) = (w as u32, h as u32);
    if w == 0 || h == 0 || !matches!(ifd.value(TAG_COMPRESSION), Some(6 | 7)) { return Ok(None); }
    if ifd.value(TAG_ROWS_PER_STRIP).is_some_and(|r| r as u32 != h) { return Ok(None); }
    let (Some(off), Some(len)) = (ifd.value(TAG_STRIP_OFFSETS), ifd.value(TAG_STRIP_BYTE_COUNTS)) else {
        return Ok(None);
    };
    let (off, len) = (off as usize, len as usize);
    let strip = d.get(off..off + len).ok_or_else(|| format!("{w}x{h} level: strip beyond end of file"))?;
    let (header, sof_off, (mcu_w, mcu_h), ri) = parse_jpeg_header(strip)
        .map_err(|e| format!("{w}x{h} level: {e}"))?;
    if ri == 0 { return Ok(None); }

    // A restart interval must not wrap across MCU rows, or intervals would not
    // form a rectangular grid.
    let mcus_per_row = w.div_ceil(mcu_w);
    if mcus_per_row % ri != 0 {
        return Err(format!("{w}x{h} level: restart interval {ri} does not divide {mcus_per_row} MCUs per row"));
    }
    let (cols, rows) = (mcus_per_row / ri, h.div_ceil(mcu_h));
    let low = ifd.u32s(TAG_MCU_STARTS, d).unwrap_or_default();
    let high = ifd.u32s(TAG_MCU_STARTS_HIGH, d).unwrap_or_default();
    let mcu_starts: Vec<u64> = low.iter().enumerate()
        .map(|(i, &l)| (high.get(i).copied().unwrap_or(0) as u64) << 32 | l as u64)
        .collect();
    if mcu_starts.len() != (cols * rows) as usize {
        return Err(format!("{w}x{h} level: {} McuStarts entries, expected {}", mcu_starts.len(), cols * rows));
    }
    Ok(Some(Level {
        w, h, strip: (off, len), header, sof_off, mcu_starts,
        tile_w: mcu_w * ri, tile_h: mcu_h, cols, rows, mpp: ifd.mpp(d),
    }))
}

struct NdpiSource {
    mmap:   memmap2::Mmap,
    levels: Vec<Level>, // one focal plane, finest first
    icc:    Option<Vec<u8>>,
}

impl NdpiSource {
    fn open(path: &str) -> Result<NdpiSource, String> {
        let file = std::fs::File::open(path).map_err(|e| format!("open: {e}"))?;
        let mmap = unsafe { memmap2::Mmap::map(&file) }.map_err(|e| format!("mmap: {e}"))?;
        let d: &[u8] = &mmap;
        if d.len() < 12 || &d[0..2] != b"II" || rd_u16(d, 2) != Some(42) {
            return Err("not a little-endian TIFF".into());
        }

        let mut ifds = Vec::new();
        let mut seen = HashSet::new();
        let mut off = rd_u64(d, 4).unwrap_or(0);
        while off != 0 && seen.insert(off) {
            let (ifd, next) = Ifd::read(d, off).ok_or_else(|| format!("truncated IFD at offset {off}"))?;
            ifds.push(ifd);
            off = next;
        }
        let ifd0 = ifds.first().ok_or("no IFD")?;
        if ifd0.value(TAG_FILE_FORMAT).is_none() {
            return Err(format!("TIFF without Hamamatsu tag {TAG_FILE_FORMAT}; not an NDPI file"));
        }
        let icc = ifd0.bytes(TAG_ICC_PROFILE, d).map(|b| b.to_vec());

        // Pyramid levels grouped by focal plane (Z offset in nm); Z=0 is the
        // autofocus plane, otherwise take the middle one.
        let mut planes: BTreeMap<i32, Vec<Level>> = BTreeMap::new();
        for ifd in &ifds {
            if let Some(lv) = parse_level(ifd, d)? {
                let z = ifd.value(TAG_Z_OFFSET).unwrap_or(0) as u32 as i32;
                planes.entry(z).or_default().push(lv);
            }
        }
        let zs: Vec<i32> = planes.keys().copied().collect();
        let z = if planes.contains_key(&0) { 0 } else { *zs.get(zs.len() / 2).ok_or("no decodable pyramid level")? };
        let mut levels = planes.remove(&z).unwrap();
        levels.sort_by(|a, b| b.w.cmp(&a.w));
        Ok(NdpiSource { mmap, levels, icc })
    }

    fn data(&self) -> &[u8] { &self.mmap }
}

// ─── Decoding ─────────────────────────────────────────────────────────────────

thread_local! {
    static TJ_DEC: RefCell<Option<turbojpeg::Decompressor>> = const { RefCell::new(None) };
}

// Entropy-coded bytes of restart interval `i`, without its trailing RST/EOI marker.
fn interval<'a>(d: &'a [u8], lv: &Level, i: usize) -> &'a [u8] {
    let end = lv.mcu_starts.get(i + 1).map_or(lv.strip.1, |&o| o as usize);
    let (s, e) = ((lv.strip.0 + lv.mcu_starts[i] as usize).min(d.len()), (lv.strip.0 + end).min(d.len()));
    let b = &d[s.min(e)..e];
    match b {
        [.., 0xFF, m] if (0xD0..=0xD7).contains(m) || *m == 0xD9 => &b[..b.len() - 2],
        _ => b,
    }
}

// Decodes restart-interval column `col`, rows `rows`, at 1/2^`shift` scale.
// Returns packed RGB pixels with their width and height.
fn decode_block(d: &[u8], lv: &Level, col: u32, rows: Range<u32>, shift: u32) -> Option<(Vec<u8>, usize, usize)> {
    let col_w = lv.w.min((col + 1) * lv.tile_w) - col * lv.tile_w;
    let px_h = lv.h.min(rows.end * lv.tile_h) - rows.start * lv.tile_h;
    let mut jpeg = lv.header.clone();
    jpeg[lv.sof_off..lv.sof_off + 2].copy_from_slice(&(px_h as u16).to_be_bytes());
    jpeg[lv.sof_off + 2..lv.sof_off + 4].copy_from_slice(&(col_w as u16).to_be_bytes());
    for (k, r) in rows.enumerate() {
        if k > 0 { jpeg.extend_from_slice(&[0xFF, 0xD0 + ((k - 1) % 8) as u8]); }
        jpeg.extend_from_slice(interval(d, lv, (r * lv.cols + col) as usize));
    }
    jpeg.extend_from_slice(&[0xFF, 0xD9]);

    TJ_DEC.with(|cell| {
        let mut guard = cell.borrow_mut();
        if guard.is_none() { *guard = turbojpeg::Decompressor::new().ok(); }
        let dec = guard.as_mut()?;
        let sf = turbojpeg::ScalingFactor::new(1, 1 << shift);
        dec.set_scaling_factor(sf).ok()?;
        let hdr = dec.read_header(&jpeg).ok()?.scaled(sf);
        let (w, h) = (hdr.width, hdr.height);
        let mut px = vec![0u8; w * h * NCH];
        dec.decompress(&jpeg, turbojpeg::Image {
            pixels: px.as_mut_slice(), width: w, pitch: w * NCH, height: h,
            format: turbojpeg::PixelFormat::RGB,
        }).ok()?;
        Some((px, w, h))
    })
}

// A decoded rectangle of a level at 1/2^shift scale; (x0, y0) is its top-left
// corner in scaled level coordinates.
struct Band {
    px: Vec<u8>,
    w:  usize,
    h:  usize,
    x0: usize,
    y0: usize,
}

// Decodes the restart-interval rectangle `cols` x `rows` in parallel.
fn decode_band(d: &[u8], lv: &Level, cols: Range<u32>, rows: Range<u32>, shift: u32) -> Result<Band, String> {
    let s = 1usize << shift;
    // Interval sizes are multiples of 8, so they scale exactly for shift <= 3.
    let (tw, th) = (lv.tile_w as usize / s, lv.tile_h as usize / s);
    let (x0, y0) = (cols.start as usize * tw, rows.start as usize * th);
    let w = (lv.w as usize).min(cols.end as usize * lv.tile_w as usize).div_ceil(s) - x0;
    let h = (lv.h as usize).min(rows.end as usize * lv.tile_h as usize).div_ceil(s) - y0;

    let tasks: Vec<(u32, u32)> = cols.clone()
        .flat_map(|c| rows.clone().step_by(CHUNK_ROWS as usize).map(move |r| (c, r)))
        .collect();
    let blocks = tasks.par_iter().map(|&(c, r)| {
        let rr = r..(r + CHUNK_ROWS).min(rows.end);
        decode_block(d, lv, c, rr.clone(), shift)
            .map(|b| (c, r, b))
            .ok_or_else(|| format!("JPEG decode failed at interval column {c}, rows {}..{}", rr.start, rr.end))
    }).collect::<Result<Vec<_>, String>>()?;

    let mut px = vec![255u8; w * h * NCH];
    for (c, r, (b, bw, bh)) in blocks {
        let (ox, oy) = (c as usize * tw - x0, r as usize * th - y0);
        let cw = bw.min(w - ox) * NCH;
        for y in 0..bh.min(h - oy) {
            let dst = ((oy + y) * w + ox) * NCH;
            px[dst..dst + cw].copy_from_slice(&b[y * bw * NCH..y * bw * NCH + cw]);
        }
    }
    Ok(Band { px, w, h, x0, y0 })
}

// ─── Output rendering ─────────────────────────────────────────────────────────

// How the output base level maps onto a stored level: one output pixel spans
// `r` (x, y) level pixels; the decoder covers the 2^shift part of it.
struct Plan<'a> {
    lv:    &'a Level,
    shift: u32,
    r:     (f64, f64),
    exact: bool, // r == 2^shift: no resize, tiles are copied out of the band
    out:   (u32, u32),
}

fn plan(src: &NdpiSource, factor: f64) -> Plan<'_> {
    let base = &src.levels[0];
    let out = (((base.w as f64 / factor).round() as u32).max(1), ((base.h as f64 / factor).round() as u32).max(1));
    // Coarsest stored level that still has at least the output resolution.
    let lv = src.levels.iter().rev()
        .find(|l| l.w as f64 >= out.0 as f64 * 0.99 && l.h as f64 >= out.1 as f64 * 0.99)
        .unwrap_or(base);
    let r = (lv.w as f64 / out.0 as f64, lv.h as f64 / out.1 as f64);
    let shift = (0..=3u32).rev().find(|&k| (1u32 << k) as f64 <= r.0.min(r.1) * 1.01).unwrap_or(0);
    let s = (1u32 << shift) as f64;
    if (r.0 / s - 1.0).abs() < 0.01 && (r.1 / s - 1.0).abs() < 0.01 {
        let out = (lv.w.div_ceil(1 << shift), lv.h.div_ceil(1 << shift));
        Plan { lv, shift, r: (s, s), exact: true, out }
    } else {
        Plan { lv, shift, r, exact: false, out }
    }
}

fn fir_alg(kernel: FilterType) -> fir::ResizeAlg {
    match kernel {
        FilterType::Nearest    => fir::ResizeAlg::Nearest,
        FilterType::Triangle   => fir::ResizeAlg::Convolution(fir::FilterType::Bilinear),
        FilterType::CatmullRom => fir::ResizeAlg::Convolution(fir::FilterType::CatmullRom),
        FilterType::Gaussian   => fir::ResizeAlg::Convolution(fir::FilterType::Lanczos3),
        FilterType::Lanczos3   => fir::ResizeAlg::Convolution(fir::FilterType::Lanczos3),
    }
}

// Renders output tile (tc, tr) of the uncropped base level from `band` into a
// white OUT_TILE x OUT_TILE canvas, then ICC-bakes and JPEG-encodes it.
fn render_tile(p: &Plan, band: &Band, (tc, tr): (u32, u32), alg: fir::ResizeAlg,
               icc: Option<&IccTransform>, quality: u8) -> Option<Vec<u8>> {
    let t = OUT_TILE as usize;
    let (ox, oy) = (tc * OUT_TILE, tr * OUT_TILE);
    let (w, h) = ((p.out.0 - ox).min(OUT_TILE) as usize, (p.out.1 - oy).min(OUT_TILE) as usize);
    let mut canvas = vec![255u8; t * t * NCH];
    let paste = |canvas: &mut [u8], src: &[u8], src_w: usize, (sx, sy): (usize, usize), (cw, ch): (usize, usize)| {
        for y in 0..ch {
            let s = ((sy + y) * src_w + sx) * NCH;
            canvas[y * t * NCH..y * t * NCH + cw * NCH].copy_from_slice(&src[s..s + cw * NCH]);
        }
    };

    if p.exact {
        let (bx, by) = (ox as usize - band.x0, oy as usize - band.y0);
        paste(&mut canvas, &band.px, band.w, (bx, by),
              (w.min(band.w.saturating_sub(bx)), h.min(band.h.saturating_sub(by))));
    } else {
        // Source rectangle of this tile in band coordinates (fractional).
        let s = (1u32 << p.shift) as f64;
        let (kx, ky) = (p.r.0 / s, p.r.1 / s);
        let left = ox as f64 * kx - band.x0 as f64;
        let top = oy as f64 * ky - band.y0 as f64;
        let cw = (w as f64 * kx).min(band.w as f64 - left);
        let chh = (h as f64 * ky).min(band.h as f64 - top);
        let src = fir::images::ImageRef::new(band.w as u32, band.h as u32, &band.px, fir::PixelType::U8x3).ok()?;
        let mut dst = fir::images::Image::new(w as u32, h as u32, fir::PixelType::U8x3);
        let opts = fir::ResizeOptions::new().resize_alg(alg).crop(left, top, cw, chh);
        fir::Resizer::new().resize(&src, &mut dst, &opts).ok()?;
        paste(&mut canvas, dst.buffer(), w, (0, 0), (w, h));
    }

    if let Some(xf) = icc {
        let mut out = vec![0u8; canvas.len()];
        apply_icc(xf, &canvas, &mut out);
        canvas = out;
    }
    turbojpeg::compress(turbojpeg::Image::<&[u8]> {
        pixels: &canvas, width: t, pitch: t * NCH, height: t, format: turbojpeg::PixelFormat::RGB,
    }, quality as i32, turbojpeg::Subsamp::Sub2x2).ok().map(|j| j.to_vec())
}

// ─── Conversion entry point ───────────────────────────────────────────────────

pub(crate) struct Converted {
    pub in_dim:  (u32, u32),
    pub out_dim: (u32, u32),
    pub in_mpp:  f64,
    pub out_mpp: f64,
}

pub(crate) enum Outcome {
    Converted(Converted),
    Skipped(String),
}

pub(crate) fn convert_ndpi(
    ndpi_path: &str,
    out_path: &str,
    args: &crate::Args,
    roi: Option<&crate::roi::Roi>,
    pb: Option<&ProgressBar>,
) -> Result<Outcome, String> {
    let src = NdpiSource::open(ndpi_path)?;
    let d = src.data();
    let base = &src.levels[0];
    let (mpp_x, mpp_y) = base.mpp;

    // Downsample factor of the output base relative to level 0.
    let factor = match args.scale {
        None => 1.0,
        Some(Scale::Half) => 2.0,
        Some(Scale::Quarter) => 4.0,
        Some(Scale::Mag20x) => crate::factor_to_20x(mpp_x)
            .ok_or("source MPP unknown or ≥0.7 µm/px (--scale 20x cannot upscale)")? as f64,
        Some(Scale::Mpp(target)) => {
            if mpp_x <= 0.0 { return Err("source MPP unknown (--scale <mpp> requires it)".into()); }
            if target < mpp_x * 1.1 {
                if args.verbose {
                    vlog(pb, format!("  [ndpi ] requested MPP {target:.4} not >10% coarser than source {mpp_x:.4} µm/px; using full resolution"));
                }
                1.0
            } else {
                target / mpp_x
            }
        }
    };
    if factor <= 1.0 && roi.is_none() {
        return Ok(Outcome::Skipped("full-resolution output requires --roi (NDPI strips cannot be repacked)".into()));
    }

    let p = plan(&src, factor);
    let lv = p.lv;
    let out_mpp = (mpp_x * base.w as f64 / p.out.0 as f64, mpp_y * base.h as f64 / p.out.1 as f64);
    if args.verbose {
        vlog(pb, format!(
            "  [ndpi ] base {}x{} {:.4} µm/px  ← level {}x{} (interval {}x{})  1/{} DCT{}  → {}x{} {:.4} µm/px",
            base.w, base.h, mpp_x, lv.w, lv.h, lv.tile_w, lv.tile_h, 1u32 << p.shift,
            if p.exact { String::new() } else { format!(" + resize x{:.3}", (1u32 << p.shift) as f64 / p.r.0) },
            p.out.0, p.out.1, out_mpp.0,
        ));
    }

    // --roi: keep only the tiles touching an annotation, cropped to their bounding box.
    let grid = (p.out.0.div_ceil(OUT_TILE), p.out.1.div_ceil(OUT_TILE));
    let crop = roi.map(|r| crate::roi::RoiCrop::from_mask(
        &r.tile_mask(grid, (OUT_TILE, OUT_TILE), p.out, (base.w, base.h)), grid)
        .ok_or("--roi: no annotation overlaps the slide")).transpose()?;
    let (c0, r0, cols, rows) = crop.as_ref().map_or((0, 0, grid.0, grid.1), |c| (c.c0, c.r0, c.cols, c.rows));
    let dim = crop.as_ref().map_or(p.out, |c| c.dim(p.out, (OUT_TILE, OUT_TILE)));
    let in_roi = |id: u32| crop.as_ref().is_none_or(|c| c.mask[id as usize]);
    if let (true, Some(c)) = (args.verbose, &crop) {
        vlog(pb, format!("  [roi  ] crop tiles {}x{} at ({}, {}) → {}x{}  {}/{} tiles inside annotations",
            c.cols, c.rows, c.c0, c.r0, dim.0, dim.1, c.mask.iter().filter(|&&b| b).count(), c.mask.len()));
    }

    let icc: Option<Arc<IccTransform>> = if args.icc_bake {
        let xf = src.icc.as_deref().and_then(crate::build_icc_transform);
        if xf.is_none() { vlog(pb, "  [warn ] NDPI has no usable ICC profile; --icc-bake ignored"); }
        xf
    } else { None };

    let n_reduced = crate::tiffds::roi_reduced_levels(dim);
    if let Some(pb) = pb {
        pb.set_length((cols * rows) as u64 + crate::tiffds::roi_reduced_tiles((cols, rows), n_reduced));
    }

    // ── Open output TIFF ──
    let ome = !args.openslide;
    let image_desc_c = if ome {
        let stem = Path::new(ndpi_path).file_stem().and_then(|s| s.to_str()).unwrap_or("image");
        let xml = crate::pipeline::ome::generate_tiff_ome_xml(stem, dim.0, dim.1, out_mpp.0, out_mpp.1, 3);
        Some(CString::new(xml).map_err(|e| e.to_string())?)
    } else { None };
    let out_c = CString::new(out_path).map_err(|e| e.to_string())?;
    let dst = unsafe { TIFFOpen(out_c.as_ptr(), c"w8".as_ptr()) };
    if dst.is_null() { return Err(format!("cannot create {out_path}")); }

    unsafe {
        if ome && n_reduced > 0 {
            let zeros = vec![0u64; n_reduced as usize];
            TIFFSetField(dst, TIFFTAG_SUBIFD, n_reduced, zeros.as_ptr());
        }
        set_tiff_ifd_tags(dst, 0, dim.0, dim.1, OUT_TILE, OUT_TILE,
            COMPRESSION_JPEG, PHOTOMETRIC_YCBCR, 3, out_mpp.0, out_mpp.1);
        TIFFSetField(dst, TIFFTAG_YCBCRSUBSAMPLING, 2u32, 2u32);
        if let Some(ref desc) = image_desc_c {
            TIFFSetField(dst, TIFFTAG_IMAGEDESCRIPTION, desc.as_ptr());
        }
        if let (false, Some(profile)) = (args.icc_bake, &src.icc) {
            TIFFSetField(dst, TIFFTAG_ICCPROFILE, profile.len() as u32, profile.as_ptr() as *const c_void);
        }
    }

    // ── Base level, one output tile row (band) at a time ──
    let alg = fir_alg(args.kernel);
    let white = crate::pipeline::encode::white_jpeg_tile(OUT_TILE, OUT_TILE, 3, false, (2, 2), args.quality);
    let mut reducer = crate::tiffds::Reducer::new((cols, rows), (OUT_TILE, OUT_TILE), 3, args.quality, None, false, false);
    let mut jpegtables_registered = false;
    // Extra level pixels decoded around a band so the resize kernel has context.
    let margin = if p.exact { 0.0 } else { 4.0 * p.r.0.max(p.r.1) };
    let result = (0..rows).try_for_each(|row| -> Result<(), String> {
        let ids: Vec<u32> = (row * cols..(row + 1) * cols).collect();
        let roi_cols: Vec<u32> = ids.iter().filter(|&&id| in_roi(id)).map(|&id| c0 + id % cols).collect();
        let tiles: Vec<(u32, Vec<u8>)> = match (roi_cols.first(), roi_cols.last()) {
            (Some(&ca), Some(&cb)) => {
                // Level-pixel rectangle covering the band's tiles inside the ROI.
                let tr = r0 + row;
                let x0 = (ca * OUT_TILE) as f64 * p.r.0 - margin;
                let x1 = ((cb + 1) * OUT_TILE).min(p.out.0) as f64 * p.r.0 + margin;
                let y0 = (tr * OUT_TILE) as f64 * p.r.1 - margin;
                let y1 = ((tr + 1) * OUT_TILE).min(p.out.1) as f64 * p.r.1 + margin;
                let ic = (x0.max(0.0) as u32 / lv.tile_w)..((x1.ceil() as u32).div_ceil(lv.tile_w)).min(lv.cols);
                let ir = (y0.max(0.0) as u32 / lv.tile_h)..((y1.ceil() as u32).div_ceil(lv.tile_h)).min(lv.rows);
                let band = decode_band(d, lv, ic, ir, p.shift)?;
                let mut tiles = ids.par_iter().map(|&id| {
                    if !in_roi(id) { return Ok((id, white.clone())); }
                    render_tile(&p, &band, (c0 + id % cols, tr), alg, icc.as_deref(), args.quality)
                        .map(|j| (id, j))
                        .ok_or_else(|| format!("failed to render tile ({}, {tr})", c0 + id % cols))
                }).collect::<Result<Vec<_>, String>>()?;
                tiles.sort_unstable_by_key(|(id, _)| *id);
                tiles
            }
            _ => ids.iter().map(|&id| (id, white.clone())).collect(),
        };
        unsafe { write_enc_chunk(dst, &tiles, &mut jpegtables_registered); }
        for (id, jpeg) in &tiles { reducer.push(*id, jpeg); }
        if let Some(pb) = pb { pb.inc(tiles.len() as u64); }
        Ok(())
    });
    if let Err(e) = result {
        unsafe { TIFFClose(dst); }
        return Err(e);
    }

    unsafe {
        TIFFWriteDirectory(dst);
        crate::tiffds::write_reduced_levels(dst, reducer, dim, out_mpp, n_reduced, args.verbose, pb);
        TIFFClose(dst);
    }
    Ok(Outcome::Converted(Converted {
        in_dim: (base.w, base.h), out_dim: dim, in_mpp: mpp_x, out_mpp: out_mpp.0,
    }))
}
