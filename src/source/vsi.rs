// Experimental Olympus CellSens (.vsi / .ets) reader and converter.
//
// A .vsi file is a TIFF-headed metadata container; the pixel data lives in
// external tile-stream (.ets) files under a sibling "_<name>_" directory.
// This module parses just enough metadata to locate the highest-resolution
// 2D pyramid, decodes its tiles, and transcodes them into a tiled pyramidal
// TIFF / OME-TIFF using the shared libtiff writer helpers.
//
// Scope (experimental): 8-bit images only. Z-stacks and time series collapse
// to a single plane (the first index). Multi-channel fluorescence is not
// composited.

use std::ffi::CString;
use std::path::{Path, PathBuf};

use indicatif::ProgressBar;
use rayon::prelude::*;

use crate::bindings::{
    TIFFOpen, TIFFClose, TIFFSetField, TIFFWriteDirectory,
    TIFFTAG_IMAGEDESCRIPTION, TIFFTAG_SUBIFD, TIFFTAG_YCBCRSUBSAMPLING,
    PHOTOMETRIC_YCBCR, PHOTOMETRIC_MINISBLACK, COMPRESSION_JPEG, FILETYPE_REDUCEDIMAGE,
};
use crate::{set_tiff_ifd_tags, write_enc_chunk, vlog};

// ─── Little-endian scalar readers ─────────────────────────────────────────────

fn rd_i32(d: &[u8], o: usize) -> i32 { i32::from_le_bytes([d[o], d[o + 1], d[o + 2], d[o + 3]]) }
fn rd_u32(d: &[u8], o: usize) -> u32 { u32::from_le_bytes([d[o], d[o + 1], d[o + 2], d[o + 3]]) }
fn rd_i64(d: &[u8], o: usize) -> i64 {
    i64::from_le_bytes([d[o], d[o+1], d[o+2], d[o+3], d[o+4], d[o+5], d[o+6], d[o+7]])
}
fn rd_f64(d: &[u8], o: usize) -> f64 {
    f64::from_le_bytes([d[o], d[o+1], d[o+2], d[o+3], d[o+4], d[o+5], d[o+6], d[o+7]])
}

// ─── VSI metadata (tag container) ─────────────────────────────────────────────

// Tag IDs needed to identify pyramids and their physical size.
const TAG_IMAGE_FRAME:        i32 = 2002;
const TAG_EXTERNAL_FILE_PROPS: i32 = 2018;
const TAG_RWC_FRAME_SCALE:    i32 = 2019;
const TAG_STACK_NAME:         i32 = 2030;
const TAG_IMAGE_BOUNDARY:     i32 = 2053;
const TAG_SLIDE_PROPS:        i32 = 2062;
const TAG_DOCUMENT_PROPS:     i32 = 2109;
const TAG_HAS_EXTERNAL_FILE:  i32 = 20005;

#[derive(Default, Clone)]
pub(crate) struct VsiPyramid {
    pub(crate) name:   Option<String>,
    pub(crate) width:  Option<u32>,
    pub(crate) height: Option<u32>,
    pub(crate) psx:    Option<f64>,
    pub(crate) psy:    Option<f64>,
}

struct TagParser<'a> {
    d:           &'a [u8],
    pyramids:    Vec<VsiPyramid>,
    meta_index:  i32,
    prev_tag:    i32,
}

impl<'a> TagParser<'a> {
    fn ensure_pyramid(&mut self) {
        while self.meta_index >= self.pyramids.len() as i32 {
            self.pyramids.push(VsiPyramid::default());
        }
    }

