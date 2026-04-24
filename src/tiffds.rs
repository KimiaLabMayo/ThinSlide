// TIFF/SVS downsampling logic for slean.
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
    TIFF,
    TIFFOpen, TIFFClose,
    TIFFGetField,
    TIFFSetField,
    TIFFSetDirectory, TIFFSetSubDirectory,
    TIFFNumberOfDirectories,
    TIFFNumberOfTiles,
    TIFFReadRawTile, TIFFReadEncodedTile,
    TIFFWriteRawTile, TIFFWriteDirectory,
    TIFFTileSize,
    TIFFTAG_IMAGEWIDTH, TIFFTAG_IMAGELENGTH,
    TIFFTAG_TILEWIDTH, TIFFTAG_TILELENGTH,
    TIFFTAG_COMPRESSION,
    TIFFTAG_PHOTOMETRIC,
    TIFFTAG_SAMPLESPERPIXEL, TIFFTAG_BITSPERSAMPLE,
    TIFFTAG_SAMPLEFORMAT, TIFFTAG_PLANARCONFIG, TIFFTAG_ORIENTATION,
    TIFFTAG_SUBFILETYPE, TIFFTAG_IMAGEDESCRIPTION, TIFFTAG_ICCPROFILE,
    TIFFTAG_JPEGTABLES, TIFFTAG_YCBCRSUBSAMPLING, TIFFTAG_SUBIFD,
    TIFFTAG_XRESOLUTION, TIFFTAG_YRESOLUTION, TIFFTAG_RESOLUTIONUNIT,
    COMPRESSION_JPEG,
    PHOTOMETRIC_RGB, PHOTOMETRIC_YCBCR, PHOTOMETRIC_MINISBLACK,
    RESUNIT_CENTIMETER, RESUNIT_INCH,
    SAMPLEFORMAT_UINT, PLANARCONFIG_CONTIG, ORIENTATION_TOPLEFT,
    FILETYPE_REDUCEDIMAGE,
};
use crate::{tile_align, nearest_16, MIN_PYRAMID_SIDE, xml_escape, split_jpeg_to_tables_and_tile};

const COMPRESSION_APERIO_JP2_YCBCR: u32 = 33003;
const COMPRESSION_APERIO_JP2_RGB: u32   = 33005;
const COMPRESSION_JP2000: u32           = 34712;

fn vlog(pb: Option<&ProgressBar>, msg: impl AsRef<str>) {
    if let Some(p) = pb { p.println(msg.as_ref()); }
    else { eprintln!("{}", msg.as_ref()); }
}

fn is_jp2k(c: u32) -> bool {
    matches!(c, COMPRESSION_APERIO_JP2_YCBCR | COMPRESSION_APERIO_JP2_RGB | COMPRESSION_JP2000)
}

// ─── Source pyramid description ───────────────────────────────────────────────

struct LevelDesc {
    img_w:       u32,
    img_h:       u32,
    tile_w:      u32,
    tile_h:      u32,
    mpp_x:       f64,
    mpp_y:       f64,
    compression: u16,
    photometric: u16,
    spp:         u16,
    n_tiles:     u32,
    nav:         LevelNav,
}

enum LevelNav {
    Dir(u32),
    SubDir(u64),
}

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
    half:              bool,
    jpeg_tables:       Option<Arc<Vec<u8>>>,
    icc_transform:     Option<Arc<crate::IccTransform>>,
}

