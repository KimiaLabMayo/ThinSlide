// TIFF/SVS downsampling logic for thinslide.
// Reads pyramidal TIFF and SVS files and writes downsampled OME-TIFF or BigTIFF.

use std::ffi::CString;
use std::os::raw::c_void;
use std::path::Path;
use std::sync::{Arc, mpsc};
use std::sync::atomic::{AtomicUsize, Ordering};
use rayon::prelude::*;
use image::imageops::FilterType;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use fast_image_resize as fir;
use jpeg2k;

use crate::bindings::{
    TIFF, TIFFOpen, TIFFClose,
    TIFFGetField,
    TIFFSetField, TIFFSetDirectory, TIFFIsTiled,
    TIFFReadRawTile, TIFFReadEncodedTile, TIFFReadRawStrip,
    TIFFWriteRawTile, TIFFWriteRawStrip, TIFFWriteDirectory,
    TIFFTileSize, TIFFNumberOfTiles, TIFFNumberOfStrips,
    TIFFTAG_SUBFILETYPE, TIFFTAG_IMAGEWIDTH, TIFFTAG_IMAGELENGTH,
    TIFFTAG_TILEWIDTH, TIFFTAG_TILELENGTH, TIFFTAG_ROWSPERSTRIP,
    TIFFTAG_COMPRESSION, TIFFTAG_BITSPERSAMPLE, TIFFTAG_SAMPLESPERPIXEL,
    TIFFTAG_PHOTOMETRIC, TIFFTAG_PLANARCONFIG, TIFFTAG_SAMPLEFORMAT,
    TIFFTAG_ORIENTATION, TIFFTAG_PREDICTOR, TIFFTAG_RESOLUTIONUNIT,
    TIFFTAG_XRESOLUTION, TIFFTAG_YRESOLUTION, TIFFTAG_EXTRASAMPLES, TIFFTAG_COLORMAP,
    TIFFTAG_TILEBYTECOUNTS, TIFFTAG_STRIPBYTECOUNTS,
    TIFFTAG_IMAGEDESCRIPTION, TIFFTAG_ICCPROFILE,
    TIFFTAG_JPEGTABLES, TIFFTAG_YCBCRSUBSAMPLING, TIFFTAG_SUBIFD,
    COMPRESSION_JPEG,
    PHOTOMETRIC_RGB, PHOTOMETRIC_YCBCR, PHOTOMETRIC_MINISBLACK,
    FILETYPE_REDUCEDIMAGE,
};
use crate::{tile_align, nearest_16, MIN_PYRAMID_SIDE,
            vlog, write_enc_chunk, compute_thread, set_tiff_ifd_tags};
use crate::source::tiff::{
    TiffSource, TiffLevel, MainIfd, navigate,
    is_jp2k, COMPRESSION_APERIO_JP2_YCBCR,
};

// ─── Output pyramid description ───────────────────────────────────────────────

struct OutputLevel {
    out_img_w:    u32,
    out_img_h:    u32,
    out_tile_w:   u32,
    out_tile_h:   u32,
    actual_mpp_x: f64,
    actual_mpp_y: f64,
    src_idx:      usize,
    passthrough:  bool,
}

// ─── Pipeline types ───────────────────────────────────────────────────────────

type RawQuad  = [Option<(Vec<u8>, bool)>; 4];
type RawChunk = Vec<(u32, RawQuad)>;
type EncChunk = Vec<(u32, Vec<u8>)>;

struct EncodeParams {
    quality:           u8,
    src_tile_w:        u32,
    src_tile_h:        u32,
    out_tile_w:        u32,
    out_tile_h:        u32,
    spp:               u32,
    resize_opts:       fir::ResizeOptions,
    fpt:               fir::PixelType,
    src_is_jpeg:       bool,
    src_jp2k_is_ycbcr: bool,
    src_photometric:   u32,
    n_reduce:          u32,
    decode_shift:      u32,
    jpeg_tables:       Option<Arc<Vec<u8>>>,
    icc_transform:     Option<Arc<crate::IccTransform>>,
    // --roi: (per-output-tile "touches an annotation" flags, pre-encoded white tile)
    roi_fill:          Option<(Vec<bool>, Vec<u8>)>,
}

fn encode_one_tile(out_id: u32, quads: &RawQuad, p: &EncodeParams) -> Option<(u32, Vec<u8>)> {
    if let Some((in_roi, white)) = &p.roi_fill {
        if !in_roi[out_id as usize] { return Some((out_id, white.clone())); }
    }
    let decoded: [Option<(Vec<u8>, u32, u32)>; 4] = std::array::from_fn(|qi| {
        let (data, is_raw_decode) = quads[qi].as_ref()?;
        if *is_raw_decode && p.src_is_jpeg {
            let inject_app14 = p.spp == 3 && p.src_photometric == PHOTOMETRIC_RGB;
            decode_jpeg_tile(data, p.spp, p.jpeg_tables.as_deref().map(|v| v.as_slice()),
                inject_app14, p.decode_shift)
        } else if *is_raw_decode {
            decode_jp2k_tile(data, p.spp, p.src_jp2k_is_ycbcr, p.n_reduce)
        } else {
            Some((data.clone(), p.src_tile_w, p.src_tile_h))
        }
    });

    crate::compose_and_encode(out_id, decoded, p.spp as usize, p.out_tile_w, p.out_tile_h,
        p.icc_transform.as_deref(), p.fpt, &p.resize_opts, p.quality, p.spp)
}

const APP14_ADOBE_RGB: [u8; 16] = [
    0xFF, 0xEE, 0x00, 0x0E,
    b'A', b'd', b'o', b'b', b'e',
    0x00, 0x64, 0x00, 0x00, 0x00, 0x00, 0x00,
];

// Decodes a TIFF JPEG tile (abbreviated when `tables` is given) to packed RGB/gray,
// optionally at 1/2^`shift` scale via DCT scaling.
fn decode_jpeg_tile(data: &[u8], spp: u32, tables: Option<&[u8]>, inject_app14: bool, shift: u32)
    -> Option<(Vec<u8>, u32, u32)>
{
    let ch = spp as usize;
    let fmt = if spp == 1 { turbojpeg::PixelFormat::GRAY } else { turbojpeg::PixelFormat::RGB };
    let combined: Vec<u8> = if let Some(tables) = tables {
        let app14_len = if inject_app14 { APP14_ADOBE_RGB.len() } else { 0 };
        let mut v = Vec::with_capacity(2 + app14_len + (tables.len() - 4) + (data.len() - 2));
        v.extend_from_slice(&tables[0..2]);
        if inject_app14 { v.extend_from_slice(&APP14_ADOBE_RGB); }
        v.extend_from_slice(&tables[2..tables.len()-2]);
        v.extend_from_slice(&data[2..]);
        v
    } else { data.to_vec() };

    let scaling = match shift {
        1 => Some(turbojpeg::ScalingFactor::ONE_HALF),
        2 => Some(turbojpeg::ScalingFactor::ONE_QUARTER),
        _ => None,
    };
    if let Some(sf) = scaling {
        let mut dec = turbojpeg::Decompressor::new().ok()?;
        dec.set_scaling_factor(sf).ok()?;
        let header = dec.read_header(&combined).ok()?;
        let scaled = header.scaled(sf);
        let (w, h) = (scaled.width, scaled.height);
        let pitch = w * ch;
        let mut pixels = vec![0u8; h * pitch];
        dec.decompress(&combined, turbojpeg::Image {
            pixels: pixels.as_mut_slice(), width: w, pitch, height: h, format: fmt,
        }).ok()?;
        Some((pixels, w as u32, h as u32))
    } else {
        let dec = turbojpeg::decompress(&combined, fmt).ok()?;
        let (w, h) = (dec.width as u32, dec.height as u32);
        let pitch = w as usize * ch;
        let pix = if dec.pitch == pitch {
            dec.pixels
        } else {
            (0..h as usize).flat_map(|r| {
                let s = r * dec.pitch;
                dec.pixels[s..s+pitch].iter().copied()
            }).collect()
        };
        Some((pix, w, h))
    }
}

// Decodes a JP2K codestream tile to packed RGB/gray at resolution level `reduce`.
fn decode_jp2k_tile(data: &[u8], spp: u32, src_jp2k_is_ycbcr: bool, reduce: u32) -> Option<(Vec<u8>, u32, u32)> {
    let params = jpeg2k::DecodeParameters::default().reduce(reduce);
    let img = jpeg2k::Image::from_bytes_with(data, params).ok()?;
    let (mut pix, luma_w, luma_h) = super::jp2k_assemble_pixels(&img, spp as usize)?;
    let color_space = img.color_space();
    let needs_ycbcr_cvt = spp == 3 && (
        matches!(color_space, jpeg2k::ColorSpace::SYCC)
        || (src_jp2k_is_ycbcr && !matches!(color_space, jpeg2k::ColorSpace::SRGB))
    );
    if needs_ycbcr_cvt {
        super::ycbcr_to_rgb(&mut pix);
    }
    Some((pix, luma_w as u32, luma_h as u32))
}

// ─── --roi reduced pyramid ────────────────────────────────────────────────────

/// --roi: builds a 4x-reduced level from the tiles of the level above, which are
/// pushed in tile-id order. Each output tile is a 4x4 block of input tiles decoded
/// at 1/4 scale. Input tiles may be TIFF JPEG (abbreviated with `jpeg_tables`),
/// full JPEG, or a JP2K codestream; missing tiles become white.
pub(crate) struct Reducer {
    in_cols:     u32,
    out_grid:    (u32, u32),
    tile:        (u32, u32),
    spp:         u32,
    quality:     u8,
    jpeg_tables: Option<Vec<u8>>,
    rgb_app14:   bool,
    jp2k_ycbcr:  bool,
    band:        Vec<Option<Vec<u8>>>,  // input tiles of the current 4-row band
    band_row:    u32,                   // first input row of `band`
    out:         Vec<(u32, Vec<u8>)>,   // encoded output tiles
}

impl Reducer {
    pub(crate) fn new(in_grid: (u32, u32), tile: (u32, u32), spp: u32, quality: u8,
           jpeg_tables: Option<Vec<u8>>, rgb_app14: bool, jp2k_ycbcr: bool) -> Reducer {
        Reducer {
            in_cols: in_grid.0,
            out_grid: (in_grid.0.div_ceil(4), in_grid.1.div_ceil(4)),
            tile, spp, quality, jpeg_tables, rgb_app14, jp2k_ycbcr,
            band: vec![None; in_grid.0 as usize * 4],
            band_row: 0,
            out: Vec::new(),
        }
    }

    pub(crate) fn push(&mut self, id: u32, data: &[u8]) {
        while id / self.in_cols >= self.band_row + 4 { self.flush_band(); }
        self.band[(id - self.band_row * self.in_cols) as usize] = Some(data.to_vec());
    }