    // Parse one tag container starting at `base`; returns the file offset just
    // past the container so a NEW_VOLUME_HEADER loop can read sibling volumes.
    // `nextField` offsets are relative to the container base, matching the
    // CellSens layout.
    fn read_tags(&mut self, base: usize) -> usize {
        let d = self.d;
        let n = d.len();
        if base + 24 >= n { return base; }

        let data_off = rd_i64(d, base + 8) as usize;
        let flags    = rd_u32(d, base + 16);
        let tag_count = flags & 0x0FFF_FFFF;

        let mut pos = base.saturating_add(data_off);
        let mut ret = pos;

        for _ in 0..tag_count {
            if pos + 16 >= n { break; }
            let fp = pos;
            let field_type = rd_u32(d, fp);
            let tag        = rd_i32(d, fp + 4);
            let next_field = rd_u32(d, fp + 8) as usize;
            let data_size  = rd_i32(d, fp + 12).max(0) as usize;
            let mut p = fp + 16;

            let extra_tag = (field_type >> 27) & 1 == 1;
            let extended  = (field_type >> 28) & 1 == 1;
            let inline    = (field_type >> 30) & 1 == 1;
            let real_type = field_type & 0x00FF_FFFF;

            if extra_tag { p += 4; }
            if tag < 0 { return p; }

            if tag == TAG_EXTERNAL_FILE_PROPS && self.prev_tag == TAG_IMAGE_FRAME {
                self.meta_index += 1;
            } else if tag == TAG_DOCUMENT_PROPS || tag == TAG_SLIDE_PROPS {
                self.meta_index = -1;
            }
            self.prev_tag = tag;
            self.ensure_pyramid();

            if extended && real_type == 0 {
                // NEW_VOLUME_HEADER: recurse through nested sibling containers.
                let end = (p + data_size).min(n);
                let mut cur = p;
                while cur < end {
                    let nxt = self.read_tags(cur);
                    if nxt <= cur { break; }
                    cur = nxt;
                }
            } else if extended && (real_type == 1 || real_type == 2) {
                // PROPERTY_SET / NEW_MDIM_VOLUME: single nested container.
                self.read_tags(p);
            } else if p <= n {
                self.consume_value(tag, real_type, inline, data_size, p);
            }

            if next_field == 0 {
                if base + data_size + 32 < n { return base + data_size + 32; }
                return pos;
            }
            let np = base.saturating_add(next_field);
            if np < n { pos = np; ret = np; } else { break; }
        }
        ret
    }

    fn consume_value(&mut self, tag: i32, real_type: u32, inline: bool, data_size: usize, p: usize) {
        let d = self.d;
        if self.meta_index < 0 || inline || data_size == 0 {
            // Inline ints (e.g. HAS_EXTERNAL_FILE) carry their value in data_size.
            if tag == TAG_HAS_EXTERNAL_FILE && self.meta_index >= 0 { /* presence only */ }
            return;
        }
        let idx = self.meta_index as usize;
        if idx >= self.pyramids.len() { return; }
        match real_type {
            13 | 8192 => {
                // TCHAR (8-bit) or UNICODE_TCHAR (UTF-16LE).
                let raw = &d[p..(p + data_size).min(d.len())];
                let s = if real_type == 8192 {
                    let units: Vec<u16> = raw.chunks_exact(2)
                        .map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
                    String::from_utf16_lossy(&units)
                } else {
                    raw.iter().map(|&b| b as char).collect()
                };
                let s = s.split('\u{0}').next().unwrap_or("").trim().to_string();
                if tag == TAG_STACK_NAME && s != "0" && !s.is_empty()
                    && self.pyramids[idx].name.is_none()
                {
                    self.pyramids[idx].name = Some(s);
                }
            }
            256 | 257 | 258 | 259 if data_size >= 16 => {
                // INT_RECT etc.: IMAGE_BOUNDARY = [x, y, width, height].
                if tag == TAG_IMAGE_BOUNDARY && self.pyramids[idx].width.is_none() {
                    self.pyramids[idx].width  = Some(rd_i32(d, p + 8).max(0) as u32);
                    self.pyramids[idx].height = Some(rd_i32(d, p + 12).max(0) as u32);
                }
            }
            260 if data_size >= 16 => {
                // DOUBLE_2: RWC_FRAME_SCALE = physical pixel size (µm).
                if tag == TAG_RWC_FRAME_SCALE && self.pyramids[idx].psx.is_none() {
                    self.pyramids[idx].psx = Some(rd_f64(d, p));
                    self.pyramids[idx].psy = Some(rd_f64(d, p + 8));
                }
            }
            _ => {}
        }
    }
}

