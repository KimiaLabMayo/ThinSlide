// MIRAX (3DHistech, .mrxs) reader and converter.
//
// A .mrxs file is an empty marker; all data lives in a same-named directory:
//   Slidedat.ini   — UTF-8(BOM) INI with all metadata (tile size, levels, files)
//   Index.dat      — little-endian i32 binary index (linked-list data pages)
//   Data####.dat   — concatenated standalone JPEG tiles (level 0 + pyramid)
//
// We read only level 0 (highest resolution): a set of standalone JPEG camera
// captures placed on the slide at integer pixel positions (see MiraxSource::open).
// The output pyramid is rebuilt here by downsampling level 0; the scanner's own
// reduced zoom levels are not read (their concat/subtile layout is intricate).
//
// Tiles overlap on the slide; placement is rounded to integer pixels and OVERLAP
// regions are simply overwritten by later tiles (greedy, no sub-pixel
// compositing). Because the source tiles are arbitrarily placed and overlapping,
// they must be decoded and re-encoded onto a regular tile grid — unlike DICOM
// there is no lossless tile passthrough for MIRAX. JPEG tiles only.

use std::collections::HashMap;
use std::ffi::CString;
use std::fs;
use std::path::{Path, PathBuf};

use fast_image_resize as fir;
use indicatif::ProgressBar;
use rayon::prelude::*;

use crate::bindings::{
    TIFFOpen, TIFFClose, TIFFSetField, TIFFWriteDirectory,
    TIFFTAG_IMAGEDESCRIPTION, TIFFTAG_SUBIFD, TIFFTAG_YCBCRSUBSAMPLING,
    PHOTOMETRIC_YCBCR, COMPRESSION_JPEG, FILETYPE_REDUCEDIMAGE,
};
use crate::{set_tiff_ifd_tags, write_enc_chunk, vlog, MIN_PYRAMID_SIDE};

const INDEX_VERSION: &str = "01.02";
const SLIDE_POSITION_RECORD_SIZE: usize = 9;
const VALUE_SLIDE_ZOOM_LEVEL: &str = "Slide zoom level";
const VALUE_VIMSLIDE_POSITION_BUFFER: &str = "VIMSLIDE_POSITION_BUFFER";

// Output tile size (multiple of 16 for JPEG YCbCr MCU compliance).
const OUT_TILE: u32 = 512;
const NCH: usize = 3; // MIRAX JPEG tiles are RGB

// ── Public types ────────────────────────────────────────────────────────────

/// One placed level-0 tile: a standalone JPEG in `data_files[fileno]`,
/// positioned with its top-left at (place_x, place_y) in the normalized canvas.
struct SrcTile {
    fileno:  usize,
    offset:  u64,
    length:  u32,
    place_x: i32,
    place_y: i32,
}

struct MiraxSource {
    img_w:      u32,        // normalized logical canvas size (whole slide, level 0)
    img_h:      u32,
    tile_w:     u32,        // DIGITIZER_WIDTH
    tile_h:     u32,        // DIGITIZER_HEIGHT
    mpp_x:      f64,
    mpp_y:      f64,
    fill_rgb:   [u8; 3],    // background for sparse regions
    data_files: Vec<PathBuf>,
    tiles:      Vec<SrcTile>,
}

// ── INI parsing ─────────────────────────────────────────────────────────────

/// Minimal INI: section -> (key -> value). Case-sensitive keys (MIRAX mixes case).
struct Ini {
    sections: HashMap<String, HashMap<String, String>>,
}

impl Ini {
    fn parse(text: &str) -> Ini {
        let mut sections: HashMap<String, HashMap<String, String>> = HashMap::new();
        let mut cur = String::new();
        for raw in text.lines() {
            let line = raw.trim_start_matches('\u{feff}').trim();
            if line.is_empty() || line.starts_with(';') { continue; }
            if let Some(rest) = line.strip_prefix('[') {
                if let Some(name) = rest.strip_suffix(']') {
                    cur = name.to_string();
                    sections.entry(cur.clone()).or_default();
                }
                continue;
            }
            if let Some(eq) = line.find('=') {
                let key = line[..eq].trim().to_string();
                let val = line[eq + 1..].trim().to_string();
                sections.entry(cur.clone()).or_default().insert(key, val);
            }
        }
        Ini { sections }
    }