    fn flush_band(&mut self) {
        let or_ = self.band_row / 4;
        let row: Vec<(u32, Vec<u8>)> = (0..self.out_grid.0).into_par_iter()
            .filter_map(|oc| Some((or_ * self.out_grid.0 + oc, self.encode_tile(oc)?)))
            .collect();
        self.out.extend(row);
        self.band.iter_mut().for_each(|t| *t = None);
        self.band_row += 4;
    }

    fn encode_tile(&self, oc: u32) -> Option<Vec<u8>> {
        let ch = self.spp as usize;
        let (tw, th) = (self.tile.0 as usize, self.tile.1 as usize);
        let (sw, sh) = (tw / 4, th / 4);
        let mut canvas = vec![255u8; tw * th * ch];
        for (i, slot) in self.band.iter().enumerate() {
            let (c, r) = (i as u32 % self.in_cols, i as u32 / self.in_cols);
            if c / 4 != oc { continue; }
            let Some(data) = slot else { continue; };
            let decoded = if data.starts_with(&[0xFF, 0xD8]) {
                decode_jpeg_tile(data, self.spp, self.jpeg_tables.as_deref(), self.rgb_app14, 2)
            } else {
                decode_jp2k_tile(data, self.spp, self.jp2k_ycbcr, 2)
            };
            let Some((pix, pw, ph)) = decoded else { continue; };
            let (ox, oy) = ((c % 4) as usize * sw, r as usize * sh);
            let w = (pw as usize).min(tw - ox);
            for y in 0..(ph as usize).min(th - oy) {
                let d = ((oy + y) * tw + ox) * ch;
                let s = y * pw as usize * ch;
                canvas[d..d + w * ch].copy_from_slice(&pix[s..s + w * ch]);
            }
        }
        let (format, subsamp) = if ch == 1 {
            (turbojpeg::PixelFormat::GRAY, turbojpeg::Subsamp::Gray)
        } else {
            (turbojpeg::PixelFormat::RGB, turbojpeg::Subsamp::Sub2x2)
        };
        turbojpeg::compress(turbojpeg::Image::<&[u8]> {
            pixels: &canvas, width: tw, pitch: tw * ch, height: th, format,
        }, self.quality as i32, subsamp).ok().map(|j| j.to_vec())
    }

    fn finish(mut self) -> Vec<(u32, Vec<u8>)> {
        while self.band_row < self.out_grid.1 * 4 { self.flush_band(); }
        self.out
    }
}

/// --roi: number of 1/4-step levels below a cropped base of size `dim`.
pub(crate) fn roi_reduced_levels(dim: (u32, u32)) -> u32 {
    let mut n = 0;
    while dim.0.max(dim.1).div_ceil(4u32.pow(n + 1)) >= MIN_PYRAMID_SIDE { n += 1; }
    n
}

/// --roi: tiles per reduced level for a base of `base_grid` tiles.
pub(crate) fn roi_reduced_tiles(base_grid: (u32, u32), n_levels: u32) -> u64 {
    (1..=n_levels).map(|k| {
        let d = 4u32.pow(k);
        base_grid.0.div_ceil(d) as u64 * base_grid.1.div_ceil(d) as u64
    }).sum()
}

/// --roi: crop of a level `grid` whose tiles each cover `footprint` pixels of a source
/// level sized `level`; None (with a warning) if no annotation overlaps the slide.
pub(crate) fn roi_crop_of(roi: &crate::roi::Roi, grid: (u32, u32), footprint: (u32, u32), level: (u32, u32),
               base: (u32, u32), src_path: &str) -> Option<crate::roi::RoiCrop> {
    let crop = crate::roi::RoiCrop::from_mask(&roi.tile_mask(grid, footprint, level, base), grid);
    if crop.is_none() { eprintln!("  [warn ] --roi: no annotation overlaps {src_path}; skipping"); }
    crop
}

/// --roi: writes the 1/4-step levels below the base, one reduced-image IFD each,
/// starting from the reducer fed with the base tiles.
pub(crate) unsafe fn write_reduced_levels(
    dst_tiff: *mut crate::bindings::TIFF,
    reducer: Reducer,
    base_dim: (u32, u32),
    base_mpp: (f64, f64),
    n_levels: u32,
    verbose: bool,
    pb: Option<&ProgressBar>,
) {
    let (tw, th) = reducer.tile;
    let (spp, quality) = (reducer.spp, reducer.quality);
    let photometric = if spp == 1 { PHOTOMETRIC_MINISBLACK } else { PHOTOMETRIC_YCBCR };
    let mut next = Some(reducer);
    for k in 1..=n_levels {
        let d = 4u32.pow(k);
        let (w, h) = (base_dim.0.div_ceil(d), base_dim.1.div_ceil(d));
        let Some(reducer) = next.take() else { break; };
        let tiles = reducer.finish();
        if verbose {
            vlog(pb, format!("  [roi  ] lv{}  {}x{}  1/{} of base  tile {}x{}", k, w, h, d, tw, th));
        }
        unsafe {
            set_tiff_ifd_tags(dst_tiff, FILETYPE_REDUCEDIMAGE, w, h, tw, th,
                COMPRESSION_JPEG, photometric, spp, base_mpp.0 * d as f64, base_mpp.1 * d as f64);
            if spp == 3 { TIFFSetField(dst_tiff, TIFFTAG_YCBCRSUBSAMPLING, 2u32, 2u32); }
        }
        if k < n_levels {
            let mut r = Reducer::new((w.div_ceil(tw), h.div_ceil(th)), (tw, th), spp, quality, None, false, false);
            for (id, jpeg) in &tiles { r.push(*id, jpeg); }
            next = Some(r);
        }
        let mut jpegtables_registered = false;
        unsafe {
            write_enc_chunk(dst_tiff, &tiles, &mut jpegtables_registered);
            TIFFWriteDirectory(dst_tiff);
        }
        if let Some(p) = pb { p.inc(tiles.len() as u64); }
    }
}

// ─── Entry point for unified thinslide binary ─────────────────────────────────

pub(crate) fn process_files(
    paths: &[std::path::PathBuf],
    args: &crate::Args,
    mp: &MultiProgress,
    logger: &crate::logger::ConversionLogger,
    stats: &crate::logger::ConversionStats,
) {
    if paths.is_empty() { return; }

    let bar_style = ProgressStyle::with_template(
        "  {msg}  [{elapsed_precise}] {bar:40.cyan/blue} {pos}/{len} tiles"
    ).unwrap().progress_chars("=>-");

    let total_files = paths.len();
    let skipped = AtomicUsize::new(0);

    for (i, path) in paths.iter().enumerate() {
        let idx = i + 1;
        let src_path = path.to_string_lossy().to_string();
        let src_name = path.file_name().unwrap_or_default()
            .to_string_lossy().to_string();
        let pb_msg = format!("({}/{}) {}", idx, total_files, src_name);
        let raw_stem = path.file_stem().unwrap_or_default().to_string_lossy();
        // Strip ".ome" suffix so foo.ome.tiff → stem "foo", output "foo.ome.tiff"
        let src_stem = if raw_stem.ends_with(".ome") {
            raw_stem[..raw_stem.len() - 4].to_string()
        } else {
            raw_stem.to_string()
        };

        let candidates = [
            format!("{}.tiff",     src_stem),
            format!("{}.ome.tiff", src_stem),
            format!("{}.svs",      src_stem),
        ];
        let output_exists = || candidates.iter()
            .any(|name| Path::new(&args.output_dir).join(name).exists());

        if output_exists() {
            if args.verbose { vlog(None, format!("  [skip ] exists: {src_name}")); }
            skipped.fetch_add(1, Ordering::Relaxed);
            stats.skipped.fetch_add(1, Ordering::Relaxed);
            logger.log_skip(idx, &src_name);
            continue;
        }

        let pb = mp.add(ProgressBar::new(0));
        pb.set_style(bar_style.clone());
        pb.set_message(pb_msg.clone());

        let roi = match args.roi.as_deref().map(|r| crate::roi::Roi::resolve(r, &src_stem, &args.roi_id)).transpose() {
            Ok(r) => r.flatten(),
            Err(e) => {
                stats.fail.fetch_add(1, Ordering::Relaxed);
                logger.log_fail(idx, &src_name, &format!("--roi: {}", e));
                pb.finish_and_clear();
                continue;
            }
        };
        if args.roi.is_some() && roi.is_none() && args.verbose {
            vlog(Some(&pb), format!("  [roi  ] no {src_stem}.geojson; converting the whole slide"));
        }

        let file_start = std::time::Instant::now();
        let panic_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            process_file(&src_path, &args.output_dir, &src_stem, args, &pb, roi.as_ref())
        }));

        let (ops, detail) = match panic_result {
            Ok(result) => result,
            Err(payload) => {
                let msg = crate::logger::ConversionLogger::panic_message(&*payload);
                stats.fail.fetch_add(1, Ordering::Relaxed);
                logger.log_fail(idx, &src_name, &format!("panic: {}", msg));
                pb.finish_and_clear();
                continue;
            }
        };
        let elapsed_s = file_start.elapsed().as_millis() as f64 / 1000.0;

        // process_file infers success from output presence; the returned tags
        // describe which operations (repack/downsample/ICC) were applied.
        let produced: Option<std::path::PathBuf> = candidates.iter()
            .map(|name| Path::new(&args.output_dir).join(name))
            .find(|p| p.exists());
        if let Some(out) = produced {
            let in_b  = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
            let out_b = std::fs::metadata(&out).map(|m| m.len()).unwrap_or(0);
            stats.ok.fetch_add(1, Ordering::Relaxed);
            stats.in_bytes.fetch_add(in_b, Ordering::Relaxed);
            stats.out_bytes.fetch_add(out_b, Ordering::Relaxed);
            logger.log_ok(idx, &src_name, elapsed_s, in_b, out_b, detail);
            pb.set_style(ProgressStyle::with_template("  {msg}").unwrap());
            pb.finish_with_message(format!(
                "{}{}  {} \u{2192} {}",
                pb_msg, crate::format_ops(&ops), crate::format_mb(in_b), crate::format_mb(out_b)
            ));
        } else {
            stats.fail.fetch_add(1, Ordering::Relaxed);
            logger.log_fail(idx, &src_name, "no output produced");
            pb.finish_and_clear();
        }
    }

    let sk = skipped.load(Ordering::Relaxed);
    if sk > 0 {
        println!("  {sk} of {total_files} TIFF/SVS files skipped (output already exists).");
    }
}

// ─── ICC bake: single tile decode → transform → encode ───────────────────────