fn parse_vsi_metadata(data: &[u8]) -> Vec<VsiPyramid> {
    let mut parser = TagParser { d: data, pyramids: Vec::new(), meta_index: -1, prev_tag: 0 };
    parser.read_tags(8);
    parser.pyramids
}

// ─── ETS (external tile stream) ───────────────────────────────────────────────

struct Chunk {
    coord:  Vec<i32>,
    offset: u64,
    nbytes: u32,
}

pub(crate) struct Ets {
    bytes:           Vec<u8>,
    n_dim:           usize,
    pixel_type:      i32,
    size_c:          i32,
    comp_type:       i32,
    tile_w:          u32,
    tile_h:          u32,
    component_order: i32,
    use_pyramid:     bool,
    background:      Vec<u8>,
    chunks:          Vec<Chunk>,
    max_res:         u32,
}

const COMP_RAW: i32 = 0;
const COMP_JPEG: i32 = 2;
const COMP_JPEG2000: i32 = 3;
const COMP_PNG: i32 = 8;
const COMP_BMP: i32 = 9;
const PIXEL_UCHAR: i32 = 2;

impl Ets {
    fn parse(path: &Path) -> Option<Ets> {
        let bytes = std::fs::read(path).ok()?;
        if bytes.len() < 48 || &bytes[0..3] != b"SIS" { return None; }

        let n_dim          = rd_i32(&bytes, 12) as usize;
        let add_off        = rd_i64(&bytes, 16) as usize;
        let used_chunk_off = rd_i64(&bytes, 32) as usize;
        let n_used         = rd_i32(&bytes, 40).max(0) as usize;
        if n_dim == 0 || n_dim > 16 || add_off + 160 > bytes.len() { return None; }
        if &bytes[add_off..add_off + 3] != b"ETS" { return None; }

        let pixel_type = rd_i32(&bytes, add_off + 8);
        let size_c     = rd_i32(&bytes, add_off + 12);
        let comp_type  = rd_i32(&bytes, add_off + 20);
        let tile_w     = rd_i32(&bytes, add_off + 28).max(0) as u32;
        let tile_h     = rd_i32(&bytes, add_off + 32).max(0) as u32;

        // backgroundColor: size_c * bpp bytes, then padded to 40 bytes total.
        let bpp = bytes_per_pixel(pixel_type);
        let bg_len = (size_c.max(0) as usize) * bpp;
        let bg_start = add_off + 108;
        let background = bytes.get(bg_start..bg_start + bg_len)
            .map(|s| s.to_vec()).unwrap_or_default();
        let component_order = rd_i32(&bytes, add_off + 148);
        let use_pyramid = rd_i32(&bytes, add_off + 152) != 0;

        // Used-chunk table.
        let entry = 4 + n_dim * 4 + 8 + 4 + 4;
        let mut chunks = Vec::with_capacity(n_used);
        let mut max_res = 0u32;
        for i in 0..n_used {
            let b = used_chunk_off + i * entry;
            if b + entry > bytes.len() { break; }
            let mut coord = Vec::with_capacity(n_dim);
            for j in 0..n_dim {
                coord.push(rd_i32(&bytes, b + 4 + j * 4));
            }
            let off    = rd_i64(&bytes, b + 4 + n_dim * 4) as u64;
            let nbytes = rd_u32(&bytes, b + 4 + n_dim * 4 + 8);
            if use_pyramid {
                max_res = max_res.max(coord[n_dim - 1].max(0) as u32);
            }
            chunks.push(Chunk { coord, offset: off, nbytes });
        }

        Some(Ets {
            bytes, n_dim, pixel_type, size_c, comp_type,
            tile_w, tile_h, component_order, use_pyramid,
            background, chunks, max_res,
        })
    }