    fn get(&self, section: &str, key: &str) -> Option<&str> {
        self.sections.get(section)?.get(key).map(|s| s.as_str())
    }
    fn get_str(&self, section: &str, key: &str) -> Option<String> {
        self.get(section, key).map(|s| s.to_string())
    }
    fn get_i64(&self, section: &str, key: &str) -> Option<i64> {
        self.get(section, key)?.trim().parse().ok()
    }
    fn get_f64(&self, section: &str, key: &str) -> Option<f64> {
        self.get(section, key)?.trim().parse().ok()
    }
}

// ── Little-endian i32 reader over an in-memory buffer ────────────────────────

fn read_i32(buf: &[u8], pos: usize) -> Option<i32> {
    let end = pos.checked_add(4)?;
    if end > buf.len() { return None; }
    Some(i32::from_le_bytes(buf[pos..end].try_into().unwrap()))
}

// ── Slide dir & input size ────────────────────────────────────────────────────

// The data directory is the .mrxs path with its extension stripped.
fn slide_dir(mrxs_path: &Path) -> PathBuf {
    mrxs_path.with_extension("")
}

// Total on-disk input size: the .mrxs marker plus every file in the slide dir
// (Slidedat.ini, Index.dat and all Data####.dat tile streams).
pub fn input_size(mrxs_path: &str) -> u64 {
    let path = Path::new(mrxs_path);
    let marker = fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    let dir = slide_dir(path);
    let mut sum = marker;
    for entry in fs::read_dir(&dir).into_iter().flatten().flatten() {
        if let Ok(m) = entry.metadata() {
            if m.is_file() { sum += m.len(); }
        }
    }
    sum
}

// ── Source open ───────────────────────────────────────────────────────────────

impl MiraxSource {
    fn open(path: &str, verbose: bool) -> Option<MiraxSource> {
        // Detection: .mrxs extension + same-named dir with Slidedat.ini.
        let ext_ok = Path::new(path).extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("mrxs"))
            .unwrap_or(false);
        if !ext_ok { return None; }

        let dir = slide_dir(Path::new(path));
        let slidedat = dir.join("Slidedat.ini");
        if !slidedat.is_file() {
            if verbose { eprintln!("[mirax] no Slidedat.ini in {}", dir.display()); }
            return None;
        }

        let text = fs::read_to_string(&slidedat).ok()?;
        let ini = Ini::parse(&text);

        // [GENERAL]
        let slide_id = ini.get_str("GENERAL", "SLIDE_ID")?;
        let images_x = ini.get_i64("GENERAL", "IMAGENUMBER_X")? as i32;
        let images_y = ini.get_i64("GENERAL", "IMAGENUMBER_Y")? as i32;
        let div = ini.get_i64("GENERAL", "CameraImageDivisionsPerSide")
            .unwrap_or(1).max(1) as i32;

        // [HIERARCHICAL]: locate the "Slide zoom level" tree and level-0 section.
        let hier_count = ini.get_i64("HIERARCHICAL", "HIER_COUNT")? as i32;
        let mut zl_tree = -1;
        for i in 0..hier_count {
            if ini.get("HIERARCHICAL", &format!("HIER_{i}_NAME")) == Some(VALUE_SLIDE_ZOOM_LEVEL) {
                zl_tree = i;
                break;
            }
        }
        if zl_tree < 0 {
            if verbose { eprintln!("[mirax] no '{VALUE_SLIDE_ZOOM_LEVEL}' hierarchy"); }
            return None;
        }
        let index_filename = ini.get_str("HIERARCHICAL", "INDEXFILE")?;
        let level0_section = ini.get_str("HIERARCHICAL",
            &format!("HIER_{zl_tree}_VAL_0_SECTION"))?;