fn bake_single_tile(
    data:              &[u8],
    is_raw_jpeg:       bool,
    is_jp2k_tile:      bool,
    src_jp2k_is_ycbcr: bool,
    spp:               u32,
    src_tile_w:        u32,
    src_tile_h:        u32,
    quality:           u8,
    xform:             &crate::IccTransform,
    jpeg_tables:       Option<&[u8]>,
    src_photometric:   u32,
) -> Option<Vec<u8>> {
    let ch = spp as usize;
    const APP14_ADOBE_RGB: [u8; 16] = [
        0xFF, 0xEE, 0x00, 0x0E,
        b'A', b'd', b'o', b'b', b'e',
        0x00, 0x64, 0x00, 0x00, 0x00, 0x00, 0x00,
    ];

    let (pixels, w, h) = if is_raw_jpeg {
        let fmt = if spp == 1 { turbojpeg::PixelFormat::GRAY } else { turbojpeg::PixelFormat::RGB };
        let combined: Vec<u8> = if let Some(tables) = jpeg_tables {
            let inject_app14 = spp == 3 && src_photometric == PHOTOMETRIC_RGB;
            let app14_len = if inject_app14 { APP14_ADOBE_RGB.len() } else { 0 };
            let mut v = Vec::with_capacity(2 + app14_len + (tables.len() - 4) + (data.len() - 2));
            v.extend_from_slice(&tables[0..2]);
            if inject_app14 { v.extend_from_slice(&APP14_ADOBE_RGB); }
            v.extend_from_slice(&tables[2..tables.len()-2]);
            v.extend_from_slice(&data[2..]);
            v
        } else {
            data.to_vec()
        };
        let dec = turbojpeg::decompress(&combined, fmt).ok()?;
        let (w, h) = (dec.width as u32, dec.height as u32);
        let pitch = w as usize * ch;
        let pix = if dec.pitch == pitch {
            dec.pixels
        } else {
            (0..h as usize).flat_map(|r| {
                dec.pixels[r*dec.pitch..r*dec.pitch+pitch].iter().copied()
            }).collect()
        };
        (pix, w, h)
    } else if is_jp2k_tile {
        let j2k = jpeg2k::Image::from_bytes_with(data, jpeg2k::DecodeParameters::default()).ok()?;
        let (mut pix, luma_w, luma_h) = super::jp2k_assemble_pixels(&j2k, spp as usize)?;
        let cs = j2k.color_space();
        if spp == 3 && (
            matches!(cs, jpeg2k::ColorSpace::SYCC)
            || (src_jp2k_is_ycbcr && !matches!(cs, jpeg2k::ColorSpace::SRGB))
        ) {
            super::ycbcr_to_rgb(&mut pix);
        }
        (pix, luma_w as u32, luma_h as u32)
    } else {
        (data.to_vec(), src_tile_w, src_tile_h)
    };

    let baked = if spp == 3 {
        let mut dst = vec![0u8; pixels.len()];
        crate::apply_icc(xform, &pixels, &mut dst);
        dst
    } else {
        pixels
    };

    if spp == 1 {
        turbojpeg::compress(
            turbojpeg::Image::<&[u8]> {
                pixels: &baked, width: w as usize,
                pitch: w as usize, height: h as usize,
                format: turbojpeg::PixelFormat::GRAY,
            },
            quality as i32, turbojpeg::Subsamp::Gray,
        ).ok().map(|b| b.to_vec())
    } else {
        turbojpeg::compress(
            turbojpeg::Image::<&[u8]> {
                pixels: &baked, width: w as usize,
                pitch: w as usize * 3, height: h as usize,
                format: turbojpeg::PixelFormat::RGB,
            },
            quality as i32, turbojpeg::Subsamp::Sub2x2,
        ).ok().map(|b| b.to_vec())
    }
}

// ─── ICC bake-only: preserve pyramid structure, apply ICC per tile ────────────

fn process_file_icc_bake_only(
    src_path:   &str,
    out_dir:    &str,
    out_stem:   &str,
    args:       &crate::Args,
    icc_xform:  Arc<crate::IccTransform>,
    src_levels: &[TiffLevel],
    layout:     &[MainIfd],
    ome_xml:    Option<&str>,
    pb:         &ProgressBar,
) {
    let out_path = if args.openslide {
        format!("{out_dir}/{out_stem}.tiff")
    } else {
        format!("{out_dir}/{out_stem}.ome.tiff")
    };
    let tmp_path = format!("{out_path}.tmp");

    let total_tiles: u64 = src_levels.iter().map(|lv| lv.n_tiles as u64).sum();
    pb.set_length(total_tiles * n_planes(layout));

    let ome   = !args.openslide;
    let base  = &src_levels[0];
    let out_spp: u32 = if base.spp >= 3 { 3 } else { 1 };
    let out_photometric = if out_spp == 1 { PHOTOMETRIC_MINISBLACK } else { PHOTOMETRIC_YCBCR };

    let image_desc_c: Option<CString> = if ome {
        let xml = if let Some(orig) = ome_xml {
            let pyramid: Vec<(u32, u32)> = src_levels.iter().map(|lv| (lv.img_w, lv.img_h)).collect();
            crate::pipeline::ome::update_ome_xml_for_output(orig, &pyramid, base.mpp_x, base.mpp_y)
        } else {
            crate::pipeline::ome::generate_tiff_ome_xml(
                out_stem, base.img_w, base.img_h, base.mpp_x, base.mpp_y, out_spp,
            )
        };
        Some(CString::new(xml).unwrap())
    } else {
        None
    };

    let chunk_size = (rayon::current_num_threads() * 4).max(1);

    let src_c   = CString::new(src_path).unwrap();
    let tmp_c   = CString::new(tmp_path.as_str()).unwrap();
    let r_mode  = CString::new("r").unwrap();
    let w8_mode = CString::new("w8").unwrap();

    let src_tiff = unsafe { TIFFOpen(src_c.as_ptr(), r_mode.as_ptr()) };
    if src_tiff.is_null() {
        eprintln!("  [error] Cannot open: {src_path}");
        return;
    }
    let dst_tiff = unsafe { TIFFOpen(tmp_c.as_ptr(), w8_mode.as_ptr()) };
    if dst_tiff.is_null() {
        eprintln!("  [error] Cannot create: {tmp_path}");
        unsafe { TIFFClose(src_tiff); }
        return;
    }

    let n_subifds = src_levels.len().saturating_sub(1);
    for (main_idx, main) in layout.iter().enumerate() {
        let plane_levels = match main {
            MainIfd::Plane(levels) => levels,
            MainIfd::Aux(dir) => { unsafe { copy_aux_ifd(src_tiff, dst_tiff, *dir, src_path); } continue; }
        };
        if ome && n_subifds > 0 {
            let zeros: Vec<u64> = vec![0u64; n_subifds];
            unsafe { TIFFSetField(dst_tiff, TIFFTAG_SUBIFD, n_subifds as u32, zeros.as_ptr()); }
        }

        for (lv_idx, src_lv) in src_levels.iter().enumerate() {
            unsafe { navigate(src_tiff, lv_idx, plane_levels); }

            let is_base  = lv_idx == 0;
            let subfile  = if is_base { 0u32 } else { FILETYPE_REDUCEDIMAGE };
            let out_tile_w = tile_align(src_lv.tile_w, 16);
            let out_tile_h = tile_align(src_lv.tile_h, 16);

            unsafe {
                set_tiff_ifd_tags(dst_tiff, subfile,
                    src_lv.img_w, src_lv.img_h, out_tile_w, out_tile_h,
                    COMPRESSION_JPEG as u32, out_photometric as u32, out_spp,
                    src_lv.mpp_x, src_lv.mpp_y);
                if out_photometric == PHOTOMETRIC_YCBCR {
                    TIFFSetField(dst_tiff, TIFFTAG_YCBCRSUBSAMPLING, 2u32, 2u32);
                }
                if is_base && main_idx == 0 {
                    if let Some(ref desc) = image_desc_c {
                        TIFFSetField(dst_tiff, TIFFTAG_IMAGEDESCRIPTION, desc.as_ptr());
                    }
                    // ICC is baked in; do not embed the profile in the output
                }
            }

            let src_is_jp2k   = is_jp2k(src_lv.compression as u32);
            let src_is_jpeg   = src_lv.compression as u32 == COMPRESSION_JPEG;
            let src_jp2k_is_ycbcr =
                src_is_jp2k && src_lv.compression as u32 == COMPRESSION_APERIO_JP2_YCBCR;

            let jpeg_tables_arc: Option<Arc<Vec<u8>>> = if src_is_jpeg {
                crate::pipeline::jpegtables_ext::get_jpeg_tables(src_tiff, src_path).map(Arc::new)
            } else { None };

            let raw_buf_size = (unsafe { TIFFTileSize(src_tiff) } as usize)
                .max(src_lv.tile_w as usize * src_lv.tile_h as usize * src_lv.spp as usize)
                .max(1 << 17);
            let pix_size = src_lv.tile_w as usize * src_lv.tile_h as usize * src_lv.spp as usize;
            let tile_ids: Vec<u32> = (0..src_lv.n_tiles).collect();

            type BakeTile = (u32, Option<(Vec<u8>, bool, bool)>);

            let (raw_tx, raw_rx) = mpsc::sync_channel::<Vec<BakeTile>>(2);
            let (enc_tx, enc_rx) = mpsc::sync_channel::<EncChunk>(2);

            let xform_t        = Arc::clone(&icc_xform);
            let tables_t       = jpeg_tables_arc.clone();
            let quality        = args.quality;
            let spp            = out_spp;
            let src_tile_w     = src_lv.tile_w;
            let src_tile_h     = src_lv.tile_h;
            let src_photometric = src_lv.photometric as u32;

            let compute_handle = std::thread::spawn(move || {
                for raw_chunk in raw_rx {
                    let mut encoded: EncChunk = raw_chunk.par_iter()
                        .filter_map(|(id, tile_opt)| {
                            let (data, is_raw_jpeg, is_jp2k_tile) = tile_opt.as_ref()?;
                            let jpeg = bake_single_tile(
                                data, *is_raw_jpeg, *is_jp2k_tile, src_jp2k_is_ycbcr,
                                spp, src_tile_w, src_tile_h, quality,
                                &xform_t,
                                tables_t.as_deref().map(|v| v.as_slice()),
                                src_photometric,
                            )?;
                            Some((*id, jpeg))
                        })
                        .collect();
                    encoded.sort_unstable_by_key(|(n, _)| *n);
                    if enc_tx.send(encoded).is_err() { break; }
                }
            });

            let mut jpegtables_registered = false;
            let mut pending_write: Option<EncChunk> = None;

            for chunk in tile_ids.chunks(chunk_size) {
                let raw_chunk: Vec<BakeTile> = chunk.iter()
                    .map(|&tile_num| {
                        if src_is_jp2k || src_is_jpeg {
                            let mut buf = vec![0u8; raw_buf_size];
                            let n = unsafe { TIFFReadRawTile(src_tiff, tile_num,
                                buf.as_mut_ptr() as *mut c_void, buf.len() as i64) };
                            if n > 0 {
                                buf.truncate(n as usize);
                                (tile_num, Some((buf, src_is_jpeg, src_is_jp2k)))
                            } else {
                                (tile_num, None)
                            }
                        } else {
                            let mut buf = vec![0u8; pix_size];
                            let n = unsafe { TIFFReadEncodedTile(src_tiff, tile_num,
                                buf.as_mut_ptr() as *mut c_void, buf.len() as i64) };
                            if n > 0 { (tile_num, Some((buf, false, false))) }
                            else { (tile_num, None) }
                        }
                    })
                    .collect();

                raw_tx.send(raw_chunk).expect("compute thread dropped");

                if let Some(prev) = pending_write.take() {
                    let n = prev.len() as u64;
                    unsafe { write_enc_chunk(dst_tiff, &prev, &mut jpegtables_registered); }
                    pb.inc(n);
                }
                pending_write = enc_rx.recv().ok();
            }

            drop(raw_tx);
            if let Some(last) = pending_write.take() {
                let n = last.len() as u64;
                unsafe { write_enc_chunk(dst_tiff, &last, &mut jpegtables_registered); }
                pb.inc(n);
            }
            for enc in enc_rx {
                let n = enc.len() as u64;
                unsafe { write_enc_chunk(dst_tiff, &enc, &mut jpegtables_registered); }
                pb.inc(n);
            }
            compute_handle.join().expect("compute thread panicked");

            unsafe { TIFFWriteDirectory(dst_tiff); }
        }
    }

    unsafe { TIFFClose(dst_tiff); }
    unsafe { TIFFClose(src_tiff); }

    if let Err(e) = std::fs::rename(&tmp_path, &out_path) {
        eprintln!("  [error] Failed to rename {tmp_path} → {out_path}: {e}");
        let _ = std::fs::remove_file(&tmp_path);
    }
}