    fn n_res(&self) -> u32 { if self.use_pyramid { self.max_res + 1 } else { 1 } }

    // Largest (x, y) tile index seen at the given resolution level.
    fn max_xy_at_res(&self, res: u32) -> (u32, u32) {
        let last = self.n_dim - 1;
        let (mut mx, mut my) = (0u32, 0u32);
        for c in &self.chunks {
            let r = if self.use_pyramid { c.coord[last].max(0) as u32 } else { 0 };
            if r == res {
                mx = mx.max(c.coord[0].max(0) as u32);
                my = my.max(c.coord[1].max(0) as u32);
            }
        }
        (mx, my)
    }

    // Full-resolution pixel area derived from tile counts (orphan fallback).
    fn res0_area(&self) -> u64 {
        let (mx, my) = self.max_xy_at_res(0);
        (mx as u64 + 1) * self.tile_w as u64 * (my as u64 + 1) * self.tile_h as u64
    }

    // Index chunks of the first 2D plane (non-spatial dims fixed to 0) by
    // (resolution, x, y) for O(1) tile lookup.
    fn plane_index(&self) -> std::collections::HashMap<(u32, u32, u32), usize> {
        let last = self.n_dim - 1;
        let mid_end = if self.use_pyramid { last } else { self.n_dim };
        let mut map = std::collections::HashMap::new();
        for (i, c) in self.chunks.iter().enumerate() {
            if !(2..mid_end).all(|j| c.coord[j] == 0) { continue; }
            let r = if self.use_pyramid { c.coord[last].max(0) as u32 } else { 0 };
            map.insert((r, c.coord[0].max(0) as u32, c.coord[1].max(0) as u32), i);
        }
        map
    }

    fn is_supported(&self) -> bool {
        self.pixel_type == PIXEL_UCHAR
            && (self.size_c == 1 || self.size_c == 3)
            && matches!(self.comp_type, COMP_RAW | COMP_JPEG | COMP_JPEG2000 | COMP_PNG | COMP_BMP)
    }
}

fn bytes_per_pixel(pixel_type: i32) -> usize {
    match pixel_type {
        1 | 2 => 1,
        3 | 4 => 2,
        5 | 6 | 9 => 4,
        7 | 8 | 10 => 8,
        _ => 1,
    }
}

// ─── Tile decoding ────────────────────────────────────────────────────────────

// Decode one ETS tile to interleaved 8-bit pixels (size_c channels), padded /
// background-filled to exactly tile_w × tile_h.
fn decode_tile(ets: &Ets, chunk: Option<&Chunk>, spp: usize) -> Vec<u8> {
    let tw = ets.tile_w as usize;
    let th = ets.tile_h as usize;
    let full = tw * th * spp;

    let Some(chunk) = chunk else {
        return background_tile(ets, full, spp);
    };
    let start = chunk.offset as usize;
    let endb  = (start + chunk.nbytes as usize).min(ets.bytes.len());
    if start >= endb { return background_tile(ets, full, spp); }
    let data = &ets.bytes[start..endb];

    let decoded: Option<Vec<u8>> = match ets.comp_type {
        COMP_JPEG => {
            let fmt = if spp == 1 { turbojpeg::PixelFormat::GRAY } else { turbojpeg::PixelFormat::RGB };
            turbojpeg::decompress(data, fmt).ok().map(|img| repack(img, spp))
        }
        COMP_JPEG2000 => {
            jpeg2k::Image::from_bytes_with(data, jpeg2k::DecodeParameters::default()).ok()
                .and_then(|img| crate::jp2k_assemble_pixels(&img, spp).map(|(p, _, _)| p))
        }
        COMP_RAW => {
            let mut v = data.to_vec();
            v.resize(full, 0);
            if spp == 3 && ets.component_order == 1 {
                for px in v.chunks_exact_mut(3) { px.swap(0, 2); }
            }
            Some(v)
        }
        COMP_PNG | COMP_BMP => {
            image::load_from_memory(data).ok().map(|img| {
                if spp == 1 { img.to_luma8().into_raw() } else { img.to_rgb8().into_raw() }
            })
        }
        _ => None,
    };

    match decoded {
        Some(mut v) if v.len() >= full => { v.truncate(full); v }
        Some(mut v) => { v.resize(full, 0); v }
        None => background_tile(ets, full, spp),
    }
}