        // Position buffer record number: sum of NONHIER_*_COUNT for trees before
        // the VIMSLIDE_POSITION_BUFFER tree (its data is VAL_0 of that tree).
        let nonhier_count = ini.get_i64("HIERARCHICAL", "NONHIER_COUNT").unwrap_or(0) as i32;
        let mut position_record = -1i32;
        let mut acc = 0i32;
        for i in 0..nonhier_count {
            let name = ini.get("HIERARCHICAL", &format!("NONHIER_{i}_NAME"));
            let cnt = ini.get_i64("HIERARCHICAL", &format!("NONHIER_{i}_COUNT"))
                .unwrap_or(0) as i32;
            if name == Some(VALUE_VIMSLIDE_POSITION_BUFFER) { position_record = acc; break; }
            acc += cnt;
        }

        // [DATAFILE]
        let file_count = ini.get_i64("DATAFILE", "FILE_COUNT")? as i32;
        let mut data_files = Vec::with_capacity(file_count as usize);
        for i in 0..file_count {
            let name = ini.get_str("DATAFILE", &format!("FILE_{i}"))?;
            data_files.push(dir.join(name));
        }

        // Level-0 zoom section parameters.
        let image_format = ini.get_str(&level0_section, "IMAGE_FORMAT")
            .unwrap_or_default().to_uppercase();
        if image_format != "JPEG" {
            eprintln!("[mirax] unsupported IMAGE_FORMAT '{image_format}' (only JPEG)");
            return None;
        }
        let tile_w = ini.get_i64(&level0_section, "DIGITIZER_WIDTH")? as i32;
        let tile_h = ini.get_i64(&level0_section, "DIGITIZER_HEIGHT")? as i32;
        let overlap_x = ini.get_f64(&level0_section, "OVERLAP_X").unwrap_or(0.0);
        let overlap_y = ini.get_f64(&level0_section, "OVERLAP_Y").unwrap_or(0.0);
        let mpp_x = ini.get_f64(&level0_section, "MICROMETER_PER_PIXEL_X").unwrap_or(0.0);
        let mpp_y = ini.get_f64(&level0_section, "MICROMETER_PER_PIXEL_Y").unwrap_or(mpp_x);
        let fill_rgb = parse_fill_bgr(ini.get(&level0_section, "IMAGE_FILL_COLOR_BGR"));

        if images_x <= 0 || images_y <= 0 || tile_w <= 0 || tile_h <= 0 {
            return None;
        }

        // ── Index.dat ──
        let index_buf = fs::read(dir.join(&index_filename)).ok()?;
        // Verify version + slide id (no NUL terminators).
        let id_len = slide_id.len();
        if index_buf.len() < 5 + id_len { return None; }
        if &index_buf[0..5] != INDEX_VERSION.as_bytes() {
            if verbose { eprintln!("[mirax] Index.dat version mismatch"); }
            return None;
        }
        if &index_buf[5..5 + id_len] != slide_id.as_bytes() {
            if verbose { eprintln!("[mirax] Index.dat slide id mismatch"); }
            return None;
        }
        let hier_root = 5 + id_len;
        let nonhier_root = hier_root + 4;

        let entries = read_level0_entries(&index_buf, hier_root)?;
        if entries.is_empty() {
            if verbose { eprintln!("[mirax] no level-0 entries in Index.dat"); }
            return None;
        }

        // Camera stage positions (level 0).
        let positions_x = images_x / div;
        let npositions = (positions_x * (images_y / div)) as usize;
        let positions = load_positions(
            &index_buf, nonhier_root, position_record,
            &data_files, npositions,
            tile_w, tile_h, div, overlap_x, overlap_y, positions_x,
            verbose,
        );