// ─── JP2K SVS passthrough ─────────────────────────────────────────────────────

/// With `ome_xml` the output is an OME-TIFF (reduced levels as SubIFDs) instead of an SVS.
/// `levels` are the first plane's levels from `skip` on; every plane of `layout` is
/// written the same way.
fn write_jp2k_svs_from_tiff(
    src_path: &str,
    levels: &[TiffLevel],
    layout: &[MainIfd],
    skip: usize,
    dst_path: &str,
    verbose: bool,
    pb: &ProgressBar,
    roi_crop: Option<&crate::roi::RoiCrop>,
    quality: u8,
    ome_xml: Option<String>,
) {
    if levels.is_empty() { return; }
    // --roi: only the base is copied (cropped); the levels below are rebuilt from it.
    let levels = if roi_crop.is_some() { &levels[..1] } else { levels };
    let base = &levels[0];
    let level_dim = |lv: &TiffLevel| roi_crop.map_or((lv.img_w, lv.img_h),
        |c| c.dim((lv.img_w, lv.img_h), (lv.tile_w, lv.tile_h)));
    let (base_w, base_h) = level_dim(base);
    let roi_levels = if roi_crop.is_some() { roi_reduced_levels((base_w, base_h)) } else { 0 };

    let ome = ome_xml.is_some();
    let img_desc = ome_xml.unwrap_or_else(|| format!(
        "Aperio Image Library\n{}x{} ({} x {})\nMPP = {:.6}",
        base_w, base_h, base.tile_w, base.tile_h, base.mpp_x
    ));

    let total_tiles: u64 = match roi_crop {
        Some(c) => (c.cols * c.rows) as u64 + roi_reduced_tiles((c.cols, c.rows), roi_levels),
        None => levels.iter().map(|lv| lv.n_tiles as u64).sum(),
    };
    pb.set_length(total_tiles * n_planes(layout));

    let src_c   = CString::new(src_path).unwrap();
    let dst_c   = CString::new(dst_path).unwrap();
    let r_mode  = CString::new("r").unwrap();
    let w8_mode = CString::new("w8").unwrap();

    let src_tiff = unsafe { TIFFOpen(src_c.as_ptr(), r_mode.as_ptr()) };
    if src_tiff.is_null() {
        eprintln!("  [error] Cannot open source for JP2K passthrough: {src_path}");
        return;
    }
    let dst_tiff = unsafe { TIFFOpen(dst_c.as_ptr(), w8_mode.as_ptr()) };
    if dst_tiff.is_null() {
        eprintln!("  [error] Cannot create SVS: {dst_path}");
        unsafe { TIFFClose(src_tiff); }
        return;
    }
    let n_subifds = levels.len() - 1 + roi_levels as usize;
    for (main_idx, main) in layout.iter().enumerate() {
        let plane_levels = match main {
            MainIfd::Plane(levels) => levels,
            MainIfd::Aux(dir) => { unsafe { copy_aux_ifd(src_tiff, dst_tiff, *dir, src_path); } continue; }
        };
        if ome && n_subifds > 0 {
            let zeros: Vec<u64> = vec![0u64; n_subifds];
            unsafe { TIFFSetField(dst_tiff, TIFFTAG_SUBIFD, n_subifds as u32, zeros.as_ptr()); }
        }
        let mut reducer: Option<Reducer> = None;

        for (idx, lv) in levels.iter().enumerate() {
            unsafe { navigate(src_tiff, skip + idx, plane_levels); }

            let aperio_compr: u32 =
                if lv.photometric as u32 == PHOTOMETRIC_YCBCR { COMPRESSION_APERIO_JP2_YCBCR }
                else { crate::source::tiff::COMPRESSION_APERIO_JP2_RGB };

            let subfile: u32 = if idx == 0 { 0 } else { FILETYPE_REDUCEDIMAGE };
            let (lv_w, lv_h) = level_dim(lv);
            unsafe {
                set_tiff_ifd_tags(dst_tiff, subfile,
                    lv_w, lv_h, lv.tile_w, lv.tile_h,
                    aperio_compr, lv.photometric as u32, lv.spp as u32,
                    lv.mpp_x, lv.mpp_y);
                if lv.photometric as u32 == PHOTOMETRIC_YCBCR {
                    TIFFSetField(dst_tiff, TIFFTAG_YCBCRSUBSAMPLING, 2u32, 2u32);
                }
            }

            if idx == 0 && main_idx == 0 {
                let desc_c = CString::new(img_desc.as_str()).unwrap();
                unsafe { TIFFSetField(dst_tiff, TIFFTAG_IMAGEDESCRIPTION, desc_c.as_ptr()); }
            }

            if verbose {
                vlog(Some(pb), format!("  [pass ] lv{}  {}x{}  {:.4} µm/px  tile {}x{}  ({} tiles)",
                    idx, lv_w, lv_h, lv.mpp_x, lv.tile_w, lv.tile_h, lv.n_tiles));
            }

            // --roi: tiles outside the annotations get a white JP2K tile instead of a raw copy.
            let roi_fill = roi_crop.map(|c| {
                let white = crate::pipeline::encode::white_jp2k_tile(lv.tile_w, lv.tile_h, lv.spp as u32,
                    aperio_compr == COMPRESSION_APERIO_JP2_YCBCR).expect("white JP2K tile encode failed");
                reducer = Some(Reducer::new((c.cols, c.rows), (lv.tile_w, lv.tile_h),
                    if lv.spp >= 3 { 3 } else { 1 }, quality, None, false,
                    lv.compression as u32 == COMPRESSION_APERIO_JP2_YCBCR));
                (c, white)
            });

            let raw_buf_size = (unsafe { TIFFTileSize(src_tiff) } as usize).max(1 << 17);
            let n_tiles = roi_crop.map_or(lv.n_tiles, |c| c.cols * c.rows);
            for tile_num in 0..n_tiles {
                let mut src_tile = tile_num;
                if let Some((crop, white)) = &roi_fill {
                    if !crop.mask[tile_num as usize] {
                        unsafe { TIFFWriteRawTile(dst_tiff, tile_num,
                            white.as_ptr() as *mut c_void, white.len() as i64); }
                        if let Some(r) = reducer.as_mut() { r.push(tile_num, white); }
                        pb.inc(1);
                        continue;
                    }
                    src_tile = crop.full_id(tile_num);
                }
                let mut buf = vec![0u8; raw_buf_size];
                let n = unsafe { TIFFReadRawTile(src_tiff, src_tile,
                    buf.as_mut_ptr() as *mut c_void, buf.len() as i64) };
                if n > 0 {
                    unsafe { TIFFWriteRawTile(dst_tiff, tile_num,
                        buf.as_ptr() as *mut c_void, n); }
                    if let Some(r) = reducer.as_mut() { r.push(tile_num, &buf[..n as usize]); }
                }
                pb.inc(1);
            }

            unsafe { TIFFWriteDirectory(dst_tiff); }
        }

        if let Some(r) = reducer {
            unsafe { write_reduced_levels(dst_tiff, r, (base_w, base_h),
                (base.mpp_x, base.mpp_y), roi_levels, verbose, Some(pb)); }
        }
    }

    unsafe { TIFFClose(dst_tiff); }
    unsafe { TIFFClose(src_tiff); }
}

// ─── OME-TIFF main IFD helpers ────────────────────────────────────────────────

fn n_planes(layout: &[MainIfd]) -> u64 {
    layout.iter().filter(|m| matches!(m, MainIfd::Plane(_))).count() as u64
}

/// --roi: (width, height) of the cropped base and of each 1/4-step level below it,
/// matching what `write_reduced_levels` writes.
fn roi_pyramid(base: (u32, u32), n_levels: u32) -> Vec<(u32, u32)> {
    (0..=n_levels).map(|k| (base.0.div_ceil(4u32.pow(k)), base.1.div_ceil(4u32.pow(k)))).collect()
}