fn repack(img: turbojpeg::Image<Vec<u8>>, spp: usize) -> Vec<u8> {
    let row = img.width * spp;
    if img.pitch == row {
        img.pixels
    } else {
        (0..img.height)
            .flat_map(|r| img.pixels[r * img.pitch..r * img.pitch + row].iter().copied())
            .collect()
    }
}

fn background_tile(ets: &Ets, full: usize, spp: usize) -> Vec<u8> {
    if ets.background.len() >= spp && ets.background.iter().any(|&b| b != 0) {
        let bg = &ets.background[..spp];
        let mut v = vec![0u8; full];
        for px in v.chunks_exact_mut(spp) { px.copy_from_slice(bg); }
        v
    } else {
        // Default to white, the usual brightfield slide background.
        vec![0xFFu8; full]
    }
}

fn encode_tile(pixels: &[u8], tw: usize, th: usize, spp: usize, quality: u8) -> Option<Vec<u8>> {
    if spp == 1 {
        turbojpeg::compress(turbojpeg::Image::<&[u8]> {
            pixels, width: tw, pitch: tw, height: th, format: turbojpeg::PixelFormat::GRAY,
        }, quality as i32, turbojpeg::Subsamp::Gray).ok().map(|b| b.to_vec())
    } else {
        turbojpeg::compress(turbojpeg::Image::<&[u8]> {
            pixels, width: tw, pitch: tw * 3, height: th, format: turbojpeg::PixelFormat::RGB,
        }, quality as i32, turbojpeg::Subsamp::Sub2x2).ok().map(|b| b.to_vec())
    }
}

// ─── File discovery ───────────────────────────────────────────────────────────

// Collect "frame*.ets" files under the "_<stem>_" sibling directory, sorted by
// path so their order matches the metadata pyramid order.
fn collect_ets_files(vsi_path: &Path) -> Vec<PathBuf> {
    let stem = match vsi_path.file_stem().and_then(|s| s.to_str()) {
        Some(s) => s,
        None => return Vec::new(),
    };
    let dir = vsi_path.with_file_name(format!("_{}_", stem));
    if !dir.is_dir() { return Vec::new(); }

    let mut out = Vec::new();
    for entry in walkdir::WalkDir::new(&dir).into_iter().filter_map(|e| e.ok()) {
        let p = entry.path();
        if !p.is_file() { continue; }
        let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if name.starts_with("frame") && name.ends_with(".ets") {
            out.push(p.to_path_buf());
        }
    }
    out.sort();
    out
}

// ─── Output pyramid level ─────────────────────────────────────────────────────

struct VsiLevel {
    res:   u32,
    img_w: u32,
    img_h: u32,
    mpp_x: f64,
    mpp_y: f64,
}