        // ── Integer placement ──
        // place(x,y) = pos[cp] + tile * (grid % div),  cp = (y/div)*positions_x + (x/div)
        let mut placed: Vec<SrcTile> = Vec::with_capacity(entries.len());
        for e in &entries {
            let gx = e.image_index % images_x;
            let gy = e.image_index / images_x;
            if gx < 0 || gy < 0 || gy >= images_y { continue; }
            let xp = gx / div;
            let yp = gy / div;
            let cp = (yp * positions_x + xp) as usize;
            if cp >= positions.len() { continue; }
            let (px, py) = positions[cp];
            // Drop dummy (0,0) positions at non-origin cells (openslide behaviour).
            if px == 0 && py == 0 && (xp != 0 || yp != 0) { continue; }
            let place_x = px + tile_w * (gx % div);
            let place_y = py + tile_h * (gy % div);
            placed.push(SrcTile {
                fileno: e.fileno as usize, offset: e.offset as u64,
                length: e.length as u32, place_x, place_y,
            });
        }
        if placed.is_empty() { return None; }

        // Normalize to origin (0,0) and compute canvas size.
        let min_x = placed.iter().map(|t| t.place_x).min().unwrap();
        let min_y = placed.iter().map(|t| t.place_y).min().unwrap();
        let mut max_x = 0i64;
        let mut max_y = 0i64;
        for t in &mut placed {
            t.place_x -= min_x;
            t.place_y -= min_y;
            max_x = max_x.max(t.place_x as i64 + tile_w as i64);
            max_y = max_y.max(t.place_y as i64 + tile_h as i64);
        }
        let img_w = max_x as u32;
        let img_h = max_y as u32;

        if verbose {
            eprintln!("[mirax] {}×{} px  tile {}×{}  div={}  overlap {:.1}/{:.1}  {} tiles  posbuf={}",
                img_w, img_h, tile_w, tile_h, div, overlap_x, overlap_y,
                placed.len(), position_record >= 0);
        }

        Some(MiraxSource {
            img_w, img_h,
            tile_w: tile_w as u32, tile_h: tile_h as u32,
            mpp_x, mpp_y,
            fill_rgb,
            data_files,
            tiles: placed,
        })
    }
}

// ── Index.dat: level-0 hierarchical entries ──────────────────────────────────

struct HierEntry { image_index: i32, offset: i32, length: i32, fileno: i32 }

/// Walk the level-0 data-page linked list and collect all image entries.
fn read_level0_entries(buf: &[u8], hier_root: usize) -> Option<Vec<HierEntry>> {
    // hier_root -> pointer to the per-zoom-level pointer table; entry 0 = level 0.
    let table_ptr = read_i32(buf, hier_root)?;
    let listhead_ptr = read_i32(buf, table_ptr as usize)?; // zoom level 0
    // ListHead: [0 marker][data-page pointer]
    if read_i32(buf, listhead_ptr as usize)? != 0 { return None; }
    let mut page = read_i32(buf, listhead_ptr as usize + 4)?;

    let mut out = Vec::new();
    while page > 0 {
        let p = page as usize;
        let page_len = read_i32(buf, p)?;
        let next_ptr = read_i32(buf, p + 4)?;
        if page_len < 0 { return None; }
        let mut rec = p + 8;
        for _ in 0..page_len {
            let image_index = read_i32(buf, rec)?;
            let offset = read_i32(buf, rec + 4)?;
            let length = read_i32(buf, rec + 8)?;
            let fileno = read_i32(buf, rec + 12)?;
            if image_index >= 0 && offset >= 0 && length >= 0 && fileno >= 0 {
                out.push(HierEntry { image_index, offset, length, fileno });
            }
            rec += 16;
        }
        page = next_ptr;
    }
    Some(out)
}

// ── Position buffer ───────────────────────────────────────────────────────────