fn encode_one_tile(out_id: u32, quads: &RawQuad, p: &EncodeParams) -> Option<(u32, Vec<u8>)> {
    let ch = p.spp as usize;
    const APP14_ADOBE_RGB: [u8; 16] = [
        0xFF, 0xEE, 0x00, 0x0E,
        b'A', b'd', b'o', b'b', b'e',
        0x00, 0x64, 0x00, 0x00, 0x00, 0x00, 0x00,
    ];

    let decoded: [Option<(Vec<u8>, u32, u32)>; 4] = std::array::from_fn(|qi| {
        let (data, is_raw_decode) = quads[qi].as_ref()?;
        if *is_raw_decode && p.src_is_jpeg {
            let fmt = if p.spp == 1 { turbojpeg::PixelFormat::GRAY } else { turbojpeg::PixelFormat::RGB };
            let inject_app14 = p.spp == 3 && p.src_photometric == PHOTOMETRIC_RGB;
            let combined: Vec<u8> = if let Some(ref tables) = p.jpeg_tables {
                let app14_len = if inject_app14 { APP14_ADOBE_RGB.len() } else { 0 };
                let mut v = Vec::with_capacity(2 + app14_len + (tables.len() - 4) + (data.len() - 2));
                v.extend_from_slice(&tables[0..2]);
                if inject_app14 { v.extend_from_slice(&APP14_ADOBE_RGB); }
                v.extend_from_slice(&tables[2..tables.len()-2]);
                v.extend_from_slice(&data[2..]);
                v
            } else { data.clone() };

            if p.half {
                let mut dec = turbojpeg::Decompressor::new().ok()?;
                dec.set_scaling_factor(turbojpeg::ScalingFactor::ONE_HALF).ok()?;
                let header = dec.read_header(&combined).ok()?;
                let scaled = header.scaled(turbojpeg::ScalingFactor::ONE_HALF);
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
        } else if *is_raw_decode {
            // JP2K
            let params = jpeg2k::DecodeParameters::default().reduce(p.n_reduce);
            let img = jpeg2k::Image::from_bytes_with(data, params).ok()?;
            let comps = img.components();
            if comps.is_empty() { return None; }
            let luma_w = comps[0].width() as usize;
            let luma_h = comps[0].height() as usize;
            if luma_w == 0 || luma_h == 0 { return None; }
            let mut pix: Vec<u8> = if p.spp == 1 || comps.len() < 3 {
                comps[0].data_u8().collect()
            } else {
                let y_u8:  Vec<u8> = comps[0].data_u8().collect();
                let cb_u8: Vec<u8> = comps[1].data_u8().collect();
                let cr_u8: Vec<u8> = comps[2].data_u8().collect();
                let cb_w = comps[1].width() as usize;
                let cb_h = comps[1].height() as usize;
                let cr_w = comps[2].width() as usize;
                let cr_h = comps[2].height() as usize;
                let mut buf = Vec::with_capacity(luma_w * luma_h * 3);
                for row in 0..luma_h {
                    for col in 0..luma_w {
                        let y = y_u8[row*luma_w+col];
                        let cb_col = (col*cb_w/luma_w).min(cb_w.saturating_sub(1));
                        let cb_row = (row*cb_h/luma_h).min(cb_h.saturating_sub(1));
                        let cb = cb_u8[cb_row*cb_w+cb_col];
                        let cr_col = (col*cr_w/luma_w).min(cr_w.saturating_sub(1));
                        let cr_row = (row*cr_h/luma_h).min(cr_h.saturating_sub(1));
                        let cr = cr_u8[cr_row*cr_w+cr_col];
                        buf.extend_from_slice(&[y, cb, cr]);
                    }
                }
                buf
            };
            let color_space = img.color_space();
            let needs_ycbcr_cvt = p.spp == 3 && (
                matches!(color_space, jpeg2k::ColorSpace::SYCC)
                || (p.src_jp2k_is_ycbcr && !matches!(color_space, jpeg2k::ColorSpace::SRGB))
            );
            if needs_ycbcr_cvt {
                for c in pix.chunks_mut(3) {
                    let y  = c[0] as f32;
                    let cb = c[1] as f32 - 128.0;
                    let cr = c[2] as f32 - 128.0;
                    c[0] = (y + 1.40200 * cr).clamp(0.0, 255.0) as u8;
                    c[1] = (y - 0.34414*cb - 0.71414*cr).clamp(0.0, 255.0) as u8;
                    c[2] = (y + 1.77200 * cb).clamp(0.0, 255.0) as u8;
                }
            }
            Some((pix, luma_w as u32, luma_h as u32))
        } else {
            Some((data.clone(), p.src_tile_w, p.src_tile_h))
        }
    });

    if decoded.iter().all(|d| d.is_none()) { return None; }

    let (slot_w, slot_h) = decoded.iter()
        .filter_map(|d| d.as_ref().map(|(_, pw, ph)| (*pw, *ph)))
        .fold((1u32, 1u32), |(mw, mh), (w, h)| (mw.max(w), mh.max(h)));
    let canvas_w = slot_w * 2;
    let canvas_h = slot_h * 2;
    let mut canvas = vec![0u8; canvas_w as usize * canvas_h as usize * ch];

    for qi in 0..4usize {
        let Some((pixels, pw, ph)) = &decoded[qi] else { continue; };
        let dc = qi % 2;
        let dr = qi / 2;
        let ox = dc * slot_w as usize;
        let oy = dr * slot_h as usize;
        for row in 0..(*ph as usize) {
            let src_start = row * *pw as usize * ch;
            let dst_start = (oy + row) * canvas_w as usize * ch + ox * ch;
            let copy_len  = *pw as usize * ch;
            canvas[dst_start..dst_start + copy_len]
                .copy_from_slice(&pixels[src_start..src_start + copy_len]);
        }
    }

    if let Some(ref xform) = p.icc_transform {
        if ch == 3 {
            let mut dst = vec![0u8; canvas.len()];
            crate::apply_icc(xform, &canvas, &mut dst);
            canvas = dst;
        }
    }

    let resized: Vec<u8> = if canvas_w == p.out_tile_w && canvas_h == p.out_tile_h {
        canvas
    } else {
        let src_fir = fir::images::Image::from_vec_u8(canvas_w, canvas_h, canvas, p.fpt).ok()?;
        let mut dst_fir = fir::images::Image::new(p.out_tile_w, p.out_tile_h, p.fpt);
        fir::Resizer::new().resize(&src_fir, &mut dst_fir, &p.resize_opts).ok()?;
        dst_fir.into_vec()
    };

    let jpeg = if p.spp == 1 {
        turbojpeg::compress(
            turbojpeg::Image::<&[u8]> {
                pixels: &resized, width: p.out_tile_w as usize,
                pitch: p.out_tile_w as usize, height: p.out_tile_h as usize,
                format: turbojpeg::PixelFormat::GRAY,
            },
            p.quality as i32, turbojpeg::Subsamp::Gray,
        ).ok()?.to_vec()
    } else {
        turbojpeg::compress(
            turbojpeg::Image::<&[u8]> {
                pixels: &resized, width: p.out_tile_w as usize,
                pitch: p.out_tile_w as usize * 3, height: p.out_tile_h as usize,
                format: turbojpeg::PixelFormat::RGB,
            },
            p.quality as i32, turbojpeg::Subsamp::Sub2x2,
        ).ok()?.to_vec()
    };

    Some((out_id, jpeg))
}

fn compute_thread_body(
    raw_rx: mpsc::Receiver<RawChunk>,
    enc_tx: mpsc::SyncSender<EncChunk>,
    params: Arc<EncodeParams>,
) {
    for raw_chunk in raw_rx {
        let mut encoded: EncChunk = raw_chunk
            .par_iter()
            .filter_map(|(id, quads)| encode_one_tile(*id, quads, &params))
            .collect();
        encoded.sort_unstable_by_key(|(n, _)| *n);
        if enc_tx.send(encoded).is_err() { break; }
    }
}

unsafe fn write_enc_chunk(
    tiff: *mut TIFF,
    chunk: &EncChunk,
    jpegtables_registered: &mut bool,
) {
    for (id, jpeg) in chunk {
        let split = split_jpeg_to_tables_and_tile(jpeg);
        if !*jpegtables_registered {
            if let Some((ref tables, _)) = split {
                unsafe {
                    TIFFSetField(tiff, TIFFTAG_JPEGTABLES,
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

// ─── Entry point for unified slean binary ─────────────────────────────────────

pub(crate) fn process_files(
    paths: &[std::path::PathBuf],
    args: &crate::Args,
    mp: &MultiProgress,
) {
    if paths.is_empty() { return; }

    let bar_style = ProgressStyle::with_template(
        "  {spinner:.green} [{elapsed_precise}] {bar:40.cyan/blue} {pos}/{len} tiles  {msg}"
    ).unwrap().progress_chars("=>-");

    let total_files = paths.len();
    let skipped = AtomicUsize::new(0);
    let file_bar = mp.add(ProgressBar::new(total_files as u64));
    file_bar.set_style(
        ProgressStyle::with_template(
            "  [{elapsed_precise}] {bar:40.green/black} {pos}/{len} TIFF/SVS"
        ).unwrap().progress_chars("=>-")
    );

    for path in paths {
        let src_path = path.to_string_lossy().to_string();
        let src_name = path.file_name().unwrap_or_default()
            .to_string_lossy().to_string();
        let src_stem = path.file_stem().unwrap_or_default()
            .to_string_lossy().to_string();

        let already_exists = [
            format!("{}.tiff",     src_stem),
            format!("{}.ome.tiff", src_stem),
            format!("{}.svs",      src_stem),
        ].iter().any(|name| Path::new(&args.output_dir).join(name).exists());

        if already_exists {
            if args.verbose { vlog(None, format!("  [skip ] exists: {src_name}")); }
            skipped.fetch_add(1, Ordering::Relaxed);
            file_bar.inc(1);
            continue;
        }

        let pb = mp.add(ProgressBar::new(0));
        pb.set_style(bar_style.clone());
        pb.set_message(src_name.clone());

        process_file(&src_path, &args.output_dir, &src_stem, args, &pb);

        pb.finish_and_clear();
        file_bar.inc(1);
    }

    file_bar.finish_and_clear();

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
            let inject_app14 = spp == 3;
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
        let comps = j2k.components();
        if comps.is_empty() { return None; }
        let luma_w = comps[0].width() as usize;
        let luma_h = comps[0].height() as usize;
        if luma_w == 0 || luma_h == 0 { return None; }
        let mut pix: Vec<u8> = if spp == 1 || comps.len() < 3 {
            comps[0].data_u8().collect()
        } else {
            let y_u8:  Vec<u8> = comps[0].data_u8().collect();
            let cb_u8: Vec<u8> = comps[1].data_u8().collect();
            let cr_u8: Vec<u8> = comps[2].data_u8().collect();
            let cb_w = comps[1].width() as usize;
            let cb_h = comps[1].height() as usize;
            let cr_w = comps[2].width() as usize;
            let cr_h = comps[2].height() as usize;
            let mut buf = Vec::with_capacity(luma_w * luma_h * 3);
            for row in 0..luma_h {
                for col in 0..luma_w {
                    let y = y_u8[row*luma_w+col];
                    let cb_col = (col*cb_w/luma_w).min(cb_w.saturating_sub(1));
                    let cb_row = (row*cb_h/luma_h).min(cb_h.saturating_sub(1));
                    let cb = cb_u8[cb_row*cb_w+cb_col];
                    let cr_col = (col*cr_w/luma_w).min(cr_w.saturating_sub(1));
                    let cr_row = (row*cr_h/luma_h).min(cr_h.saturating_sub(1));
                    let cr = cr_u8[cr_row*cr_w+cr_col];
                    buf.extend_from_slice(&[y, cb, cr]);
                }
            }
            buf
        };
        let cs = j2k.color_space();
        if spp == 3 && (
            matches!(cs, jpeg2k::ColorSpace::SYCC)
            || (src_jp2k_is_ycbcr && !matches!(cs, jpeg2k::ColorSpace::SRGB))
        ) {
            for c in pix.chunks_mut(3) {
                let y  = c[0] as f32;
                let cb = c[1] as f32 - 128.0;
                let cr = c[2] as f32 - 128.0;
                c[0] = (y + 1.40200 * cr).clamp(0.0, 255.0) as u8;
                c[1] = (y - 0.34414*cb - 0.71414*cr).clamp(0.0, 255.0) as u8;
                c[2] = (y + 1.77200 * cb).clamp(0.0, 255.0) as u8;
            }
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
    src_levels: &[LevelDesc],
    pb:         &ProgressBar,
) {
    let out_path = if args.legacy {
        format!("{out_dir}/{out_stem}.tiff")
    } else {
        format!("{out_dir}/{out_stem}.ome.tiff")
    };
    let tmp_path = format!("{out_path}.tmp");

    let total_tiles: u64 = src_levels.iter().map(|lv| lv.n_tiles as u64).sum();
    pb.set_length(total_tiles);

    let ome   = !args.legacy;
    let base  = &src_levels[0];
    let out_spp: u32 = if base.spp >= 3 { 3 } else { 1 };
    let out_photometric = if out_spp == 1 { PHOTOMETRIC_MINISBLACK } else { PHOTOMETRIC_YCBCR };

    let file_stem = Path::new(src_path).file_stem()
        .unwrap_or_default().to_string_lossy().to_string();
    let image_desc_c: Option<CString> = if ome {
        let xml = generate_ome_xml(
            &file_stem,
            base.img_w, base.img_h,
            base.mpp_x, base.mpp_y,
            out_spp,
        );
        Some(CString::new(xml).unwrap())
    } else {
        None
    };

    let chunk_size = (rayon::current_num_threads() * 4).max(1);

    unsafe {
        let src_c   = CString::new(src_path).unwrap();
        let tmp_c   = CString::new(tmp_path.as_str()).unwrap();
        let r_mode  = CString::new("r").unwrap();
        let w8_mode = CString::new("w8").unwrap();

        let src_tiff = TIFFOpen(src_c.as_ptr(), r_mode.as_ptr());
        if src_tiff.is_null() {
            eprintln!("  [error] Cannot open: {src_path}");
            return;
        }
        let dst_tiff = TIFFOpen(tmp_c.as_ptr(), w8_mode.as_ptr());
        if dst_tiff.is_null() {
            eprintln!("  [error] Cannot create: {tmp_path}");
            TIFFClose(src_tiff);
            return;
        }

        let n_subifds = src_levels.len().saturating_sub(1);
        if ome && n_subifds > 0 {
            let zeros: Vec<u64> = vec![0u64; n_subifds];
            TIFFSetField(dst_tiff, TIFFTAG_SUBIFD, n_subifds as u32, zeros.as_ptr());
        }

        for (lv_idx, src_lv) in src_levels.iter().enumerate() {
            navigate_to_level(src_tiff, &src_lv.nav);

            let is_base  = lv_idx == 0;
            let subfile  = if is_base { 0u32 } else { FILETYPE_REDUCEDIMAGE };
            let out_tile_w = tile_align(src_lv.tile_w, 16);
            let out_tile_h = tile_align(src_lv.tile_h, 16);

            TIFFSetField(dst_tiff, TIFFTAG_SUBFILETYPE,      subfile);
            TIFFSetField(dst_tiff, TIFFTAG_IMAGEWIDTH,       src_lv.img_w);
            TIFFSetField(dst_tiff, TIFFTAG_IMAGELENGTH,      src_lv.img_h);
            TIFFSetField(dst_tiff, TIFFTAG_TILEWIDTH,        out_tile_w);
            TIFFSetField(dst_tiff, TIFFTAG_TILELENGTH,       out_tile_h);
            TIFFSetField(dst_tiff, TIFFTAG_COMPRESSION,      COMPRESSION_JPEG);
            TIFFSetField(dst_tiff, TIFFTAG_PHOTOMETRIC,      out_photometric);
            if out_photometric == PHOTOMETRIC_YCBCR {
                TIFFSetField(dst_tiff, TIFFTAG_YCBCRSUBSAMPLING, 2u32, 2u32);
            }
            TIFFSetField(dst_tiff, TIFFTAG_SAMPLESPERPIXEL,  out_spp);
            TIFFSetField(dst_tiff, TIFFTAG_BITSPERSAMPLE,    8u32);
            TIFFSetField(dst_tiff, TIFFTAG_SAMPLEFORMAT,     SAMPLEFORMAT_UINT as u32);
            TIFFSetField(dst_tiff, TIFFTAG_PLANARCONFIG,     PLANARCONFIG_CONTIG as u32);
            TIFFSetField(dst_tiff, TIFFTAG_ORIENTATION,      ORIENTATION_TOPLEFT as u32);
            TIFFSetField(dst_tiff, TIFFTAG_RESOLUTIONUNIT,   RESUNIT_CENTIMETER as u32);
            if src_lv.mpp_x > 0.0 {
                TIFFSetField(dst_tiff, TIFFTAG_XRESOLUTION,  1e4 / src_lv.mpp_x);
                TIFFSetField(dst_tiff, TIFFTAG_YRESOLUTION,  1e4 / src_lv.mpp_y);
            }
            if is_base {
                if let Some(ref desc) = image_desc_c {
                    TIFFSetField(dst_tiff, TIFFTAG_IMAGEDESCRIPTION, desc.as_ptr());
                }
                // ICC is baked in; do not embed the profile in the output
            }

            let src_is_jp2k   = is_jp2k(src_lv.compression as u32);
            let src_is_jpeg   = src_lv.compression as u32 == COMPRESSION_JPEG;
            let src_jp2k_is_ycbcr =
                src_is_jp2k && src_lv.compression as u32 == COMPRESSION_APERIO_JP2_YCBCR;

            let jpeg_tables_arc: Option<Arc<Vec<u8>>> = if src_is_jpeg {
                let mut tlen: u32 = 0;
                let mut tptr: *const u8 = std::ptr::null();
                let ok = TIFFGetField(src_tiff, TIFFTAG_JPEGTABLES,
                    &mut tlen as *mut u32, &mut tptr as *mut *const u8);
                if ok != 0 && !tptr.is_null() && tlen > 2 {
                    Some(Arc::new(std::slice::from_raw_parts(tptr, tlen as usize).to_vec()))
                } else { None }
            } else { None };

            let raw_buf_size = (TIFFTileSize(src_tiff) as usize)
                .max(src_lv.tile_w as usize * src_lv.tile_h as usize * src_lv.spp as usize)
                .max(1 << 17);
            let pix_size = src_lv.tile_w as usize * src_lv.tile_h as usize * src_lv.spp as usize;
            let tile_ids: Vec<u32> = (0..src_lv.n_tiles).collect();

            type BakeTile = (u32, Option<(Vec<u8>, bool, bool)>);

            let (raw_tx, raw_rx) = mpsc::sync_channel::<Vec<BakeTile>>(2);
            let (enc_tx, enc_rx) = mpsc::sync_channel::<EncChunk>(2);

            let xform_t       = Arc::clone(&icc_xform);
            let tables_t      = jpeg_tables_arc.clone();
            let quality       = args.quality;
            let spp           = out_spp;
            let src_tile_w    = src_lv.tile_w;
            let src_tile_h    = src_lv.tile_h;

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
                            let n = TIFFReadRawTile(src_tiff, tile_num,
                                buf.as_mut_ptr() as *mut c_void, buf.len() as i64);
                            if n > 0 {
                                buf.truncate(n as usize);
                                (tile_num, Some((buf, src_is_jpeg, src_is_jp2k)))
                            } else {
                                (tile_num, None)
                            }
                        } else {
                            let mut buf = vec![0u8; pix_size];
                            let n = TIFFReadEncodedTile(src_tiff, tile_num,
                                buf.as_mut_ptr() as *mut c_void, buf.len() as i64);
                            if n > 0 { (tile_num, Some((buf, false, false))) }
                            else { (tile_num, None) }
                        }
                    })
                    .collect();

                raw_tx.send(raw_chunk).expect("compute thread dropped");

                if let Some(prev) = pending_write.take() {
                    let n = prev.len() as u64;
                    write_enc_chunk(dst_tiff, &prev, &mut jpegtables_registered);
                    pb.inc(n);
                }
                pending_write = enc_rx.recv().ok();
            }

            drop(raw_tx);
            if let Some(last) = pending_write.take() {
                let n = last.len() as u64;
                write_enc_chunk(dst_tiff, &last, &mut jpegtables_registered);
                pb.inc(n);
            }
            for enc in enc_rx {
                let n = enc.len() as u64;
                write_enc_chunk(dst_tiff, &enc, &mut jpegtables_registered);
                pb.inc(n);
            }
            compute_handle.join().expect("compute thread panicked");

            TIFFWriteDirectory(dst_tiff);
        }

        TIFFClose(dst_tiff);
        TIFFClose(src_tiff);
    }

    if let Err(e) = std::fs::rename(&tmp_path, &out_path) {
        eprintln!("  [error] Failed to rename {tmp_path} → {out_path}: {e}");
        let _ = std::fs::remove_file(&tmp_path);
    }
}

// ─── JP2K SVS passthrough ─────────────────────────────────────────────────────

fn write_jp2k_svs_from_tiff(
    src_path: &str,
    levels: &[LevelDesc],
    dst_path: &str,
    verbose: bool,
    pb: &ProgressBar,
) {
    if levels.is_empty() { return; }
    let base = &levels[0];

    let img_desc = format!(
        "Aperio Image Library\n{}x{} ({} x {})\nMPP = {:.6}",
        base.img_w, base.img_h, base.tile_w, base.tile_h, base.mpp_x
    );

    let total_tiles: u64 = levels.iter().map(|lv| lv.n_tiles as u64).sum();
    pb.set_length(total_tiles);

    unsafe {
        let src_c = CString::new(src_path).unwrap();
        let dst_c = CString::new(dst_path).unwrap();
        let r_mode  = CString::new("r").unwrap();
        let w8_mode = CString::new("w8").unwrap();

        let src_tiff = TIFFOpen(src_c.as_ptr(), r_mode.as_ptr());
        if src_tiff.is_null() {
            eprintln!("  [error] Cannot open source for JP2K passthrough: {src_path}");
            return;
        }
        let dst_tiff = TIFFOpen(dst_c.as_ptr(), w8_mode.as_ptr());
        if dst_tiff.is_null() {
            eprintln!("  [error] Cannot create SVS: {dst_path}");
            TIFFClose(src_tiff);
            return;
        }

        for (idx, lv) in levels.iter().enumerate() {
            navigate_to_level(src_tiff, &lv.nav);

            let aperio_compr: u32 =
                if lv.photometric as u32 == PHOTOMETRIC_YCBCR { COMPRESSION_APERIO_JP2_YCBCR }
                else { COMPRESSION_APERIO_JP2_RGB };

            let subfile: u32 = if idx == 0 { 0 } else { FILETYPE_REDUCEDIMAGE };
            TIFFSetField(dst_tiff, TIFFTAG_SUBFILETYPE,     subfile);
            TIFFSetField(dst_tiff, TIFFTAG_IMAGEWIDTH,      lv.img_w);
            TIFFSetField(dst_tiff, TIFFTAG_IMAGELENGTH,     lv.img_h);
            TIFFSetField(dst_tiff, TIFFTAG_TILEWIDTH,       lv.tile_w);
            TIFFSetField(dst_tiff, TIFFTAG_TILELENGTH,      lv.tile_h);
            TIFFSetField(dst_tiff, TIFFTAG_COMPRESSION,     aperio_compr);
            TIFFSetField(dst_tiff, TIFFTAG_PHOTOMETRIC,     lv.photometric as u32);
            if lv.photometric as u32 == PHOTOMETRIC_YCBCR {
                TIFFSetField(dst_tiff, TIFFTAG_YCBCRSUBSAMPLING, 2u32, 2u32);
            }
            TIFFSetField(dst_tiff, TIFFTAG_SAMPLESPERPIXEL, lv.spp as u32);
            TIFFSetField(dst_tiff, TIFFTAG_BITSPERSAMPLE,   8u32);
            TIFFSetField(dst_tiff, TIFFTAG_SAMPLEFORMAT,    SAMPLEFORMAT_UINT as u32);
            TIFFSetField(dst_tiff, TIFFTAG_PLANARCONFIG,    PLANARCONFIG_CONTIG as u32);
            TIFFSetField(dst_tiff, TIFFTAG_ORIENTATION,     ORIENTATION_TOPLEFT as u32);
            TIFFSetField(dst_tiff, TIFFTAG_RESOLUTIONUNIT,  RESUNIT_CENTIMETER as u32);
            TIFFSetField(dst_tiff, TIFFTAG_XRESOLUTION,     1e4 / lv.mpp_x);
            TIFFSetField(dst_tiff, TIFFTAG_YRESOLUTION,     1e4 / lv.mpp_y);

            if idx == 0 {
                let desc_c = CString::new(img_desc.as_str()).unwrap();
                TIFFSetField(dst_tiff, TIFFTAG_IMAGEDESCRIPTION, desc_c.as_ptr());
            }

            if verbose {
                vlog(Some(pb), format!("  [pass ] lv{}  {}x{}  {:.4} µm/px  tile {}x{}  ({} tiles)",
                    idx, lv.img_w, lv.img_h, lv.mpp_x, lv.tile_w, lv.tile_h, lv.n_tiles));
            }

            let raw_buf_size = (TIFFTileSize(src_tiff) as usize).max(1 << 17);
            for tile_num in 0..lv.n_tiles {
                let mut buf = vec![0u8; raw_buf_size];
                let n = TIFFReadRawTile(src_tiff, tile_num,
                    buf.as_mut_ptr() as *mut c_void, buf.len() as i64);
                if n > 0 {
                    TIFFWriteRawTile(dst_tiff, tile_num,
                        buf.as_ptr() as *mut c_void, n);
                }
                pb.inc(1);
            }

            TIFFWriteDirectory(dst_tiff);
        }

        TIFFClose(dst_tiff);
        TIFFClose(src_tiff);
    }
}

// ─── Per-file processing ──────────────────────────────────────────────────────

fn process_file(src_path: &str, out_dir: &str, out_stem: &str, args: &crate::Args, pb: &ProgressBar) {
    let (mut src_levels, icc_profile) = {
        let src_c = CString::new(src_path).unwrap();
        let tiff = unsafe { TIFFOpen(src_c.as_ptr(), CString::new("r").unwrap().as_ptr()) };
        if tiff.is_null() {
            eprintln!("  [error] Cannot open: {src_path}");
            return;
        }
        let levels = collect_pyramid_levels(tiff);
        let icc    = read_icc_profile(tiff, &levels);
        unsafe { TIFFClose(tiff); }
        (levels, icc)
    };

    if src_levels.is_empty() {
        eprintln!("  [warn] No tiled pyramid found in: {src_path}");
        return;
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
        return;
    }

    // --icc-bake without --mpp/--half: pure 1:1 ICC bake
    if args.icc_bake && args.mpp.is_none() && !args.half {
        let icc = icc_profile.as_deref().unwrap(); // guaranteed Some by check above
        if let Some(xform) = crate::build_icc_transform(icc) {
            if args.verbose {
                vlog(Some(pb), format!("  [icc  ] baking {} bytes → sRGB", icc.len()));
            }
            process_file_icc_bake_only(src_path, out_dir, out_stem, args, xform, &src_levels, pb);
        } else {
            eprintln!("  [error] Invalid ICC profile in {src_path}; skipping.");
        }
        return;
    }

    let mpp_unknown = src_levels[0].mpp_x <= 0.0;
    if mpp_unknown && !args.half {
        eprintln!("  [error] Cannot determine resolution for {src_path}: \
            no XRESOLUTION tag and no 'MPP = <value>' in ImageDescription. Skipping.");
        return;
    }
    if args.half && mpp_unknown {
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

    let target_mpp = if args.half { base.mpp_x * 2.0 } else { args.mpp.unwrap() };
    let jp2k_svs_skip: Option<usize> = if !args.icc_bake && is_jp2k(base.compression as u32) {
        let skip = src_levels.iter()
            .take_while(|lv| lv.mpp_x < target_mpp * 0.9)
            .count();
        let has_match = src_levels.get(skip)
            .map(|lv| (lv.mpp_x - target_mpp).abs() / target_mpp < 0.1)
            .unwrap_or(false);
        if skip > 0 && has_match { Some(skip) } else { None }
    } else {
        None
    };

    let out_path = if jp2k_svs_skip.is_some() {
        format!("{out_dir}/{out_stem}.svs")
    } else if args.legacy {
        format!("{out_dir}/{out_stem}.tiff")
    } else {
        format!("{out_dir}/{out_stem}.ome.tiff")
    };
    let tmp_path = format!("{out_path}.tmp");

    if let Some(skip) = jp2k_svs_skip {
        write_jp2k_svs_from_tiff(src_path, &src_levels[skip..], &tmp_path, args.verbose, pb);
        std::fs::rename(&tmp_path, &out_path)
            .expect("Failed to rename tmp to output");
        return;
    }

    let mut output_levels = compute_output_levels(&src_levels, target_mpp, args.verbose, args.icc_bake);
    if mpp_unknown {
        for lv in output_levels.iter_mut() {
            lv.actual_mpp_x = 0.0;
            lv.actual_mpp_y = 0.0;
        }
    }
    if output_levels.is_empty() {
        eprintln!("  [warn] No output levels produced for {src_path}");
        return;
    }

    let total_tiles: u64 = output_levels.iter()
        .map(|lv| {
            if lv.passthrough {
                src_levels[lv.src_idx].n_tiles as u64
            } else {
                let out_ntx = (lv.out_img_w + lv.out_tile_w - 1) / lv.out_tile_w;
                let out_nty = (lv.out_img_h + lv.out_tile_h - 1) / lv.out_tile_h;
                (out_ntx * out_nty) as u64
            }
        })
        .sum();
    pb.set_length(total_tiles);

    let file_stem = Path::new(src_path).file_stem()
        .unwrap_or_default().to_string_lossy().to_string();
    let ome = !args.legacy;

    let base_lv = &output_levels[0];
    let image_desc_c: Option<CString> = if ome {
        let xml = generate_ome_xml(
            &file_stem,
            base_lv.out_img_w, base_lv.out_img_h,
            base_lv.actual_mpp_x, base_lv.actual_mpp_y,
            src_levels[base_lv.src_idx].spp as u32,
        );
        Some(CString::new(xml).unwrap())
    } else {
        None
    };

    let base_src = &src_levels[output_levels[0].src_idx];
    let out_spp: u32 = if base_src.spp >= 3 { 3 } else { 1 };
    let out_photometric = if out_spp == 1 { PHOTOMETRIC_MINISBLACK } else { PHOTOMETRIC_YCBCR };

    let fir_alg = match args.filter {
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
            format!("  [icc  ] baking → sRGB (resample+bake mode)")
        } else {
            "  [icc  ] transform build failed; skipping ICC bake".to_string()
        };
        vlog(Some(pb), &msg);
    }

    let src_c   = CString::new(src_path).unwrap();
    let tmp_c   = CString::new(tmp_path.as_str()).unwrap();
    let r_mode  = CString::new("r").unwrap();
    let w8_mode = CString::new("w8").unwrap();

    unsafe {
        let src_tiff = TIFFOpen(src_c.as_ptr(), r_mode.as_ptr());
        if src_tiff.is_null() {
            eprintln!("  [error] Cannot re-open: {src_path}");
            return;
        }
        let dst_tiff = TIFFOpen(tmp_c.as_ptr(), w8_mode.as_ptr());
        if dst_tiff.is_null() {
            eprintln!("  [error] Cannot create: {tmp_path}");
            TIFFClose(src_tiff);
            return;
        }

        let n_subifds = output_levels.len() - 1;
        if ome && n_subifds > 0 {
            let zeros: Vec<u64> = vec![0u64; n_subifds];
            TIFFSetField(dst_tiff, TIFFTAG_SUBIFD, n_subifds as u32, zeros.as_ptr());
        }

        let chunk_size = (rayon::current_num_threads() * 4).max(1);

        for (lv_idx, lv_out) in output_levels.iter().enumerate() {
            let src_lv  = &src_levels[lv_out.src_idx];
            let is_base = lv_idx == 0;
            let subfile = if is_base { 0u32 } else { FILETYPE_REDUCEDIMAGE };

            navigate_to_level(src_tiff, &src_lv.nav);

            let (src_subsamp_h, src_subsamp_v) = if src_lv.compression as u32 == COMPRESSION_JPEG
                && src_lv.photometric as u32 == PHOTOMETRIC_YCBCR
            {
                let mut sh: u16 = 2;
                let mut sv: u16 = 2;
                TIFFGetField(src_tiff, TIFFTAG_YCBCRSUBSAMPLING,
                    &mut sh as *mut u16, &mut sv as *mut u16);
                (sh, sv)
            } else {
                (2u16, 2u16)
            };

            TIFFSetField(dst_tiff, TIFFTAG_SUBFILETYPE,      subfile);
            TIFFSetField(dst_tiff, TIFFTAG_IMAGEWIDTH,       lv_out.out_img_w);
            TIFFSetField(dst_tiff, TIFFTAG_IMAGELENGTH,      lv_out.out_img_h);
            TIFFSetField(dst_tiff, TIFFTAG_TILEWIDTH,        tile_align(lv_out.out_tile_w, 16));
            TIFFSetField(dst_tiff, TIFFTAG_TILELENGTH,       tile_align(lv_out.out_tile_h, 16));
            TIFFSetField(dst_tiff, TIFFTAG_SAMPLESPERPIXEL,  out_spp);
            TIFFSetField(dst_tiff, TIFFTAG_BITSPERSAMPLE,    8u32);
            TIFFSetField(dst_tiff, TIFFTAG_SAMPLEFORMAT,     SAMPLEFORMAT_UINT as u32);
            TIFFSetField(dst_tiff, TIFFTAG_PLANARCONFIG,     PLANARCONFIG_CONTIG as u32);
            TIFFSetField(dst_tiff, TIFFTAG_ORIENTATION,      ORIENTATION_TOPLEFT as u32);
            TIFFSetField(dst_tiff, TIFFTAG_RESOLUTIONUNIT,   RESUNIT_CENTIMETER as u32);
            if lv_out.actual_mpp_x > 0.0 {
                TIFFSetField(dst_tiff, TIFFTAG_XRESOLUTION,  1e4 / lv_out.actual_mpp_x);
                TIFFSetField(dst_tiff, TIFFTAG_YRESOLUTION,  1e4 / lv_out.actual_mpp_y);
            }

            if lv_out.passthrough {
                TIFFSetField(dst_tiff, TIFFTAG_COMPRESSION,  src_lv.compression as u32);
                TIFFSetField(dst_tiff, TIFFTAG_PHOTOMETRIC,  src_lv.photometric as u32);
                if src_lv.photometric as u32 == PHOTOMETRIC_YCBCR {
                    TIFFSetField(dst_tiff, TIFFTAG_YCBCRSUBSAMPLING,
                        src_subsamp_h as u32, src_subsamp_v as u32);
                }
                if src_lv.compression as u32 == COMPRESSION_JPEG {
                    let mut tlen: u32 = 0;
                    let mut tptr: *const u8 = std::ptr::null();
                    let ok = TIFFGetField(src_tiff, TIFFTAG_JPEGTABLES,
                        &mut tlen as *mut u32,
                        &mut tptr as *mut *const u8);
                    if ok != 0 && !tptr.is_null() && tlen > 2 {
                        TIFFSetField(dst_tiff, TIFFTAG_JPEGTABLES, tlen, tptr);
                    }
                }
            } else {
                TIFFSetField(dst_tiff, TIFFTAG_COMPRESSION,  COMPRESSION_JPEG);
                TIFFSetField(dst_tiff, TIFFTAG_PHOTOMETRIC,  out_photometric);
                if out_photometric == PHOTOMETRIC_YCBCR {
                    TIFFSetField(dst_tiff, TIFFTAG_YCBCRSUBSAMPLING, 2u32, 2u32);
                }
            }

            if is_base {
                if let Some(ref desc) = image_desc_c {
                    TIFFSetField(dst_tiff, TIFFTAG_IMAGEDESCRIPTION, desc.as_ptr());
                }
                if !args.icc_bake {
                    if let Some(ref icc) = icc_profile {
                        TIFFSetField(dst_tiff, TIFFTAG_ICCPROFILE,
                            icc.len() as u32, icc.as_ptr() as *const c_void);
                    }
                }
            }

            let raw_buf_size = (TIFFTileSize(src_tiff) as usize)
                .max(src_lv.tile_w as usize * src_lv.tile_h as usize * src_lv.spp as usize)
                .max(1 << 17);

            let n_tiles  = src_lv.n_tiles;
            let tile_ids: Vec<u32> = (0..n_tiles).collect();

            let pix_size    = src_lv.tile_w as usize
                * src_lv.tile_h as usize
                * src_lv.spp as usize;
            let src_is_jp2k   = is_jp2k(src_lv.compression as u32);
            let src_is_jpeg   = src_lv.compression as u32 == COMPRESSION_JPEG;
            let src_jp2k_is_ycbcr =
                src_is_jp2k && src_lv.compression as u32 == COMPRESSION_APERIO_JP2_YCBCR;

            let n_reduce: u32 = if src_is_jp2k && !lv_out.passthrough {
                let nat_otw = (lv_out.out_tile_w / 2).max(1);
                let nat_oth = (lv_out.out_tile_h / 2).max(1);
                let scale_down = (src_lv.tile_w as f64 / nat_otw as f64)
                    .min(src_lv.tile_h as f64 / nat_oth as f64);
                if scale_down > 1.0 { scale_down.log2().floor() as u32 } else { 0 }
            } else {
                0
            };

            let jpeg_tables_arc: Option<Arc<Vec<u8>>> = if src_is_jpeg && !lv_out.passthrough {
                let mut tlen: u32 = 0;
                let mut tptr: *const u8 = std::ptr::null();
                let ok = TIFFGetField(src_tiff, TIFFTAG_JPEGTABLES,
                    &mut tlen as *mut u32,
                    &mut tptr as *mut *const u8);
                if ok != 0 && !tptr.is_null() && tlen > 2 {
                    Some(Arc::new(std::slice::from_raw_parts(tptr, tlen as usize).to_vec()))
                } else {
                    None
                }
            } else {
                None
            };

            if lv_out.passthrough {
                for chunk in tile_ids.chunks(chunk_size) {
                    let raw_chunk: Vec<(u32, Vec<u8>)> = chunk.iter()
                        .map(|&tile_num| {
                            let mut buf = vec![0u8; raw_buf_size];
                            let n = TIFFReadRawTile(src_tiff, tile_num,
                                buf.as_mut_ptr() as *mut c_void, buf.len() as i64);
                            if n > 0 { buf.truncate(n as usize); (tile_num, buf) }
                            else { (tile_num, Vec::new()) }
                        })
                        .collect();
                    for (tile_num, data) in raw_chunk {
                        if !data.is_empty() {
                            TIFFWriteRawTile(dst_tiff, tile_num,
                                data.as_ptr() as *mut c_void, data.len() as i64);
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
                    half:              args.half,
                    jpeg_tables:       jpeg_tables_arc.clone(),
                    icc_transform:     icc_transform_arc.clone(),
                });

                let (raw_tx, raw_rx) = mpsc::sync_channel::<RawChunk>(2);
                let (enc_tx, enc_rx) = mpsc::sync_channel::<EncChunk>(2);
                let params_t = Arc::clone(&enc_params);
                let compute_handle = std::thread::spawn(move || {
                    compute_thread_body(raw_rx, enc_tx, params_t);
                });

                let mut jpegtables_registered = false;
                let mut pending_write: Option<EncChunk> = None;

                for chunk in out_tile_ids.chunks(chunk_size) {
                    let raw_chunk: RawChunk = chunk.iter()
                        .map(|&out_id| {
                            let oc  = out_id % out_ntx;
                            let or_ = out_id / out_ntx;
                            let mut quads: RawQuad = [None, None, None, None];
                            for qi in 0..4usize {
                                let dc = (qi % 2) as u32;
                                let dr = (qi / 2) as u32;
                                let sc = 2 * oc + dc;
                                let sr = 2 * or_ + dr;
                                if sc >= src_ntx || sr >= src_nty { continue; }
                                let tile_num = sr * src_ntx + sc;
                                if src_is_jp2k {
                                    let mut buf = vec![0u8; raw_buf_size];
                                    let n = TIFFReadRawTile(src_tiff, tile_num,
                                        buf.as_mut_ptr() as *mut c_void, buf.len() as i64);
                                    if n > 0 {
                                        buf.truncate(n as usize);
                                        quads[qi] = Some((buf, true));
                                    }
                                } else if src_is_jpeg {
                                    let mut raw_buf = vec![0u8; raw_buf_size];
                                    let raw_n = TIFFReadRawTile(src_tiff, tile_num,
                                        raw_buf.as_mut_ptr() as *mut c_void,
                                        raw_buf.len() as i64);
                                    if raw_n > 2
                                        && raw_buf[0] == 0xFF && raw_buf[1] == 0xD8
                                    {
                                        raw_buf.truncate(raw_n as usize);
                                        quads[qi] = Some((raw_buf, true));
                                    } else {
                                        let mut pix_buf = vec![0u8; pix_size];
                                        let n = TIFFReadEncodedTile(src_tiff, tile_num,
                                            pix_buf.as_mut_ptr() as *mut c_void,
                                            pix_buf.len() as i64);
                                        if n > 0 { quads[qi] = Some((pix_buf, false)); }
                                    }
                                } else {
                                    let mut buf = vec![0u8; pix_size];
                                    let n = TIFFReadEncodedTile(src_tiff, tile_num,
                                        buf.as_mut_ptr() as *mut c_void, buf.len() as i64);
                                    if n > 0 { quads[qi] = Some((buf, false)); }
                                }
                            }
                            (out_id, quads)
                        })
                        .collect();

                    raw_tx.send(raw_chunk).expect("compute thread dropped");

                    if let Some(prev) = pending_write.take() {
                        let n = prev.len() as u64;
                        write_enc_chunk(dst_tiff, &prev, &mut jpegtables_registered);
                        pb.inc(n);
                    }

                    pending_write = enc_rx.recv().ok();
                }

                drop(raw_tx);

                if let Some(last) = pending_write.take() {
                    let n = last.len() as u64;
                    write_enc_chunk(dst_tiff, &last, &mut jpegtables_registered);
                    pb.inc(n);
                }
                for enc in enc_rx {
                    let n = enc.len() as u64;
                    write_enc_chunk(dst_tiff, &enc, &mut jpegtables_registered);
                    pb.inc(n);
                }
                compute_handle.join().expect("compute thread panicked");
            }

            TIFFWriteDirectory(dst_tiff);
        }

        TIFFClose(dst_tiff);
        TIFFClose(src_tiff);
    }

    if let Err(e) = std::fs::rename(&tmp_path, &out_path) {
        eprintln!("  [error] Failed to rename {tmp_path} → {out_path}: {e}");
        let _ = std::fs::remove_file(&tmp_path);
    }
}

// ─── Source TIFF navigation ───────────────────────────────────────────────────

fn navigate_to_level(tiff: *mut TIFF, nav: &LevelNav) {
    unsafe {
        match nav {
            LevelNav::Dir(d)    => { TIFFSetDirectory(tiff, *d); }
            LevelNav::SubDir(o) => { TIFFSetSubDirectory(tiff, *o); }
        }
    }
}

fn collect_pyramid_levels(tiff: *mut TIFF) -> Vec<LevelDesc> {
    let mut levels = Vec::new();

    unsafe { TIFFSetDirectory(tiff, 0); }
    let Some(mut lv0) = read_level_meta(tiff) else { return levels; };

    let mut n_sub: u16 = 0;
    let mut sub_ptr: *const u64 = std::ptr::null();
    let has_subifds = unsafe {
        TIFFGetField(tiff, TIFFTAG_SUBIFD,
            &mut n_sub as *mut u16,
            &mut sub_ptr as *mut *const u64) != 0
    } && n_sub > 0 && !sub_ptr.is_null();

    if lv0.mpp_x <= 0.0 {
        if let Some(mpp) = parse_mpp_from_image_description(tiff) {
            lv0.mpp_x = mpp;
            lv0.mpp_y = mpp;
        }
    }

    lv0.nav = LevelNav::Dir(0);
    levels.push(lv0);

    if has_subifds {
        let offsets = unsafe { std::slice::from_raw_parts(sub_ptr, n_sub as usize) };
        for &off in offsets {
            if off == 0 { continue; }
            if unsafe { TIFFSetSubDirectory(tiff, off) } != 0 {
                if let Some(mut lv) = read_level_meta(tiff) {
                    lv.nav = LevelNav::SubDir(off);
                    levels.push(lv);
                }
                unsafe { TIFFSetDirectory(tiff, 0); }
            }
        }
    } else {
        let n_dirs = unsafe { TIFFNumberOfDirectories(tiff) };
        for dir_idx in 1..n_dirs {
            unsafe { TIFFSetDirectory(tiff, dir_idx); }
            if let Some(mut lv) = read_level_meta(tiff) {
                lv.nav = LevelNav::Dir(dir_idx);
                levels.push(lv);
            }
        }
        unsafe { TIFFSetDirectory(tiff, 0); }
    }

    levels.sort_by(|a, b| b.img_w.cmp(&a.img_w));

    if levels.len() > 1 && levels[0].img_w > 0 {
        let bw  = levels[0].img_w as f64;
        let bh  = levels[0].img_h as f64;
        let bmx = levels[0].mpp_x;
        let bmy = levels[0].mpp_y;
        for lv in levels.iter_mut().skip(1) {
            if lv.img_w > 0 { lv.mpp_x = bmx * bw / lv.img_w as f64; }
            if lv.img_h > 0 { lv.mpp_y = bmy * bh / lv.img_h as f64; }
        }
    }

    levels.sort_by(|a, b| a.mpp_x.partial_cmp(&b.mpp_x).unwrap());
    levels
}

fn read_level_meta(tiff: *mut TIFF) -> Option<LevelDesc> {
    let mut width: u32 = 0;
    let mut height: u32 = 0;
    let mut tile_w: u32 = 0;
    let mut tile_h: u32 = 0;
    let mut compression: u16 = 1;
    let mut photometric: u16 = 2;
    let mut spp: u16 = 3;
    let mut xres: f32 = 0.0;
    let mut yres: f32 = 0.0;
    let mut resunit: u16 = RESUNIT_CENTIMETER as u16;

    unsafe {
        if TIFFGetField(tiff, TIFFTAG_IMAGEWIDTH,  &mut width  as *mut u32) == 0 { return None; }
        if TIFFGetField(tiff, TIFFTAG_IMAGELENGTH, &mut height as *mut u32) == 0 { return None; }
        if TIFFGetField(tiff, TIFFTAG_TILEWIDTH,   &mut tile_w as *mut u32) == 0 { return None; }
        if TIFFGetField(tiff, TIFFTAG_TILELENGTH,  &mut tile_h as *mut u32) == 0 { return None; }
        if tile_w == 0 || tile_h == 0 { return None; }

        TIFFGetField(tiff, TIFFTAG_COMPRESSION,     &mut compression as *mut u16);
        TIFFGetField(tiff, TIFFTAG_PHOTOMETRIC,     &mut photometric as *mut u16);
        TIFFGetField(tiff, TIFFTAG_SAMPLESPERPIXEL, &mut spp        as *mut u16);
        TIFFGetField(tiff, TIFFTAG_XRESOLUTION,     &mut xres       as *mut f32);
        TIFFGetField(tiff, TIFFTAG_YRESOLUTION,     &mut yres       as *mut f32);
        TIFFGetField(tiff, TIFFTAG_RESOLUTIONUNIT,  &mut resunit    as *mut u16);
    }

    let (mpp_x, mpp_y) = mpp_from_resolution(xres as f64, yres as f64, resunit as u32);
    let n_tiles = unsafe { TIFFNumberOfTiles(tiff) };

    Some(LevelDesc {
        img_w: width, img_h: height,
        tile_w, tile_h,
        mpp_x, mpp_y,
        compression, photometric, spp,
        n_tiles,
        nav: LevelNav::Dir(0),
    })
}

fn mpp_from_resolution(xres: f64, yres: f64, resunit: u32) -> (f64, f64) {
    if xres <= 0.0 || yres <= 0.0 { return (0.0, 0.0); }
    if resunit == RESUNIT_CENTIMETER {
        (10000.0 / xres, 10000.0 / yres)
    } else if resunit == RESUNIT_INCH {
        (25400.0 / xres, 25400.0 / yres)
    } else {
        (0.0, 0.0)
    }
}

fn parse_mpp_from_image_description(tiff: *mut TIFF) -> Option<f64> {
    let mut desc_ptr: *const std::os::raw::c_char = std::ptr::null();
    let ok = unsafe {
        TIFFGetField(tiff, TIFFTAG_IMAGEDESCRIPTION,
            &mut desc_ptr as *mut *const std::os::raw::c_char)
    };
    if ok == 0 || desc_ptr.is_null() { return None; }
    let desc = unsafe { std::ffi::CStr::from_ptr(desc_ptr) }.to_string_lossy();
    for part in desc.split('|') {
        let part = part.trim();
        if let Some(val_str) = part.strip_prefix("MPP = ") {
            if let Ok(mpp) = val_str.trim().parse::<f64>() {
                if mpp > 0.0 { return Some(mpp); }
            }
        }
    }
    None
}

fn read_icc_profile(tiff: *mut TIFF, levels: &[LevelDesc]) -> Option<Vec<u8>> {
    if levels.is_empty() { return None; }
    navigate_to_level(tiff, &levels[0].nav);
    let mut icc_len: u32 = 0;
    let mut icc_ptr: *const u8 = std::ptr::null();
    let ok = unsafe {
        TIFFGetField(tiff, TIFFTAG_ICCPROFILE,
            &mut icc_len as *mut u32,
            &mut icc_ptr as *mut *const u8) != 0
    };
    if ok && icc_len > 0 && !icc_ptr.is_null() {
        Some(unsafe { std::slice::from_raw_parts(icc_ptr, icc_len as usize) }.to_vec())
    } else {
        None
    }
}

// ─── Output pyramid computation ───────────────────────────────────────────────

fn compute_output_levels(
    src_levels: &[LevelDesc],
    target_mpp: f64,
    verbose: bool,
    icc_bake: bool,
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

// ─── OME-XML generation ────────────────────────────────────────────────────────

fn generate_ome_xml(name: &str, width: u32, height: u32, mpp_x: f64, mpp_y: f64, spp: u32) -> String {
    let safe_name = xml_escape(name);
    let type_str  = "uint8";
    let size_c    = if spp == 1 { 1 } else { spp };

    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<OME xmlns="http://www.openmicroscopy.org/Schemas/OME/2016-06" xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance" xsi:schemaLocation="http://www.openmicroscopy.org/Schemas/OME/2016-06 http://www.openmicroscopy.org/Schemas/OME/2016-06/ome.xsd">
  <Image ID="Image:0" Name="{safe_name}">
    <Pixels ID="Pixels:0" DimensionOrder="XYZCT" Type="{type_str}" SizeX="{width}" SizeY="{height}" SizeZ="1" SizeC="{size_c}" SizeT="1" PhysicalSizeX="{mpp_x:.6}" PhysicalSizeXUnit="µm" PhysicalSizeY="{mpp_y:.6}" PhysicalSizeYUnit="µm">
      <Channel ID="Channel:0:0" SamplesPerPixel="{spp}"/>
      <TiffData/>
    </Pixels>
  </Image>
</OME>"#
    )
}
