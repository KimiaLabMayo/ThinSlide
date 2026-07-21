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
    TIFFOpen, TIFFClose,
    TIFFGetField,
    TIFFSetField,
    TIFFReadRawTile, TIFFReadEncodedTile,
    TIFFWriteRawTile, TIFFWriteDirectory,
    TIFFTileSize,
    TIFFTAG_IMAGEDESCRIPTION, TIFFTAG_ICCPROFILE,
    TIFFTAG_JPEGTABLES, TIFFTAG_YCBCRSUBSAMPLING, TIFFTAG_SUBIFD,
    COMPRESSION_JPEG,
    PHOTOMETRIC_RGB, PHOTOMETRIC_YCBCR, PHOTOMETRIC_MINISBLACK,
    FILETYPE_REDUCEDIMAGE,
};
use crate::{tile_align, nearest_16, MIN_PYRAMID_SIDE,
            vlog, write_enc_chunk, compute_thread, set_tiff_ifd_tags};
use crate::source::tiff::{
    TiffSource, TiffLevel, navigate,
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

            let scaling = match p.decode_shift {
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
        } else if *is_raw_decode {
            // JP2K
            let params = jpeg2k::DecodeParameters::default().reduce(p.n_reduce);
            let img = jpeg2k::Image::from_bytes_with(data, params).ok()?;
            let (mut pix, luma_w, luma_h) = super::jp2k_assemble_pixels(&img, p.spp as usize)?;
            let color_space = img.color_space();
            let needs_ycbcr_cvt = p.spp == 3 && (
                matches!(color_space, jpeg2k::ColorSpace::SYCC)
                || (p.src_jp2k_is_ycbcr && !matches!(color_space, jpeg2k::ColorSpace::SRGB))
            );
            if needs_ycbcr_cvt {
                super::ycbcr_to_rgb(&mut pix);
            }
            Some((pix, luma_w as u32, luma_h as u32))
        } else {
            Some((data.clone(), p.src_tile_w, p.src_tile_h))
        }
    });

    crate::compose_and_encode(out_id, decoded, ch, p.out_tile_w, p.out_tile_h,
        p.icc_transform.as_deref(), p.fpt, &p.resize_opts, p.quality, p.spp)
}

// ─── Entry point for unified thinslide binary ─────────────────────────────────