/// Load camera positions, falling back to a regular overlap-based grid when no
/// position buffer record exists (or it can't be read).
#[allow(clippy::too_many_arguments)]
fn load_positions(
    index_buf:    &[u8],
    nonhier_root: usize,
    record:       i32,
    data_files:   &[PathBuf],
    npositions:   usize,
    tile_w: i32, tile_h: i32, div: i32,
    overlap_x: f64, overlap_y: f64, positions_x: i32,
    verbose: bool,
) -> Vec<(i32, i32)> {
    if record >= 0 {
        if let Some((fileno, offset, size)) =
            read_nonhier_record(index_buf, nonhier_root, record)
        {
            let expected = SLIDE_POSITION_RECORD_SIZE * npositions;
            if size as usize == expected && (fileno as usize) < data_files.len() {
                if let Some(buf) = read_data_bytes(&data_files[fileno as usize], offset as u64, size as usize) {
                    let mut pos = Vec::with_capacity(npositions);
                    for i in 0..npositions {
                        let b = i * SLIDE_POSITION_RECORD_SIZE;
                        // [flags(1)][x i32 LE][y i32 LE]
                        let x = i32::from_le_bytes(buf[b + 1..b + 5].try_into().unwrap());
                        let y = i32::from_le_bytes(buf[b + 5..b + 9].try_into().unwrap());
                        pos.push((x, y));
                    }
                    return pos;
                }
            } else if verbose {
                eprintln!("[mirax] position buffer size {size} != expected {expected}; using regular grid");
            }
        }
    }
    // Fallback: ideal regular grid from tile size and nominal overlap.
    if verbose { eprintln!("[mirax] no usable position buffer; using regular grid"); }
    let adv_x = (tile_w * div) as f64 - overlap_x;
    let adv_y = (tile_h * div) as f64 - overlap_y;
    (0..npositions).map(|i| {
        let i = i as i32;
        let x = ((i % positions_x) as f64 * adv_x).round() as i32;
        let y = ((i / positions_x) as f64 * adv_y).round() as i32;
        (x, y)
    }).collect()
}

/// Resolve a non-hierarchical record to (fileno, offset, size) via Index.dat.
fn read_nonhier_record(buf: &[u8], nonhier_root: usize, recordno: i32) -> Option<(i32, i32, i32)> {
    let table_ptr = read_i32(buf, nonhier_root)?;
    let rec_ptr = read_i32(buf, table_ptr as usize + 4 * recordno as usize)?;
    // record: [0 marker][data-page pointer]
    if read_i32(buf, rec_ptr as usize)? != 0 { return None; }
    let page = read_i32(buf, rec_ptr as usize + 4)? as usize;
    // data page: [pagesize>=1][next_ptr][0][0][offset][size][fileno]
    if read_i32(buf, page)? < 1 { return None; }
    let offset = read_i32(buf, page + 16)?;
    let size = read_i32(buf, page + 20)?;
    let fileno = read_i32(buf, page + 24)?;
    if offset < 0 || size < 0 || fileno < 0 { return None; }
    Some((fileno, offset, size))
}

fn read_data_bytes(path: &Path, offset: u64, len: usize) -> Option<Vec<u8>> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = fs::File::open(path).ok()?;
    f.seek(SeekFrom::Start(offset)).ok()?;
    let mut buf = vec![0u8; len];
    f.read_exact(&mut buf).ok()?;
    Some(buf)
}

/// IMAGE_FILL_COLOR_BGR is a 32-bit BGR integer → RGB bytes. Default white.
fn parse_fill_bgr(val: Option<&str>) -> [u8; 3] {
    let v = match val.and_then(|s| s.trim().parse::<i64>().ok()) {
        Some(v) => v as u32,
        None => return [255, 255, 255],
    };
    let b = (v & 0xFF) as u8;
    let g = ((v >> 8) & 0xFF) as u8;
    let r = ((v >> 16) & 0xFF) as u8;
    [r, g, b]
}

// ── Tile placement for one output pyramid level ───────────────────────────────

// A source tile mapped onto a downsampled output level: top-left and size in the
// level's pixel space (full resolution / `downsample`).
struct ScaledTile {
    src:    usize,  // index into MiraxSource.tiles
    x:      i32,
    y:      i32,
    w:      u32,
    h:      u32,
}