/// Copies main IFD `dir` of `src` (an OME label/macro/thumbnail image) to a new IFD of
/// `dst` without re-encoding: the image-structure tags plus the raw strips or tiles.
/// Keeping these IFDs in place keeps the OME-XML <TiffData> IFD numbers valid, so a
/// failed copy panics (the file is reported as failed) instead of shifting them.
unsafe fn copy_aux_ifd(src: *mut TIFF, dst: *mut TIFF, dir: u32, src_path: &str) {
    if unsafe { TIFFSetDirectory(src, dir) } == 0 {
        panic!("{src_path}: cannot read auxiliary IFD {dir}");
    }
    let tiled = unsafe { TIFFIsTiled(src) } != 0;
    unsafe {
        let mut v32: u32 = 0;
        for tag in [TIFFTAG_SUBFILETYPE, TIFFTAG_IMAGEWIDTH, TIFFTAG_IMAGELENGTH] {
            if TIFFGetField(src, tag, &mut v32 as *mut u32) != 0 { TIFFSetField(dst, tag, v32); }
        }
        let layout_tags: &[u32] = if tiled { &[TIFFTAG_TILEWIDTH, TIFFTAG_TILELENGTH] } else { &[TIFFTAG_ROWSPERSTRIP] };
        for &tag in layout_tags {
            if TIFFGetField(src, tag, &mut v32 as *mut u32) != 0 { TIFFSetField(dst, tag, v32); }
        }
        // Compression first: codec-specific tags (Predictor) are only known once it is set.
        let mut v16: u16 = 0;
        for tag in [TIFFTAG_COMPRESSION, TIFFTAG_BITSPERSAMPLE, TIFFTAG_SAMPLESPERPIXEL,
                    TIFFTAG_PHOTOMETRIC, TIFFTAG_PLANARCONFIG, TIFFTAG_SAMPLEFORMAT,
                    TIFFTAG_ORIENTATION, TIFFTAG_PREDICTOR, TIFFTAG_RESOLUTIONUNIT] {
            if TIFFGetField(src, tag, &mut v16 as *mut u16) != 0 { TIFFSetField(dst, tag, v16 as u32); }
        }
        let mut res: f32 = 0.0;
        for tag in [TIFFTAG_XRESOLUTION, TIFFTAG_YRESOLUTION] {
            if TIFFGetField(src, tag, &mut res as *mut f32) != 0 { TIFFSetField(dst, tag, res as f64); }
        }
        let (mut sh, mut sv): (u16, u16) = (0, 0);
        if TIFFGetField(src, TIFFTAG_YCBCRSUBSAMPLING, &mut sh as *mut u16, &mut sv as *mut u16) != 0 {
            TIFFSetField(dst, TIFFTAG_YCBCRSUBSAMPLING, sh as u32, sv as u32);
        }
        let mut n_extra: u16 = 0;
        let mut extra: *const u16 = std::ptr::null();
        if TIFFGetField(src, TIFFTAG_EXTRASAMPLES, &mut n_extra as *mut u16, &mut extra as *mut *const u16) != 0 {
            TIFFSetField(dst, TIFFTAG_EXTRASAMPLES, n_extra as u32, extra);
        }
        let (mut r, mut g, mut b): (*const u16, *const u16, *const u16) =
            (std::ptr::null(), std::ptr::null(), std::ptr::null());
        if TIFFGetField(src, TIFFTAG_COLORMAP, &mut r as *mut *const u16,
            &mut g as *mut *const u16, &mut b as *mut *const u16) != 0 {
            TIFFSetField(dst, TIFFTAG_COLORMAP, r, g, b);
        }
        if let Some(tables) = crate::pipeline::jpegtables_ext::get_jpeg_tables(src, src_path) {
            crate::pipeline::writer::set_jpeg_tables(dst, &tables);
        }
    }

    let (n_chunks, counts_tag) = if tiled {
        (unsafe { TIFFNumberOfTiles(src) }, TIFFTAG_TILEBYTECOUNTS)
    } else {
        (unsafe { TIFFNumberOfStrips(src) }, TIFFTAG_STRIPBYTECOUNTS)
    };
    let mut counts_ptr: *const u64 = std::ptr::null();
    if unsafe { TIFFGetField(src, counts_tag, &mut counts_ptr as *mut *const u64) } == 0 || counts_ptr.is_null() {
        panic!("{src_path}: auxiliary IFD {dir} has no byte counts");
    }
    let counts = unsafe { std::slice::from_raw_parts(counts_ptr, n_chunks as usize) }.to_vec();
    for (i, &len) in counts.iter().enumerate() {
        let mut buf = vec![0u8; len as usize];
        let i = i as u32;
        let ok = unsafe {
            let buf_ptr = buf.as_mut_ptr() as *mut c_void;
            if tiled {
                TIFFReadRawTile(src, i, buf_ptr, len as i64) == len as i64
                    && TIFFWriteRawTile(dst, i, buf_ptr, len as i64) == len as i64
            } else {
                TIFFReadRawStrip(src, i, buf_ptr, len as i64) == len as i64
                    && TIFFWriteRawStrip(dst, i, buf_ptr, len as i64) == len as i64
            }
        };
        assert!(ok, "{src_path}: failed to copy chunk {i} of auxiliary IFD {dir}");
    }
    unsafe { TIFFWriteDirectory(dst); }
}

// ─── Per-file processing ──────────────────────────────────────────────────────

fn compression_name(code: u16) -> String {
    if code as u32 == COMPRESSION_JPEG {
        "JPEG".to_string()
    } else if is_jp2k(code as u32) {
        "JPEG 2000".to_string()
    } else {
        format!("compression {}", code)
    }
}

// Detail for a level that is copied/re-tiled without resampling (in and out
// geometry are identical).
fn passthrough_detail(src_path: &str, out_path: &str, lv: &TiffLevel) -> crate::logger::ConversionDetail {
    crate::logger::ConversionDetail {
        input_path:  src_path.to_string(),
        output_path: out_path.to_string(),
        encoding:    compression_name(lv.compression),
        in_tile:  Some((lv.tile_w, lv.tile_h)),
        out_tile: Some((lv.tile_w, lv.tile_h)),
        in_dim:   Some((lv.img_w, lv.img_h)),
        out_dim:  Some((lv.img_w, lv.img_h)),
        in_mpp:   lv.mpp_x,
        out_mpp:  lv.mpp_x,
    }
}