// Build the output pyramid following the CellSens halving rule: each level is
// half the previous, clamped to the tile-grid extent at that resolution.
fn build_levels(ets: &Ets, pyr: &VsiPyramid) -> Vec<VsiLevel> {
    let (mx0, my0) = ets.max_xy_at_res(0);
    let base_w = pyr.width.filter(|&w| w > 0).unwrap_or((mx0 + 1) * ets.tile_w);
    let base_h = pyr.height.filter(|&h| h > 0).unwrap_or((my0 + 1) * ets.tile_h);
    let base_mpp_x = pyr.psx.unwrap_or(0.0);
    let base_mpp_y = pyr.psy.unwrap_or(0.0);

    let mut levels = vec![VsiLevel {
        res: 0, img_w: base_w, img_h: base_h, mpp_x: base_mpp_x, mpp_y: base_mpp_y,
    }];

    for r in 1..ets.n_res() {
        let (mx, my) = ets.max_xy_at_res(r);
        let max_w = ets.tile_w * (mx + 1);
        let max_h = ets.tile_h * (my + 1);
        let prev = levels.last().unwrap();

        let mut w = prev.img_w / 2;
        if prev.img_w % 2 == 1 && w < max_w { w += 1; } else if w > max_w { w = max_w; }
        let mut h = prev.img_h / 2;
        if prev.img_h % 2 == 1 && h < max_h { h += 1; } else if h > max_h { h = max_h; }
        if w == 0 || h == 0 { break; }

        let scale = (1u32 << r) as f64;
        levels.push(VsiLevel {
            res: r, img_w: w, img_h: h,
            mpp_x: if base_mpp_x > 0.0 { base_mpp_x * scale } else { 0.0 },
            mpp_y: if base_mpp_y > 0.0 { base_mpp_y * scale } else { 0.0 },
        });
    }
    levels
}

// ─── Conversion entry point ───────────────────────────────────────────────────