// Decode one source JPEG and downsample it to the level's scale, returning
// interleaved RGB pixels of size st.w × st.h (or None on failure).
fn decode_scaled(bytes: &[u8], st: &ScaledTile) -> Option<Vec<u8>> {
    let img = turbojpeg::decompress(bytes, turbojpeg::PixelFormat::RGB).ok()?;
    let (w, h) = (img.width, img.height);
    if w == 0 || h == 0 { return None; }
    let pitch = w * NCH;
    let pixels: Vec<u8> = if img.pitch == pitch {
        img.pixels
    } else {
        (0..h).flat_map(|r| img.pixels[r * img.pitch..r * img.pitch + pitch].iter().copied()).collect()
    };
    if st.w as usize == w && st.h as usize == h {
        return Some(pixels);
    }
    // Downsample with antialiasing (bilinear convolution scales the filter).
    let src = fir::images::Image::from_vec_u8(w as u32, h as u32, pixels, fir::PixelType::U8x3).ok()?;
    let mut dst = fir::images::Image::new(st.w.max(1), st.h.max(1), fir::PixelType::U8x3);
    let opts = fir::ResizeOptions::new()
        .resize_alg(fir::ResizeAlg::Convolution(fir::FilterType::Bilinear));
    fir::Resizer::new().resize(&src, &mut dst, &opts).ok()?;
    Some(dst.into_vec())
}

// Build one output tile: greedily blit all overlapping scaled source tiles onto
// a background canvas, then JPEG-encode. Returns None if no source tile actually
// covers the tile (so the TIFF tile stays blank).
fn build_tile(
    out_id:   u32,
    tile_x0:  i32,
    tile_y0:  i32,
    out_w:    usize,
    out_h:    usize,
    cell:     &[ScaledTile],
    src:      &MiraxSource,
    mmaps:    &[Option<memmap2::Mmap>],
    quality:  u8,
) -> Option<(u32, Vec<u8>)> {
    let mut canvas = vec![0u8; OUT_TILE as usize * OUT_TILE as usize * NCH];
    for px in canvas.chunks_exact_mut(NCH) { px.copy_from_slice(&src.fill_rgb); }

    let mut any = false;
    for st in cell {
        let t = &src.tiles[st.src];
        let Some(mm) = mmaps.get(t.fileno).and_then(|m| m.as_ref()) else { continue; };
        let s = t.offset as usize;
        let e = (s + t.length as usize).min(mm.len());
        if s >= e { continue; }
        let Some(pixels) = decode_scaled(&mm[s..e], st) else { continue; };

        let ox0 = st.x.max(tile_x0);
        let oy0 = st.y.max(tile_y0);
        let ox1 = (st.x + st.w as i32).min(tile_x0 + out_w as i32);
        let oy1 = (st.y + st.h as i32).min(tile_y0 + out_h as i32);
        if ox1 <= ox0 || oy1 <= oy0 { continue; }
        any = true;
        let copy_w = (ox1 - ox0) as usize;
        for y in oy0..oy1 {
            let src_off = ((y - st.y) as usize * st.w as usize + (ox0 - st.x) as usize) * NCH;
            let dst_off = ((y - tile_y0) as usize * OUT_TILE as usize + (ox0 - tile_x0) as usize) * NCH;
            canvas[dst_off..dst_off + copy_w * NCH]
                .copy_from_slice(&pixels[src_off..src_off + copy_w * NCH]);
        }
    }
    if !any { return None; }

    // Encode the full OUT_TILE×OUT_TILE canvas; edge tiles keep their background
    // padding so the JPEG SOF matches the declared (16-aligned) tile size.
    let jpeg = turbojpeg::compress(turbojpeg::Image::<&[u8]> {
        pixels: &canvas, width: OUT_TILE as usize,
        pitch: OUT_TILE as usize * NCH, height: OUT_TILE as usize,
        format: turbojpeg::PixelFormat::RGB,
    }, quality as i32, turbojpeg::Subsamp::Sub2x2).ok()?.to_vec();
    Some((out_id, jpeg))
}