fn process_file(src_path: &str, out_dir: &str, out_stem: &str, args: &crate::Args, pb: &ProgressBar,
                roi: Option<&crate::roi::Roi>) -> (Vec<String>, crate::logger::ConversionDetail) {
    let src = match TiffSource::open(src_path) {
        Ok(src) => src,
        Err(e) => {
            eprintln!("  [warn ] {src_path}: {e}; skipping");
            return (Vec::new(), Default::default());
        }
    };
    let (mut src_levels, icc_profile, ome_xml, mut layout, _meta) = src.into_parts();

    // OpenSlide output holds a single pyramid: keep the first OME plane only and
    // drop the other planes and auxiliary images (label, macro, ...).
    if args.openslide && layout.len() > 1 {
        let planes = n_planes(&layout);
        if planes > 1 {
            eprintln!("  [warn ] {src_path}: --openslide writes only the first of {planes} OME planes");
        }
        layout.truncate(1);
    }
    if args.verbose && ome_xml.is_some() {
        vlog(Some(pb), format!("  [ome  ] {} plane(s), {} auxiliary image(s)",
            n_planes(&layout), layout.len() as u64 - n_planes(&layout)));
    }

    // --icc-bake: no ICC profile → copy and return
    if args.icc_bake && icc_profile.is_none() {
        let src_name = Path::new(src_path).file_name()
            .unwrap_or_default().to_string_lossy().to_string();
        eprintln!("  [warn ] No ICC profile found in {src_name}; copying to output as-is.");
        let dst = std::path::PathBuf::from(out_dir).join(&src_name);
        if let Err(e) = std::fs::copy(src_path, &dst) {
            eprintln!("  [error] Copy failed for {src_name}: {e}");
        }
        let detail = passthrough_detail(src_path, &dst.to_string_lossy(), &src_levels[0]);
        return (vec!["copy".to_string()], detail);
    }

    // --scale half / quarter / 20x: classify the source magnification bucket;
    // skip (20x only) when the MPP is unknown or the source is coarser than
    // 20x (upscaling not supported). half/quarter always halve/quarter,
    // regardless of source MPP.
    let mag_factor: u32 = if args.quarter() {
        4
    } else if args.half() {
        2
    } else if args.mag_20x() {
        match crate::factor_to_20x(src_levels[0].mpp_x) {
            Some(f) => f,
            None => {
                eprintln!("  [skip ] {src_path}: source MPP unknown or ≥0.7 µm/px (--scale 20x cannot upscale)");
                return (Vec::new(), Default::default());
            }
        }
    } else {
        1
    };

    // --scale 20x at native 20x (or no --scale, i.e. a --roi directory without a
    // matching GeoJSON), no color conversion requested: copy through as-is.
    if (args.scale.is_none() || (args.mag_20x() && mag_factor == 1)) && !args.icc_bake && roi.is_none() {
        let src_name = Path::new(src_path).file_name()
            .unwrap_or_default().to_string_lossy().to_string();
        let dst = std::path::PathBuf::from(out_dir).join(&src_name);
        if let Err(e) = std::fs::copy(src_path, &dst) {
            eprintln!("  [error] Copy failed for {src_name}: {e}");
        }
        let detail = passthrough_detail(src_path, &dst.to_string_lossy(), &src_levels[0]);
        return (vec!["copy".to_string()], detail);
    }

    // Pure 1:1 ICC bake: plain --icc-bake, or --scale 20x already at native 20x.
    // With --roi the crop path below handles the bake.
    if roi.is_none() && args.icc_bake && args.mpp().is_none() && !args.half() && !args.quarter() && (!args.mag_20x() || mag_factor == 1) {
        let icc = icc_profile.as_deref().unwrap();
        let out_path = if args.openslide {
            format!("{out_dir}/{out_stem}.tiff")
        } else {
            format!("{out_dir}/{out_stem}.ome.tiff")
        };
        if let Some(xform) = crate::build_icc_transform(icc) {
            if args.verbose {
                vlog(Some(pb), format!("  [icc  ] baking {} bytes → sRGB", icc.len()));
            }
            process_file_icc_bake_only(src_path, out_dir, out_stem, args, xform, &src_levels, &layout, ome_xml.as_deref(), pb);
        } else {
            eprintln!("  [error] Invalid ICC profile in {src_path}; skipping.");
        }
        let detail = passthrough_detail(src_path, &out_path, &src_levels[0]);
        return (vec!["ICC".to_string()], detail);
    }

    // Reaching here without --scale means --roi crop at full resolution.
    let crop = args.scale.is_none();

    // --scale half/quarter with unknown source MPP: derive a synthetic 1.0 µm/px
    // base so downstream MPP-based level selection still works, but remember
    // to blank the resolution tags on output (see mpp_unknown below).
    let mpp_unknown = src_levels[0].mpp_x <= 0.0;
    if (args.half() || args.quarter() || crop) && mpp_unknown {
        let bw = src_levels[0].img_w as f64;
        let bh = src_levels[0].img_h as f64;
        src_levels[0].mpp_x = 1.0;
        src_levels[0].mpp_y = 1.0;
        for lv in src_levels.iter_mut().skip(1) {
            if lv.img_w > 0 { lv.mpp_x = bw / lv.img_w as f64; }
            if lv.img_h > 0 { lv.mpp_y = bh / lv.img_h as f64; }
        }
    }

    let base = &src_levels[0];
    if args.verbose {
        vlog(Some(pb), format!("[src] {}  {}x{}  {:.4} µm/px  {} levels",
            src_path, base.img_w, base.img_h, base.mpp_x, src_levels.len()));
        let icc_msg = match &icc_profile {
            Some(icc) => format!("  [icc  ] {} bytes", icc.len()),
            None      => "  [icc  ] not found".to_string(),
        };
        vlog(Some(pb), &icc_msg);
    }

    if !args.mag_20x() && !args.half() && !args.quarter() {
        if let Some(t) = args.mpp() {
            if base.mpp_x <= 0.0 {
                eprintln!("  [error] Cannot determine resolution for {src_path}: \
                    no XRESOLUTION tag and no 'MPP = <value>' in ImageDescription. Skipping.");
                return (Vec::new(), Default::default());
            }
            if t <= base.mpp_x {
                eprintln!(
                    "  [warn ] requested MPP {:.4} µm/px ≤ source {:.4} µm/px (upscaling not supported); {}",
                    t, base.mpp_x,
                    if args.icc_bake && roi.is_none() { "applying ICC bake at 1:1" } else { "skipping" }
                );
                if args.icc_bake && roi.is_none() {
                    let icc = icc_profile.as_deref().unwrap();
                    let out_path = if args.openslide {
                        format!("{out_dir}/{out_stem}.tiff")
                    } else {
                        format!("{out_dir}/{out_stem}.ome.tiff")
                    };
                    if let Some(xform) = crate::build_icc_transform(icc) {
                        process_file_icc_bake_only(src_path, out_dir, out_stem, args, xform, &src_levels, &layout, ome_xml.as_deref(), pb);
                        let detail = passthrough_detail(src_path, &out_path, &src_levels[0]);
                        return (vec!["ICC".to_string()], detail);
                    } else {
                        eprintln!("  [error] Invalid ICC profile in {src_path}; skipping.");
                    }
                }
                return (Vec::new(), Default::default());
            }
        }
    }

    let decode_shift: u32 = if args.mag_20x() || args.half() || args.quarter() { mag_factor.trailing_zeros() } else { 0 };
    let target_mpp = if args.mag_20x() || args.half() || args.quarter() || crop { base.mpp_x * mag_factor as f64 } else { args.mpp().unwrap() };
    let jp2k_svs_skip: Option<usize> = if !args.icc_bake && is_jp2k(base.compression as u32) {
        let skip = src_levels.iter()
            .take_while(|lv| lv.mpp_x < target_mpp * 0.9)
            .count();
        let has_match = src_levels.get(skip)
            .map(|lv| (lv.mpp_x - target_mpp).abs() / target_mpp < 0.1)
            .unwrap_or(false);
        // With --roi a 1:1 match (skip == 0) is also raw-copied, with white fill.
        if (skip > 0 || roi.is_some()) && has_match { Some(skip) } else { None }
    } else {
        None
    };

    // --roi without --openslide stays OME-TIFF, which carries the JP2K tiles as well.
    let jp2k_ome = jp2k_svs_skip.is_some() && roi.is_some() && !args.openslide;
    let out_path = if jp2k_svs_skip.is_some() && !jp2k_ome {
        format!("{out_dir}/{out_stem}.svs")
    } else if args.openslide {
        format!("{out_dir}/{out_stem}.tiff")
    } else {
        format!("{out_dir}/{out_stem}.ome.tiff")
    };
    let tmp_path = format!("{out_path}.tmp");

    let mut ops: Vec<String> = vec!["repack".to_string()];
    if args.quarter() {
        ops.push("quarter".to_string());
    } else if args.half() {
        ops.push("half".to_string());
    } else if args.mag_20x() {
        ops.push("20x downsample".to_string());
    } else if args.mpp().is_some() {
        ops.push(format!("mpp {:.4} downsample", target_mpp));
    }
    if args.icc_bake { ops.push("ICC".to_string()); }
    if roi.is_some() { ops.push("ROI".to_string()); }

    if let Some(skip) = jp2k_svs_skip {
        let lv = &src_levels[skip];
        let roi_crop = match roi {
            None => None,
            Some(r) => match roi_crop_of(r, (lv.img_w.div_ceil(lv.tile_w), lv.img_h.div_ceil(lv.tile_h)),
                (lv.tile_w, lv.tile_h), (lv.img_w, lv.img_h), (src_levels[0].img_w, src_levels[0].img_h), src_path) {
                None => return (Vec::new(), Default::default()),
                c => c,
            },
        };
        let out_dim = roi_crop.as_ref().map_or((lv.img_w, lv.img_h),
            |c| c.dim((lv.img_w, lv.img_h), (lv.tile_w, lv.tile_h)));
        let lv_ome_xml = jp2k_ome.then(|| match ome_xml {
            // jp2k_ome implies --roi: the levels below the cropped base are rebuilt at 1/4 steps.
            Some(ref orig) => crate::pipeline::ome::update_ome_xml_for_output(
                orig, &roi_pyramid(out_dim, roi_reduced_levels(out_dim)), lv.mpp_x, lv.mpp_y),
            None => crate::pipeline::ome::generate_tiff_ome_xml(
                out_stem, out_dim.0, out_dim.1, lv.mpp_x, lv.mpp_y, lv.spp as u32),
        });
        write_jp2k_svs_from_tiff(src_path, &src_levels[skip..], &layout, skip, &tmp_path, args.verbose, pb,
            roi_crop.as_ref(), args.quality, lv_ome_xml);
        std::fs::rename(&tmp_path, &out_path)
            .expect("Failed to rename tmp to output");
        let detail = crate::logger::ConversionDetail {
            input_path:  src_path.to_string(),
            output_path: out_path.clone(),
            encoding:    compression_name(src_levels[0].compression),
            in_tile:  Some((src_levels[0].tile_w, src_levels[0].tile_h)),
            out_tile: Some((src_levels[skip].tile_w, src_levels[skip].tile_h)),
            in_dim:   Some((src_levels[0].img_w, src_levels[0].img_h)),
            out_dim:  Some(out_dim),
            in_mpp:   src_levels[0].mpp_x,
            out_mpp:  src_levels[skip].mpp_x,
        };
        return (ops, detail);
    }

    let mut output_levels = compute_output_levels(&src_levels, target_mpp, args.verbose, args.icc_bake, decode_shift);
    if mpp_unknown {
        for lv in output_levels.iter_mut() {
            lv.actual_mpp_x = 0.0;
            lv.actual_mpp_y = 0.0;
        }
    }
    if output_levels.is_empty() {
        eprintln!("  [warn] No output levels produced for {src_path}");
        return (Vec::new(), Default::default());
    }

    // --roi: keep only the base level, cropped to the tiles touching an annotation;
    // the levels below it are rebuilt from its tiles at 1/4 steps.
    let roi_crop = match roi {
        None => None,
        Some(r) => {
            output_levels.truncate(1);
            let lv = &mut output_levels[0];
            let src_lv = &src_levels[lv.src_idx];
            let grid = (lv.out_img_w.div_ceil(lv.out_tile_w), lv.out_img_h.div_ceil(lv.out_tile_h));
            // A passthrough tile is one source tile; a resampled one is built from 2x2.
            let k = if lv.passthrough { 1 } else { 2 };
            let Some(crop) = roi_crop_of(r, grid, (k * src_lv.tile_w, k * src_lv.tile_h),
                (src_lv.img_w, src_lv.img_h), (src_levels[0].img_w, src_levels[0].img_h), src_path)
            else {
                return (Vec::new(), Default::default());
            };
            (lv.out_img_w, lv.out_img_h) = crop.dim((lv.out_img_w, lv.out_img_h), (lv.out_tile_w, lv.out_tile_h));
            if args.verbose {
                vlog(Some(pb), format!("  [roi  ] crop tiles {}x{} at ({}, {}) → {}x{}  {}/{} tiles inside annotations",
                    crop.cols, crop.rows, crop.c0, crop.r0, lv.out_img_w, lv.out_img_h,
                    crop.mask.iter().filter(|&&b| b).count(), crop.mask.len()));
            }
            Some(crop)
        }
    };
    let roi_levels = if roi_crop.is_some() {
        roi_reduced_levels((output_levels[0].out_img_w, output_levels[0].out_img_h))
    } else { 0 };

    let total_tiles: u64 = output_levels.iter()
        .map(|lv| {
            if let Some(c) = &roi_crop {
                (c.cols * c.rows) as u64 + roi_reduced_tiles((c.cols, c.rows), roi_levels)
            } else if lv.passthrough {
                src_levels[lv.src_idx].n_tiles as u64
            } else {
                let out_ntx = (lv.out_img_w + lv.out_tile_w - 1) / lv.out_tile_w;
                let out_nty = (lv.out_img_h + lv.out_tile_h - 1) / lv.out_tile_h;
                (out_ntx * out_nty) as u64
            }
        })
        .sum();
    pb.set_length(total_tiles * n_planes(&layout));

    let ome = !args.openslide;

    let base_lv = &output_levels[0];
    let image_desc_c: Option<CString> = if ome {
        let xml = if let Some(ref orig) = ome_xml {
            let pyramid: Vec<(u32, u32)> = if roi_crop.is_some() {
                roi_pyramid((base_lv.out_img_w, base_lv.out_img_h), roi_levels)
            } else {
                output_levels.iter().map(|lv| (lv.out_img_w, lv.out_img_h)).collect()
            };
            crate::pipeline::ome::update_ome_xml_for_output(
                orig, &pyramid,
                base_lv.actual_mpp_x, base_lv.actual_mpp_y,
            )
        } else {
            crate::pipeline::ome::generate_tiff_ome_xml(
                out_stem,
                base_lv.out_img_w, base_lv.out_img_h,
                base_lv.actual_mpp_x, base_lv.actual_mpp_y,
                src_levels[base_lv.src_idx].spp as u32,
            )
        };
        Some(CString::new(xml).unwrap())
    } else {
        None
    };

    let base_src = &src_levels[output_levels[0].src_idx];
    let out_spp: u32 = if base_src.spp >= 3 { 3 } else { 1 };
    let out_photometric = if out_spp == 1 { PHOTOMETRIC_MINISBLACK } else { PHOTOMETRIC_YCBCR };

    let fir_alg = match args.kernel {
        FilterType::Nearest    => fir::ResizeAlg::Nearest,
        FilterType::Triangle   => fir::ResizeAlg::Convolution(fir::FilterType::Bilinear),
        FilterType::CatmullRom => fir::ResizeAlg::Convolution(fir::FilterType::CatmullRom),
        FilterType::Gaussian   => fir::ResizeAlg::Convolution(fir::FilterType::Lanczos3),
        FilterType::Lanczos3   => fir::ResizeAlg::Convolution(fir::FilterType::Lanczos3),
    };
    let fir_pixel_type = if out_spp == 1 { fir::PixelType::U8 } else { fir::PixelType::U8x3 };
    let resize_opts = fir::ResizeOptions::new().resize_alg(fir_alg);

    let icc_transform_arc: Option<Arc<crate::IccTransform>> = if args.icc_bake {
        icc_profile.as_deref().and_then(crate::build_icc_transform)
    } else {
        None
    };
    if args.icc_bake && args.verbose {
        let msg = if icc_transform_arc.is_some() {
            "  [icc  ] baking → sRGB (resample+bake mode)".to_string()
        } else {
            "  [icc  ] transform build failed; skipping ICC bake".to_string()
        };
        vlog(Some(pb), &msg);
    }

    let src_c   = CString::new(src_path).unwrap();
    let tmp_c   = CString::new(tmp_path.as_str()).unwrap();
    let r_mode  = CString::new("r").unwrap();
    let w8_mode = CString::new("w8").unwrap();

    let src_tiff = unsafe { TIFFOpen(src_c.as_ptr(), r_mode.as_ptr()) };
    if src_tiff.is_null() {
        eprintln!("  [error] Cannot re-open: {src_path}");
        return (Vec::new(), Default::default());
    }
    let dst_tiff = unsafe { TIFFOpen(tmp_c.as_ptr(), w8_mode.as_ptr()) };
    if dst_tiff.is_null() {
        eprintln!("  [error] Cannot create: {tmp_path}");
        unsafe { TIFFClose(src_tiff); }
        return (Vec::new(), Default::default());
    }

    let n_subifds = output_levels.len() - 1 + roi_levels as usize;
    let chunk_size = (rayon::current_num_threads() * 4).max(1);

    // Planes share the first plane's geometry and encoding, so `src_levels` describes
    // every plane; only navigation goes through the plane's own levels.
    for (main_idx, main) in layout.iter().enumerate() {
        let plane_levels = match main {
            MainIfd::Plane(levels) => levels,
            MainIfd::Aux(dir) => { unsafe { copy_aux_ifd(src_tiff, dst_tiff, *dir, src_path); } continue; }
        };
        if ome && n_subifds > 0 {
            let zeros: Vec<u64> = vec![0u64; n_subifds];
            unsafe { TIFFSetField(dst_tiff, TIFFTAG_SUBIFD, n_subifds as u32, zeros.as_ptr()); }
        }
        // --roi: fed with the base level's tiles to build the 1/4-step levels.
        let mut reducer: Option<Reducer> = None;

        for (lv_idx, lv_out) in output_levels.iter().enumerate() {
            let src_lv  = &src_levels[lv_out.src_idx];
            let is_base = lv_idx == 0;
            let subfile = if is_base { 0u32 } else { FILETYPE_REDUCEDIMAGE };

            unsafe { navigate(src_tiff, lv_out.src_idx, plane_levels); }

            let (src_subsamp_h, src_subsamp_v) = if src_lv.compression as u32 == COMPRESSION_JPEG
                && src_lv.photometric as u32 == PHOTOMETRIC_YCBCR
            {
                let mut sh: u16 = 2;
                let mut sv: u16 = 2;
                unsafe { TIFFGetField(src_tiff, TIFFTAG_YCBCRSUBSAMPLING,
                    &mut sh as *mut u16, &mut sv as *mut u16); }
                (sh, sv)
            } else {
                (2u16, 2u16)
            };

            let ifd_compr = if lv_out.passthrough { src_lv.compression as u32 } else { COMPRESSION_JPEG };
            let ifd_photo = if lv_out.passthrough { src_lv.photometric as u32 } else { out_photometric };
            unsafe { set_tiff_ifd_tags(dst_tiff, subfile,
                lv_out.out_img_w, lv_out.out_img_h,
                lv_out.out_tile_w, lv_out.out_tile_h,
                ifd_compr, ifd_photo, out_spp,
                lv_out.actual_mpp_x, lv_out.actual_mpp_y); }

            if lv_out.passthrough {
                if src_lv.photometric as u32 == PHOTOMETRIC_YCBCR {
                    unsafe { TIFFSetField(dst_tiff, TIFFTAG_YCBCRSUBSAMPLING,
                        src_subsamp_h as u32, src_subsamp_v as u32); }
                }
                if src_lv.compression as u32 == COMPRESSION_JPEG {
                    if let Some(tables) = crate::pipeline::jpegtables_ext::get_jpeg_tables(src_tiff, src_path) {
                        let set_ok = unsafe { TIFFSetField(dst_tiff, TIFFTAG_JPEGTABLES,
                            tables.len() as u32, tables.as_ptr()) };
                        assert!(set_ok == 1, "TIFFSetField(JPEGTABLES) failed — output tiles would be undecodable");
                    }
                }
            } else if out_photometric == PHOTOMETRIC_YCBCR {
                unsafe { TIFFSetField(dst_tiff, TIFFTAG_YCBCRSUBSAMPLING, 2u32, 2u32); }
            }

            if is_base {
                if let Some(ref desc) = image_desc_c.as_ref().filter(|_| main_idx == 0) {
                    unsafe { TIFFSetField(dst_tiff, TIFFTAG_IMAGEDESCRIPTION, desc.as_ptr()); }
                }
                if !args.icc_bake {
                    if let Some(ref icc) = icc_profile {
                        unsafe { TIFFSetField(dst_tiff, TIFFTAG_ICCPROFILE,
                            icc.len() as u32, icc.as_ptr() as *const c_void); }
                    }
                }
            }

            let raw_buf_size = (unsafe { TIFFTileSize(src_tiff) } as usize)
                .max(src_lv.tile_w as usize * src_lv.tile_h as usize * src_lv.spp as usize)
                .max(1 << 17);

            let n_tiles  = roi_crop.as_ref().map_or(src_lv.n_tiles, |c| c.cols * c.rows);
            let tile_ids: Vec<u32> = (0..n_tiles).collect();

            let pix_size    = src_lv.tile_w as usize
                * src_lv.tile_h as usize
                * src_lv.spp as usize;
            let src_is_jp2k   = is_jp2k(src_lv.compression as u32);
            let src_is_jpeg   = src_lv.compression as u32 == COMPRESSION_JPEG;
            let src_jp2k_is_ycbcr =
                src_is_jp2k && src_lv.compression as u32 == COMPRESSION_APERIO_JP2_YCBCR;

            let n_reduce: u32 = if src_is_jp2k && !lv_out.passthrough {
                if decode_shift > 0 {
                    decode_shift  // out_tile was derived from ceil(src/2^decode_shift), exact
                } else {
                    let nat_otw = (lv_out.out_tile_w / 2).max(1);
                    let nat_oth = (lv_out.out_tile_h / 2).max(1);
                    let scale_down = (src_lv.tile_w as f64 / nat_otw as f64)
                        .min(src_lv.tile_h as f64 / nat_oth as f64);
                    if scale_down > 1.0 { scale_down.log2().floor() as u32 } else { 0 }
                }
            } else {
                0
            };

            let jpeg_tables_arc: Option<Arc<Vec<u8>>> = if src_is_jpeg && !lv_out.passthrough {
                crate::pipeline::jpegtables_ext::get_jpeg_tables(src_tiff, src_path).map(Arc::new)
            } else {
                None
            };

            if lv_out.passthrough {
                // --roi: tiles outside the annotations get a white tile matching the
                // source's photometric/subsampling instead of a raw copy.
                let roi_fill = roi_crop.as_ref().map(|c| {
                    let white = crate::pipeline::encode::white_jpeg_tile(src_lv.tile_w, src_lv.tile_h, out_spp,
                        src_lv.photometric as u32 == PHOTOMETRIC_RGB, (src_subsamp_h, src_subsamp_v), args.quality);
                    reducer = Some(Reducer::new((c.cols, c.rows), (src_lv.tile_w, src_lv.tile_h), out_spp, args.quality,
                        crate::pipeline::jpegtables_ext::get_jpeg_tables(src_tiff, src_path),
                        out_spp == 3 && src_lv.photometric as u32 == PHOTOMETRIC_RGB, false));
                    (c, white)
                });
                for chunk in tile_ids.chunks(chunk_size) {
                    let raw_chunk: Vec<(u32, Vec<u8>)> = chunk.iter()
                        .map(|&tile_num| {
                            let mut src_tile = tile_num;
                            if let Some((crop, white)) = &roi_fill {
                                if !crop.mask[tile_num as usize] {
                                    return (tile_num, white.clone());
                                }
                                src_tile = crop.full_id(tile_num);
                            }
                            let mut buf = vec![0u8; raw_buf_size];
                            let n = unsafe { TIFFReadRawTile(src_tiff, src_tile,
                                buf.as_mut_ptr() as *mut c_void, buf.len() as i64) };
                            if n > 0 { buf.truncate(n as usize); (tile_num, buf) }
                            else { (tile_num, Vec::new()) }
                        })
                        .collect();
                    for (tile_num, data) in raw_chunk {
                        if !data.is_empty() {
                            unsafe { TIFFWriteRawTile(dst_tiff, tile_num,
                                data.as_ptr() as *mut c_void, data.len() as i64); }
                            if let Some(r) = reducer.as_mut() { r.push(tile_num, &data); }
                        }
                        pb.inc(1);
                    }
                }
            } else {
                let src_tile_w = src_lv.tile_w;
                let src_tile_h = src_lv.tile_h;
                let out_tile_w = lv_out.out_tile_w;
                let out_tile_h = lv_out.out_tile_h;
                let src_ntx = (src_lv.img_w + src_tile_w - 1) / src_tile_w;
                let src_nty = (src_lv.img_h + src_tile_h - 1) / src_tile_h;
                let out_ntx = (lv_out.out_img_w + out_tile_w - 1) / out_tile_w;
                let out_nty = (lv_out.out_img_h + out_tile_h - 1) / out_tile_h;
                let out_tile_ids: Vec<u32> = (0..out_ntx * out_nty).collect();

                // --roi: output tiles are numbered within the crop; (oc0, or0) maps
                // them back onto the full grid of 2x2 source-tile cells.
                let (oc0, or0) = roi_crop.as_ref().map_or((0, 0), |c| (c.c0, c.r0));
                let roi_fill = roi_crop.as_ref().map(|c| {
                    reducer = Some(Reducer::new((c.cols, c.rows), (out_tile_w, out_tile_h), out_spp, args.quality,
                        None, false, false));
                    (c.mask.clone(), crate::pipeline::encode::white_jpeg_tile(out_tile_w, out_tile_h, out_spp, false, (2, 2), args.quality))
                });

                let enc_params = Arc::new(EncodeParams {
                    quality:           args.quality,
                    src_tile_w,
                    src_tile_h,
                    out_tile_w,
                    out_tile_h,
                    spp:               out_spp,
                    resize_opts:       resize_opts.clone(),
                    fpt:               fir_pixel_type,
                    src_is_jpeg,
                    src_jp2k_is_ycbcr,
                    src_photometric:   src_lv.photometric as u32,
                    n_reduce,
                    decode_shift,
                    jpeg_tables:       jpeg_tables_arc.clone(),
                    icc_transform:     icc_transform_arc.clone(),
                    roi_fill,
                });

                let (raw_tx, raw_rx) = mpsc::sync_channel::<RawChunk>(2);
                let (enc_tx, enc_rx) = mpsc::sync_channel::<EncChunk>(2);
                let params_t = Arc::clone(&enc_params);
                let compute_handle = std::thread::spawn(move || {
                    compute_thread(raw_rx, enc_tx, |id, quads| encode_one_tile(id, quads, &params_t));
                });

                let mut jpegtables_registered = false;
                let mut pending_write: Option<EncChunk> = None;

                for chunk in out_tile_ids.chunks(chunk_size) {
                    let raw_chunk: RawChunk = chunk.iter()
                        .map(|&out_id| {
                            let oc  = out_id % out_ntx + oc0;
                            let or_ = out_id / out_ntx + or0;
                            let mut quads: RawQuad = [None, None, None, None];
                            // Outside --roi: skip source reads; encode_one_tile emits white.
                            if enc_params.roi_fill.as_ref().is_some_and(|(m, _)| !m[out_id as usize]) {
                                return (out_id, quads);
                            }
                            for qi in 0..4usize {
                                let dc = (qi % 2) as u32;
                                let dr = (qi / 2) as u32;
                                let sc = 2 * oc + dc;
                                let sr = 2 * or_ + dr;
                                if sc >= src_ntx || sr >= src_nty { continue; }
                                let tile_num = sr * src_ntx + sc;
                                if src_is_jp2k {
                                    let mut buf = vec![0u8; raw_buf_size];
                                    let n = unsafe { TIFFReadRawTile(src_tiff, tile_num,
                                        buf.as_mut_ptr() as *mut c_void, buf.len() as i64) };
                                    if n > 0 {
                                        buf.truncate(n as usize);
                                        quads[qi] = Some((buf, true));
                                    }
                                } else if src_is_jpeg {
                                    let mut raw_buf = vec![0u8; raw_buf_size];
                                    let raw_n = unsafe { TIFFReadRawTile(src_tiff, tile_num,
                                        raw_buf.as_mut_ptr() as *mut c_void,
                                        raw_buf.len() as i64) };
                                    if raw_n > 2
                                        && raw_buf[0] == 0xFF && raw_buf[1] == 0xD8
                                    {
                                        raw_buf.truncate(raw_n as usize);
                                        quads[qi] = Some((raw_buf, true));
                                    } else {
                                        let mut pix_buf = vec![0u8; pix_size];
                                        let n = unsafe { TIFFReadEncodedTile(src_tiff, tile_num,
                                            pix_buf.as_mut_ptr() as *mut c_void,
                                            pix_buf.len() as i64) };
                                        if n > 0 { quads[qi] = Some((pix_buf, false)); }
                                    }
                                } else {
                                    let mut buf = vec![0u8; pix_size];
                                    let n = unsafe { TIFFReadEncodedTile(src_tiff, tile_num,
                                        buf.as_mut_ptr() as *mut c_void, buf.len() as i64) };
                                    if n > 0 { quads[qi] = Some((buf, false)); }
                                }
                            }
                            (out_id, quads)
                        })
                        .collect();

                    raw_tx.send(raw_chunk).expect("compute thread dropped");

                    if let Some(prev) = pending_write.take() {
                        let n = prev.len() as u64;
                        unsafe { write_enc_chunk(dst_tiff, &prev, &mut jpegtables_registered); }
                        if let Some(r) = reducer.as_mut() { for (id, t) in &prev { r.push(*id, t); } }
                        pb.inc(n);
                    }

                    pending_write = enc_rx.recv().ok();
                }

                drop(raw_tx);

                if let Some(last) = pending_write.take() {
                    let n = last.len() as u64;
                    unsafe { write_enc_chunk(dst_tiff, &last, &mut jpegtables_registered); }
                    if let Some(r) = reducer.as_mut() { for (id, t) in &last { r.push(*id, t); } }
                    pb.inc(n);
                }
                for enc in enc_rx {
                    let n = enc.len() as u64;
                    unsafe { write_enc_chunk(dst_tiff, &enc, &mut jpegtables_registered); }
                    if let Some(r) = reducer.as_mut() { for (id, t) in &enc { r.push(*id, t); } }
                    pb.inc(n);
                }
                compute_handle.join().expect("compute thread panicked");
            }

            unsafe { TIFFWriteDirectory(dst_tiff); }
        }

        if let Some(r) = reducer {
            let b = &output_levels[0];
            unsafe { write_reduced_levels(dst_tiff, r, (b.out_img_w, b.out_img_h),
                (b.actual_mpp_x, b.actual_mpp_y), roi_levels, args.verbose, Some(pb)); }
        }
    }

    unsafe { TIFFClose(dst_tiff); }
    unsafe { TIFFClose(src_tiff); }

    if let Err(e) = std::fs::rename(&tmp_path, &out_path) {
        eprintln!("  [error] Failed to rename {tmp_path} → {out_path}: {e}");
        let _ = std::fs::remove_file(&tmp_path);
    }
    let detail = crate::logger::ConversionDetail {
        input_path:  src_path.to_string(),
        output_path: out_path.clone(),
        encoding:    compression_name(src_levels[0].compression),
        in_tile:  Some((src_levels[0].tile_w, src_levels[0].tile_h)),
        out_tile: Some((base_lv.out_tile_w, base_lv.out_tile_h)),
        in_dim:   Some((src_levels[0].img_w, src_levels[0].img_h)),
        out_dim:  Some((base_lv.out_img_w, base_lv.out_img_h)),
        in_mpp:   src_levels[0].mpp_x,
        out_mpp:  base_lv.actual_mpp_x,
    };
    (ops, detail)
}