pub(crate) fn process_files(
    paths: &[std::path::PathBuf],
    args: &crate::Args,
    mp: &MultiProgress,
    stats: &crate::logger::ConversionStats,
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
            file_bar.inc(1);
            continue;
        }

        let pb = mp.add(ProgressBar::new(0));
        pb.set_style(bar_style.clone());
        pb.set_message(src_name.clone());

        process_file(&src_path, &args.output_dir, &src_stem, args, &pb);

        // process_file has no return value; infer success from output presence.
        let produced: Option<std::path::PathBuf> = candidates.iter()
            .map(|name| Path::new(&args.output_dir).join(name))
            .find(|p| p.exists());
        if let Some(out) = produced {
            let in_b  = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
            let out_b = std::fs::metadata(&out).map(|m| m.len()).unwrap_or(0);
            stats.ok.fetch_add(1, Ordering::Relaxed);
            stats.in_bytes.fetch_add(in_b, Ordering::Relaxed);
            stats.out_bytes.fetch_add(out_b, Ordering::Relaxed);
        } else {
            stats.fail.fetch_add(1, Ordering::Relaxed);
        }

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
    ome_xml:    Option<&str>,
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

    let image_desc_c: Option<CString> = if ome {
        let xml = if let Some(orig) = ome_xml {
            crate::pipeline::ome::update_ome_xml_for_output(
                orig, base.img_w, base.img_h, base.mpp_x, base.mpp_y,
            )
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
    if ome && n_subifds > 0 {
        let zeros: Vec<u64> = vec![0u64; n_subifds];
        unsafe { TIFFSetField(dst_tiff, TIFFTAG_SUBIFD, n_subifds as u32, zeros.as_ptr()); }
    }

    for (lv_idx, src_lv) in src_levels.iter().enumerate() {
        unsafe { navigate(src_tiff, lv_idx, src_levels); }

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
            if is_base {
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
            let mut tlen: u32 = 0;
            let mut tptr: *const u8 = std::ptr::null();
            let ok = unsafe { TIFFGetField(src_tiff, TIFFTAG_JPEGTABLES,
                &mut tlen as *mut u32, &mut tptr as *mut *const u8) };
            if ok != 0 && !tptr.is_null() && tlen > 2 {
                Some(Arc::new(unsafe { std::slice::from_raw_parts(tptr, tlen as usize) }.to_vec()))
            } else { None }
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

    unsafe { TIFFClose(dst_tiff); }
    unsafe { TIFFClose(src_tiff); }

    if let Err(e) = std::fs::rename(&tmp_path, &out_path) {
        eprintln!("  [error] Failed to rename {tmp_path} → {out_path}: {e}");
        let _ = std::fs::remove_file(&tmp_path);
    }
}

// ─── JP2K SVS passthrough ─────────────────────────────────────────────────────

fn write_jp2k_svs_from_tiff(
    src_path: &str,
    levels: &[TiffLevel],
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

    for (idx, lv) in levels.iter().enumerate() {
        unsafe { navigate(src_tiff, idx, levels); }

        let aperio_compr: u32 =
            if lv.photometric as u32 == PHOTOMETRIC_YCBCR { COMPRESSION_APERIO_JP2_YCBCR }
            else { crate::source::tiff::COMPRESSION_APERIO_JP2_RGB };

        let subfile: u32 = if idx == 0 { 0 } else { FILETYPE_REDUCEDIMAGE };
        unsafe {
            set_tiff_ifd_tags(dst_tiff, subfile,
                lv.img_w, lv.img_h, lv.tile_w, lv.tile_h,
                aperio_compr, lv.photometric as u32, lv.spp as u32,
                lv.mpp_x, lv.mpp_y);
            if lv.photometric as u32 == PHOTOMETRIC_YCBCR {
                TIFFSetField(dst_tiff, TIFFTAG_YCBCRSUBSAMPLING, 2u32, 2u32);
            }
        }

        if idx == 0 {
            let desc_c = CString::new(img_desc.as_str()).unwrap();
            unsafe { TIFFSetField(dst_tiff, TIFFTAG_IMAGEDESCRIPTION, desc_c.as_ptr()); }
        }

        if verbose {
            vlog(Some(pb), format!("  [pass ] lv{}  {}x{}  {:.4} µm/px  tile {}x{}  ({} tiles)",
                idx, lv.img_w, lv.img_h, lv.mpp_x, lv.tile_w, lv.tile_h, lv.n_tiles));
        }

        let raw_buf_size = (unsafe { TIFFTileSize(src_tiff) } as usize).max(1 << 17);
        for tile_num in 0..lv.n_tiles {
            let mut buf = vec![0u8; raw_buf_size];
            let n = unsafe { TIFFReadRawTile(src_tiff, tile_num,
                buf.as_mut_ptr() as *mut c_void, buf.len() as i64) };
            if n > 0 {
                unsafe { TIFFWriteRawTile(dst_tiff, tile_num,
                    buf.as_ptr() as *mut c_void, n); }
            }
            pb.inc(1);
        }

        unsafe { TIFFWriteDirectory(dst_tiff); }
    }

    unsafe { TIFFClose(dst_tiff); }
    unsafe { TIFFClose(src_tiff); }
}

// ─── Per-file processing ──────────────────────────────────────────────────────

fn process_file(src_path: &str, out_dir: &str, out_stem: &str, args: &crate::Args, pb: &ProgressBar) {
    let Some(src) = TiffSource::open(src_path) else {
        eprintln!("  [error] Cannot open: {src_path}");
        return;
    };
    let (mut src_levels, icc_profile, ome_xml, _meta) = src.into_parts();

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

    // --half / --quarter / --20x: classify the source magnification bucket;
    // skip (--20x only) when the MPP is unknown or the source is coarser than
    // 20x (upscaling not supported). --half/--quarter always halve/quarter,
    // regardless of source MPP.
    let mag_factor: u32 = if args.quarter {
        4
    } else if args.half {
        2
    } else if args.mag_20x {
        match crate::factor_to_20x(src_levels[0].mpp_x) {
            Some(f) => f,
            None => {
                eprintln!("  [skip ] {src_path}: source MPP unknown or ≥0.7 µm/px (--20x cannot upscale)");
                return;
            }
        }
    } else {
        1
    };

    // --20x at native 20x, no color conversion requested: copy through as-is.
    if args.mag_20x && mag_factor == 1 && !args.icc_bake {
        let src_name = Path::new(src_path).file_name()
            .unwrap_or_default().to_string_lossy().to_string();
        let dst = std::path::PathBuf::from(out_dir).join(&src_name);
        if let Err(e) = std::fs::copy(src_path, &dst) {
            eprintln!("  [error] Copy failed for {src_name}: {e}");
        }
        return;
    }

    // Pure 1:1 ICC bake: plain --icc-bake, or --20x already at native 20x.
    if args.icc_bake && args.mpp.is_none() && !args.half && !args.quarter && (!args.mag_20x || mag_factor == 1) {
        let icc = icc_profile.as_deref().unwrap();
        if let Some(xform) = crate::build_icc_transform(icc) {
            if args.verbose {
                vlog(Some(pb), format!("  [icc  ] baking {} bytes → sRGB", icc.len()));
            }
            process_file_icc_bake_only(src_path, out_dir, out_stem, args, xform, &src_levels, ome_xml.as_deref(), pb);
        } else {
            eprintln!("  [error] Invalid ICC profile in {src_path}; skipping.");
        }
        return;
    }

    // --half/--quarter with unknown source MPP: derive a synthetic 1.0 µm/px
    // base so downstream MPP-based level selection still works, but remember
    // to blank the resolution tags on output (see mpp_unknown below).
    let mpp_unknown = src_levels[0].mpp_x <= 0.0;
    if (args.half || args.quarter) && mpp_unknown {
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

    if !args.mag_20x && !args.half && !args.quarter {
        if let Some(t) = args.mpp {
            if base.mpp_x <= 0.0 {
                eprintln!("  [error] Cannot determine resolution for {src_path}: \
                    no XRESOLUTION tag and no 'MPP = <value>' in ImageDescription. Skipping.");
                return;
            }
            if t <= base.mpp_x {
                eprintln!(
                    "  [warn ] requested MPP {:.4} µm/px ≤ source {:.4} µm/px (upscaling not supported); {}",
                    t, base.mpp_x,
                    if args.icc_bake { "applying ICC bake at 1:1" } else { "skipping" }
                );
                if args.icc_bake {
                    let icc = icc_profile.as_deref().unwrap();
                    if let Some(xform) = crate::build_icc_transform(icc) {
                        process_file_icc_bake_only(src_path, out_dir, out_stem, args, xform, &src_levels, ome_xml.as_deref(), pb);
                    } else {
                        eprintln!("  [error] Invalid ICC profile in {src_path}; skipping.");
                    }
                }
                return;
            }
        }
    }

    let decode_shift: u32 = if args.mag_20x || args.half || args.quarter { mag_factor.trailing_zeros() } else { 0 };
    let target_mpp = if args.mag_20x || args.half || args.quarter { base.mpp_x * mag_factor as f64 } else { args.mpp.unwrap() };
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

    let mut output_levels = compute_output_levels(&src_levels, target_mpp, args.verbose, args.icc_bake, decode_shift);
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

    let ome = !args.legacy;

    let base_lv = &output_levels[0];
    let image_desc_c: Option<CString> = if ome {
        let xml = if let Some(ref orig) = ome_xml {
            crate::pipeline::ome::update_ome_xml_for_output(
                orig,
                base_lv.out_img_w, base_lv.out_img_h,
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
        return;
    }
    let dst_tiff = unsafe { TIFFOpen(tmp_c.as_ptr(), w8_mode.as_ptr()) };
    if dst_tiff.is_null() {
        eprintln!("  [error] Cannot create: {tmp_path}");
        unsafe { TIFFClose(src_tiff); }
        return;
    }

    let n_subifds = output_levels.len() - 1;
    if ome && n_subifds > 0 {
        let zeros: Vec<u64> = vec![0u64; n_subifds];
        unsafe { TIFFSetField(dst_tiff, TIFFTAG_SUBIFD, n_subifds as u32, zeros.as_ptr()); }
    }

    let chunk_size = (rayon::current_num_threads() * 4).max(1);

    for (lv_idx, lv_out) in output_levels.iter().enumerate() {
        let src_lv  = &src_levels[lv_out.src_idx];
        let is_base = lv_idx == 0;
        let subfile = if is_base { 0u32 } else { FILETYPE_REDUCEDIMAGE };

        unsafe { navigate(src_tiff, lv_out.src_idx, &src_levels); }

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
                let mut tlen: u32 = 0;
                let mut tptr: *const u8 = std::ptr::null();
                let ok = unsafe { TIFFGetField(src_tiff, TIFFTAG_JPEGTABLES,
                    &mut tlen as *mut u32,
                    &mut tptr as *mut *const u8) };
                if ok != 0 && !tptr.is_null() && tlen > 2 {
                    unsafe { TIFFSetField(dst_tiff, TIFFTAG_JPEGTABLES, tlen, tptr); }
                }
            }
        } else if out_photometric == PHOTOMETRIC_YCBCR {
            unsafe { TIFFSetField(dst_tiff, TIFFTAG_YCBCRSUBSAMPLING, 2u32, 2u32); }
        }

        if is_base {
            if let Some(ref desc) = image_desc_c {
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
            let mut tlen: u32 = 0;
            let mut tptr: *const u8 = std::ptr::null();
            let ok = unsafe { TIFFGetField(src_tiff, TIFFTAG_JPEGTABLES,
                &mut tlen as *mut u32,
                &mut tptr as *mut *const u8) };
            if ok != 0 && !tptr.is_null() && tlen > 2 {
                Some(Arc::new(unsafe { std::slice::from_raw_parts(tptr, tlen as usize) }.to_vec()))
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
                        let n = unsafe { TIFFReadRawTile(src_tiff, tile_num,
                            buf.as_mut_ptr() as *mut c_void, buf.len() as i64) };
                        if n > 0 { buf.truncate(n as usize); (tile_num, buf) }
                        else { (tile_num, Vec::new()) }
                    })
                    .collect();
                for (tile_num, data) in raw_chunk {
                    if !data.is_empty() {
                        unsafe { TIFFWriteRawTile(dst_tiff, tile_num,
                            data.as_ptr() as *mut c_void, data.len() as i64); }
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
                decode_shift,
                jpeg_tables:       jpeg_tables_arc.clone(),
                icc_transform:     icc_transform_arc.clone(),
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
        }

        unsafe { TIFFWriteDirectory(dst_tiff); }
    }

    unsafe { TIFFClose(dst_tiff); }
    unsafe { TIFFClose(src_tiff); }

    if let Err(e) = std::fs::rename(&tmp_path, &out_path) {
        eprintln!("  [error] Failed to rename {tmp_path} → {out_path}: {e}");
        let _ = std::fs::remove_file(&tmp_path);
    }
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