// ── Output pyramid level ─────────────────────────────────────────────────────

struct OutLevel {
    downsample: u32,
    img_w:      u32,
    img_h:      u32,
    mpp_x:      f64,
    mpp_y:      f64,
    cells:      Vec<Vec<ScaledTile>>, // per output tile (row-major), source tiles overlapping it
    ntx:        u32,
    nty:        u32,
}

// Assign every source tile to the output tiles it overlaps, at the given
// downsample factor. Returns None if the level is below MIN_PYRAMID_SIDE.
fn build_level(src: &MiraxSource, downsample: u32) -> Option<OutLevel> {
    let d = downsample as f64;
    let img_w = (src.img_w as f64 / d).round().max(1.0) as u32;
    let img_h = (src.img_h as f64 / d).round().max(1.0) as u32;
    let ntx = img_w.div_ceil(OUT_TILE);
    let nty = img_h.div_ceil(OUT_TILE);
    let mut cells: Vec<Vec<ScaledTile>> =
        (0..ntx * nty).map(|_| Vec::new()).collect();

    for (i, t) in src.tiles.iter().enumerate() {
        let x = (t.place_x as f64 / d).round() as i32;
        let y = (t.place_y as f64 / d).round() as i32;
        let w = (src.tile_w as f64 / d).round().max(1.0) as u32;
        let h = (src.tile_h as f64 / d).round().max(1.0) as u32;
        let cx0 = (x.max(0) as u32 / OUT_TILE).min(ntx.saturating_sub(1));
        let cy0 = (y.max(0) as u32 / OUT_TILE).min(nty.saturating_sub(1));
        let cx1 = ((x + w as i32 - 1).max(0) as u32 / OUT_TILE).min(ntx.saturating_sub(1));
        let cy1 = ((y + h as i32 - 1).max(0) as u32 / OUT_TILE).min(nty.saturating_sub(1));
        for cy in cy0..=cy1 {
            for cx in cx0..=cx1 {
                cells[(cy * ntx + cx) as usize].push(ScaledTile { src: i, x, y, w, h });
            }
        }
    }

    Some(OutLevel {
        downsample,
        img_w, img_h,
        mpp_x: src.mpp_x * d,
        mpp_y: src.mpp_y * d,
        cells, ntx, nty,
    })
}

// ── Conversion entry point ───────────────────────────────────────────────────