// ─── Output pyramid computation ───────────────────────────────────────────────

fn compute_output_levels(
    src_levels: &[TiffLevel],
    target_mpp: f64,
    verbose: bool,
    icc_bake: bool,
    decode_shift: u32,
) -> Vec<OutputLevel> {
    let base_mpp = src_levels[0].mpp_x;
    let mut out  = Vec::new();

    for (i, src_lv_i) in src_levels.iter().enumerate() {
        let target_lv_mpp_x = target_mpp * (src_lv_i.mpp_x / base_mpp);
        let target_lv_mpp_y = target_mpp * (src_lv_i.mpp_y / base_mpp);

        let (best_idx, best) = src_levels.iter().enumerate()
            .min_by(|(_, a), (_, b)| {
                (a.mpp_x - target_lv_mpp_x).abs()
                    .partial_cmp(&(b.mpp_x - target_lv_mpp_x).abs()).unwrap()
            })
            .unwrap();

        let diff       = (best.mpp_x - target_lv_mpp_x).abs() / target_lv_mpp_x;
        let aligned    = best.tile_w % 16 == 0 && best.tile_h % 16 == 0;
        let passthrough = diff < 0.1
            && best.compression as u32 == COMPRESSION_JPEG
            && aligned
            && !icc_bake;

        let (out_img_w, out_img_h, out_tile_w, out_tile_h, actual_mpp_x, actual_mpp_y) =
            if passthrough {
                (best.img_w, best.img_h, best.tile_w, best.tile_h, best.mpp_x, best.mpp_y)
            } else if decode_shift > 0 && (is_jp2k(best.compression as u32)
                || best.compression as u32 == COMPRESSION_JPEG) {
                // JP2K DWT level-N and JPEG (turbojpeg) scaled decode both give
                // exactly ceil(tw/2^N) per quad; use that directly so the assembled
                // canvas matches out_tile exactly — no correction resize needed.
                let div = 1u32 << decode_shift;
                let nat_otw = ((best.tile_w + div - 1) / div).max(1);  // ceil(tw/div)
                let nat_oth = ((best.tile_h + div - 1) / div).max(1);
                let sx  = if best.tile_w > 0 { nat_otw as f64 / best.tile_w as f64 } else { 1.0 };
                let sy  = if best.tile_h > 0 { nat_oth as f64 / best.tile_h as f64 } else { 1.0 };
                let oiw = (best.img_w as f64 * sx).round() as u32;
                let oih = (best.img_h as f64 * sy).round() as u32;
                let amx = if nat_otw > 0 { best.mpp_x * best.tile_w as f64 / nat_otw as f64 } else { best.mpp_x };
                let amy = if nat_oth > 0 { best.mpp_y * best.tile_h as f64 / nat_oth as f64 } else { best.mpp_y };
                (oiw, oih, nat_otw * 2, nat_oth * 2, amx, amy)
            } else {
                let nat_otw = nearest_16(best.tile_w as f64 * best.mpp_x / target_lv_mpp_x);
                let nat_oth = nearest_16(best.tile_h as f64 * best.mpp_y / target_lv_mpp_y);
                let sx  = if best.tile_w > 0 { nat_otw as f64 / best.tile_w as f64 } else { 1.0 };
                let sy  = if best.tile_h > 0 { nat_oth as f64 / best.tile_h as f64 } else { 1.0 };
                let oiw = (best.img_w as f64 * sx).round() as u32;
                let oih = (best.img_h as f64 * sy).round() as u32;
                let amx = if nat_otw > 0 { best.mpp_x * best.tile_w as f64 / nat_otw as f64 } else { best.mpp_x };
                let amy = if nat_oth > 0 { best.mpp_y * best.tile_h as f64 / nat_oth as f64 } else { best.mpp_y };
                let otw = nat_otw * 2;
                let oth = nat_oth * 2;
                (oiw, oih, otw, oth, amx, amy)
            };

        if out_img_w.max(out_img_h) < MIN_PYRAMID_SIDE {
            if verbose {
                vlog(None, format!("  [skip ] lv{}  {}x{}  below MIN_PYRAMID_SIDE ({})",
                    i, out_img_w, out_img_h, MIN_PYRAMID_SIDE));
            }
            continue;
        }

        if verbose {
            let tag = if passthrough { "[pass ]" } else { "[resamp]" };
            if passthrough {
                vlog(None, format!("  {} lv{}  {}x{}  {:.4} µm/px  tile {}x{}",
                    tag, i, out_img_w, out_img_h, actual_mpp_x, out_tile_w, out_tile_h));
            } else {
                vlog(None, format!("  {} lv{}  {}x{}  {:.4} µm/px  src tile {}x{}→{}x{}",
                    tag, i, out_img_w, out_img_h, actual_mpp_x,
                    best.tile_w, best.tile_h, out_tile_w, out_tile_h));
            }
        }

        out.push(OutputLevel {
            out_img_w, out_img_h,
            out_tile_w, out_tile_h,
            actual_mpp_x, actual_mpp_y,
            src_idx: best_idx,
            passthrough,
        });
    }

    out
}