pub(crate) fn convert_vsi(
    vsi_path: &str,
    out_path: &str,
    legacy: bool,
    quality: u8,
    verbose: bool,
    pb: Option<&ProgressBar>,
) -> Result<(), String> {
    let vp = Path::new(vsi_path);
    let vsi_bytes = std::fs::read(vp).map_err(|e| format!("read vsi: {e}"))?;
    let pyramids = parse_vsi_metadata(&vsi_bytes);
    let ets_files = collect_ets_files(vp);
    if ets_files.is_empty() {
        return Err("no .ets pixel data found".to_string());
    }

    // Parse every ETS and pair it with metadata by position (when counts match).
    let mut candidates: Vec<(Ets, VsiPyramid)> = Vec::new();
    for (i, ep) in ets_files.iter().enumerate() {
        let Some(ets) = Ets::parse(ep) else { continue; };
        let meta = if pyramids.len() == ets_files.len() {
            pyramids[i].clone()
        } else {
            VsiPyramid::default()
        };
        candidates.push((ets, meta));
    }
    if candidates.is_empty() {
        return Err("no readable .ets stream found".to_string());
    }

    // Identify the true main image: finest physical pixel size, then largest
    // area. Label/overview/macro streams are coarser, so this picks the real
    // scan even when it later proves unsupported.
    let best = candidates.iter().enumerate().min_by(|(_, (ea, ma)), (_, (eb, mb))| {
        let ka = ma.psx.unwrap_or(f64::MAX);
        let kb = mb.psx.unwrap_or(f64::MAX);
        ka.partial_cmp(&kb).unwrap_or(std::cmp::Ordering::Equal)
            .then(eb.res0_area().cmp(&ea.res0_area()))
    }).map(|(i, _)| i).unwrap();
    let (ets, meta) = &candidates[best];

    if !ets.is_supported() {
        return Err(format!(
            "main image unsupported (pixelType={}, channels={}, compression={}); \
             only 8-bit images are handled",
            ets.pixel_type, ets.size_c, ets.comp_type,
        ));
    }

    let spp = ets.size_c as usize;
    let levels = build_levels(ets, meta);
    if verbose {
        vlog(pb, format!(
            "  [vsi  ] main '{}'  {}x{}  {} levels  spp={}  comp={}",
            meta.name.as_deref().unwrap_or("?"),
            levels[0].img_w, levels[0].img_h, levels.len(), spp, ets.comp_type,
        ));
    }

    let total_tiles: u64 = levels.iter().map(|lv| {
        let ntx = (lv.img_w + ets.tile_w - 1) / ets.tile_w;
        let nty = (lv.img_h + ets.tile_h - 1) / ets.tile_h;
        (ntx * nty) as u64
    }).sum();
    if let Some(p) = pb { p.set_length(total_tiles); }

    let ome = !legacy;
    let out_photometric = if spp == 1 { PHOTOMETRIC_MINISBLACK } else { PHOTOMETRIC_YCBCR };

    let image_desc_c: Option<CString> = if ome {
        let stem = vp.file_stem().and_then(|s| s.to_str()).unwrap_or("image");
        let xml = crate::pipeline::ome::generate_tiff_ome_xml(
            stem, levels[0].img_w, levels[0].img_h, levels[0].mpp_x, levels[0].mpp_y, spp as u32,
        );
        Some(CString::new(xml).map_err(|e| e.to_string())?)
    } else {
        None
    };

    let out_c   = CString::new(out_path).map_err(|e| e.to_string())?;
    let w8_mode = CString::new("w8").unwrap();
    let dst = unsafe { TIFFOpen(out_c.as_ptr(), w8_mode.as_ptr()) };
    if dst.is_null() { return Err(format!("cannot create {out_path}")); }

    let n_subifds = levels.len().saturating_sub(1);
    if ome && n_subifds > 0 {
        let zeros: Vec<u64> = vec![0u64; n_subifds];
        unsafe { TIFFSetField(dst, TIFFTAG_SUBIFD, n_subifds as u32, zeros.as_ptr()); }
    }

    let tw = ets.tile_w as usize;
    let th = ets.tile_h as usize;
    let index = ets.plane_index();

    for (lv_idx, lv) in levels.iter().enumerate() {
        let subfile = if lv_idx == 0 { 0u32 } else { FILETYPE_REDUCEDIMAGE };
        unsafe {
            set_tiff_ifd_tags(dst, subfile,
                lv.img_w, lv.img_h, ets.tile_w, ets.tile_h,
                COMPRESSION_JPEG, out_photometric, spp as u32,
                lv.mpp_x, lv.mpp_y);
            if out_photometric == PHOTOMETRIC_YCBCR {
                TIFFSetField(dst, TIFFTAG_YCBCRSUBSAMPLING as u32, 2u32, 2u32);
            }
            if lv_idx == 0 {
                if let Some(ref desc) = image_desc_c {
                    TIFFSetField(dst, TIFFTAG_IMAGEDESCRIPTION as u32, desc.as_ptr());
                }
            }
        }

        let ntx = (lv.img_w + ets.tile_w - 1) / ets.tile_w;
        let nty = (lv.img_h + ets.tile_h - 1) / ets.tile_h;
        let ids: Vec<u32> = (0..ntx * nty).collect();

        // Decode + re-encode every tile in parallel, then write in tile order.
        let mut encoded: Vec<(u32, Vec<u8>)> = ids.par_iter().filter_map(|&id| {
            let tc = id % ntx;
            let tr = id / ntx;
            let chunk = index.get(&(lv.res, tc, tr)).map(|&i| &ets.chunks[i]);
            let pixels = decode_tile(ets, chunk, spp);
            encode_tile(&pixels, tw, th, spp, quality).map(|j| (id, j))
        }).collect();
        encoded.sort_unstable_by_key(|(id, _)| *id);

        let mut registered = false;
        unsafe { write_enc_chunk(dst, &encoded, &mut registered); }
        if let Some(p) = pb { p.inc(encoded.len() as u64); }

        unsafe { TIFFWriteDirectory(dst); }
    }

    unsafe { TIFFClose(dst); }
    Ok(())
}