pub(crate) fn convert_mrxs(
    mrxs_path: &str,
    out_path: &str,
    legacy: bool,
    quality: u8,
    half: bool,
    verbose: bool,
    pb: Option<&ProgressBar>,
) -> Result<(), String> {
    let src = MiraxSource::open(mrxs_path, verbose)
        .ok_or_else(|| "not a valid MIRAX slide (missing Slidedat.ini / Index.dat)".to_string())?;

    // Memory-map every Data####.dat so tiles are read straight from the page
    // cache by parallel decoders.
    let mmaps: Vec<Option<memmap2::Mmap>> = src.data_files.iter().map(|p| {
        fs::File::open(p).ok().and_then(|f| unsafe { memmap2::Mmap::map(&f).ok() })
    }).collect();

    // Build the output pyramid: downsample by ×2 each level until the longer
    // side drops below MIN_PYRAMID_SIDE. --half drops the full-resolution level
    // by starting at ×2.
    let start = if half { 2u32 } else { 1u32 };
    let mut levels: Vec<OutLevel> = Vec::new();
    let mut d = start;
    loop {
        let lv = build_level(&src, d).unwrap();
        let below = lv.img_w.max(lv.img_h) < MIN_PYRAMID_SIDE;
        let is_first = levels.is_empty();
        levels.push(lv);
        if below { break; }
        d *= 2;
        if !is_first && (src.img_w / d).max(src.img_h / d) == 0 { break; }
    }

    if verbose {
        vlog(pb, format!(
            "  [mirax] {}x{}  {} levels  spp=3  (decode→greedy blit→re-encode)",
            levels[0].img_w, levels[0].img_h, levels.len(),
        ));
    }

    let total_tiles: u64 = levels.iter()
        .map(|lv| lv.cells.iter().filter(|c| !c.is_empty()).count() as u64)
        .sum();
    if let Some(p) = pb { p.set_length(total_tiles); }

    // ── Open output TIFF ──
    let out_c   = CString::new(out_path).map_err(|e| e.to_string())?;
    let w8_mode = CString::new("w8").unwrap();
    let dst = unsafe { TIFFOpen(out_c.as_ptr(), w8_mode.as_ptr()) };
    if dst.is_null() { return Err(format!("cannot create {out_path}")); }

    let ome = !legacy;
    let n_subifds = levels.len().saturating_sub(1);
    if ome && n_subifds > 0 {
        let zeros: Vec<u64> = vec![0u64; n_subifds];
        unsafe { TIFFSetField(dst, TIFFTAG_SUBIFD, n_subifds as u32, zeros.as_ptr()); }
    }

    let image_desc_c: Option<CString> = if ome {
        let stem = Path::new(mrxs_path).file_stem().and_then(|s| s.to_str()).unwrap_or("image");
        let xml = crate::pipeline::ome::generate_tiff_ome_xml(
            stem, levels[0].img_w, levels[0].img_h, levels[0].mpp_x, levels[0].mpp_y, 3,
        );
        Some(CString::new(xml).map_err(|e| e.to_string())?)
    } else {
        None
    };

    let chunk_size = (rayon::current_num_threads() * 4).max(1);

    for (lv_idx, lv) in levels.iter().enumerate() {
        if verbose {
            vlog(pb, format!("  [mirax] lv{} 1/{}  {}x{}  {:.4} µm/px",
                lv_idx, lv.downsample, lv.img_w, lv.img_h, lv.mpp_x));
        }
        let subfile = if lv_idx == 0 { 0u32 } else { FILETYPE_REDUCEDIMAGE };
        unsafe {
            set_tiff_ifd_tags(dst, subfile,
                lv.img_w, lv.img_h, OUT_TILE, OUT_TILE,
                COMPRESSION_JPEG, PHOTOMETRIC_YCBCR, 3,
                lv.mpp_x, lv.mpp_y);
            TIFFSetField(dst, TIFFTAG_YCBCRSUBSAMPLING as u32, 2u32, 2u32);
            if lv_idx == 0 {
                if let Some(ref desc) = image_desc_c {
                    TIFFSetField(dst, TIFFTAG_IMAGEDESCRIPTION as u32, desc.as_ptr());
                }
            }
        }

        // Encode tiles in chunks (bounded memory) and write each chunk in order.
        let ids: Vec<u32> = (0..lv.ntx * lv.nty)
            .filter(|&id| !lv.cells[id as usize].is_empty())
            .collect();
        let mut jpegtables_registered = false;
        for chunk in ids.chunks(chunk_size) {
            let mut encoded: Vec<(u32, Vec<u8>)> = chunk.par_iter().filter_map(|&id| {
                let cx = id % lv.ntx;
                let cy = id / lv.ntx;
                let tile_x0 = (cx * OUT_TILE) as i32;
                let tile_y0 = (cy * OUT_TILE) as i32;
                let out_w = (lv.img_w - cx * OUT_TILE).min(OUT_TILE) as usize;
                let out_h = (lv.img_h - cy * OUT_TILE).min(OUT_TILE) as usize;
                build_tile(id, tile_x0, tile_y0, out_w, out_h,
                    &lv.cells[id as usize], &src, &mmaps, quality)
            }).collect();
            encoded.sort_unstable_by_key(|(id, _)| *id);
            let n = encoded.len() as u64;
            unsafe { write_enc_chunk(dst, &encoded, &mut jpegtables_registered); }
            if let Some(p) = pb { p.inc(n); }
        }

        unsafe { TIFFWriteDirectory(dst); }
    }

    unsafe { TIFFClose(dst); }
    Ok(())
}
