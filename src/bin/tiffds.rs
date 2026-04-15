// tiffds: TIFF/SVS downsampling tool
// Reads every .tiff / .svs file under an input directory and writes
// a pyramidal OME-TIFF (default) or flat pyramidal BigTIFF (--legacy)
// resampled to the requested --mpp resolution.

use std::ffi::CString;
use std::os::raw::c_void;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use rayon::prelude::*;
use walkdir::WalkDir;
use image::imageops::FilterType;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use fast_image_resize as fir;
use jpeg2k;

use wsi_tools::bindings::{
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
    TIFFTAG_YCBCRSUBSAMPLING, TIFFTAG_SUBIFD,
    TIFFTAG_XRESOLUTION, TIFFTAG_YRESOLUTION, TIFFTAG_RESOLUTIONUNIT,
    COMPRESSION_JPEG,
    PHOTOMETRIC_YCBCR, PHOTOMETRIC_MINISBLACK,
    RESUNIT_CENTIMETER, RESUNIT_INCH,
    SAMPLEFORMAT_UINT, PLANARCONFIG_CONTIG, ORIENTATION_TOPLEFT,
    FILETYPE_REDUCEDIMAGE,
};
use wsi_tools::{tile_align, nearest_16, MIN_PYRAMID_SIDE, xml_escape};

const COMPRESSION_APERIO_JP2_YCBCR: u32 = 33003;
const COMPRESSION_APERIO_JP2_RGB: u32   = 33005;
const COMPRESSION_JP2000: u32           = 34712;

fn is_jp2k(c: u32) -> bool {
    matches!(c, COMPRESSION_APERIO_JP2_YCBCR | COMPRESSION_APERIO_JP2_RGB | COMPRESSION_JP2000)
}

// ─── Args ─────────────────────────────────────────────────────────────────────

struct Args {
    input_dir:  String,
    output_dir: String,
    legacy:     bool,
    mpp:        f64,
    quality:    u8,
    filter:     FilterType,
    verbose:    bool,
    jobs:       Option<usize>,
}

impl Args {
    fn build(args: impl Iterator<Item = String>) -> Result<Args, String> {
        let all: Vec<String> = args.collect();
        let legacy  = all.iter().any(|a| a == "--legacy");
        let verbose = all.iter().any(|a| a == "-v" || a == "--verbose");

        let jobs = all.windows(2).find_map(|w| {
            if w[0] == "--jobs" || w[0] == "-j" { w[1].parse::<usize>().ok() } else { None }
        });

        let mpp = all.windows(2).find_map(|w| {
            if w[0] == "--mpp" { w[1].parse::<f64>().ok() } else { None }
        }).ok_or_else(|| "--mpp <value> is required".to_string())?;

        let quality = all.windows(2).find_map(|w| {
            if w[0] == "--quality" { w[1].parse::<u8>().ok() } else { None }
        }).unwrap_or(87);

        let filter = all.windows(2).find_map(|w| {
            if w[0] == "--filter" {
                match w[1].to_lowercase().as_str() {
                    "nearest"               => Some(FilterType::Nearest),
                    "triangle" | "bilinear" => Some(FilterType::Triangle),
                    "catmullrom"| "bicubic" => Some(FilterType::CatmullRom),
                    "gaussian"              => Some(FilterType::Gaussian),
                    "lanczos3"              => Some(FilterType::Lanczos3),
                    _                       => None,
                }
            } else {
                None
            }
        }).unwrap_or(FilterType::Nearest);

        let mut positional: Vec<&str> = Vec::new();
        let mut skip_next = false;
        for token in &all[1..] {
            if skip_next { skip_next = false; continue; }
            if matches!(token.as_str(), "--jobs" | "-j" | "--mpp" | "--quality" | "--filter") {
                skip_next = true; continue;
            }
            if token.starts_with('-') { continue; }
            positional.push(token.as_str());
        }

        let input_dir  = positional.first()
            .ok_or_else(|| "Missing input directory".to_string())?.to_string();
        let output_dir = positional.get(1)
            .ok_or_else(|| "Missing output directory".to_string())?.to_string();

        Ok(Args { input_dir, output_dir, legacy, mpp, quality, filter, verbose, jobs })
    }
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
    Dir(u32),     // TIFFSetDirectory(tiff, index)
    SubDir(u64),  // TIFFSetSubDirectory(tiff, file_offset)
}

struct OutputLevel {
    out_img_w:   u32,
    out_img_h:   u32,
    out_tile_w:  u32,
    out_tile_h:  u32,
    actual_mpp_x: f64,
    actual_mpp_y: f64,
    src_idx:     usize,
    passthrough: bool,
}

// ─── Entry point ──────────────────────────────────────────────────────────────

fn main() {
    let start = std::time::Instant::now();
    let args = Args::build(std::env::args()).unwrap_or_else(|e| {
        eprintln!("Error: {e}");
        eprintln!("Usage: tiffds <input_dir> <output_dir> --mpp <mpp> [OPTIONS]");
        eprintln!("Options:");
        eprintln!("  --mpp <f>         Target microns-per-pixel (required)");
        eprintln!("  --legacy          Flat pyramidal BigTIFF instead of OME-TIFF");
        eprintln!("  --quality <n>     JPEG quality (default: 87)");
        eprintln!("  --filter <name>   nearest|triangle|catmullrom|gaussian|lanczos3 (default: nearest)");
        eprintln!("  -j/--jobs <n>     Number of parallel threads");
        eprintln!("  -v/--verbose      Verbose logging");
        std::process::exit(1);
    });
    run(args);
    println!("Total execution time: {:.2?}", start.elapsed());
}

// ─── Run ──────────────────────────────────────────────────────────────────────

fn run(args: Args) {
    if let Some(n) = args.jobs {
        let _ = rayon::ThreadPoolBuilder::new().num_threads(n).build_global();
    }

    if args.verbose {
        println!("Input:  {}", args.input_dir);
        println!("Output: {}", args.output_dir);
        println!("Target MPP: {:.4} µm/px", args.mpp);
    }

    if !Path::new(&args.output_dir).exists() {
        std::fs::create_dir_all(&args.output_dir).expect("Failed to create output directory");
    }

    // Remove stale .tmp files
    for entry in std::fs::read_dir(&args.output_dir).into_iter().flatten().flatten() {
        let p = entry.path();
        if p.extension().map_or(false, |e| e == "tmp") {
            let _ = std::fs::remove_file(&p);
        }
    }

    // Collect .tiff / .svs files
    let files: Vec<_> = WalkDir::new(&args.input_dir)
        .follow_links(true)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| {
            let ext = e.path().extension()
                .and_then(|s| s.to_str()).unwrap_or("").to_lowercase();
            ext == "tiff" || ext == "svs"
        })
        .collect();

    if files.is_empty() {
        eprintln!("No .tiff or .svs files found in {}", args.input_dir);
        return;
    }

    let mp = MultiProgress::new();
    let bar_style = ProgressStyle::with_template(
        "  {spinner:.green} [{elapsed_precise}] {bar:40.cyan/blue} {pos}/{len} tiles  {msg}"
    ).unwrap().progress_chars("=>-");

    let total_files  = files.len();
    let skipped      = AtomicUsize::new(0);
    let file_bar     = mp.add(ProgressBar::new(total_files as u64));
    file_bar.set_style(
        ProgressStyle::with_template(
            "  [{elapsed_precise}] {bar:40.green/black} {pos}/{len} files"
        ).unwrap().progress_chars("=>-")
    );

    for entry in &files {
        let src_path = entry.path().to_string_lossy().to_string();
        let src_name = entry.path().file_name().unwrap_or_default()
            .to_string_lossy().to_string();
        let src_stem = Path::new(&src_name)
            .file_stem().unwrap_or_default().to_string_lossy().to_string();
        let out_name = if args.legacy {
            format!("{src_stem}.tiff")
        } else {
            format!("{src_stem}.ome.tiff")
        };
        let out_path = Path::new(&args.output_dir).join(&out_name)
            .to_string_lossy().to_string();

        if Path::new(&out_path).exists() {
            if args.verbose { println!("  Skip (exists): {src_name}"); }
            skipped.fetch_add(1, Ordering::Relaxed);
            file_bar.inc(1);
            continue;
        }

        let pb = mp.add(ProgressBar::new(0));
        pb.set_style(bar_style.clone());
        pb.set_message(src_name.clone());

        process_file(&src_path, &out_path, &args, &pb);

        pb.finish_and_clear();
        file_bar.inc(1);
    }

    file_bar.finish_and_clear();

    let sk = skipped.load(Ordering::Relaxed);
    if sk > 0 {
        println!("  {sk} of {total_files} files skipped (output already exists).");
    }
}

// ─── Per-file processing ──────────────────────────────────────────────────────

fn process_file(src_path: &str, out_path: &str, args: &Args, pb: &ProgressBar) {
    let tmp_path = format!("{out_path}.tmp");

    // Read source pyramid structure
    let (src_levels, icc_profile) = {
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

    let base = &src_levels[0];
    if args.verbose {
        println!("  Source: {}x{} px @ {:.4} µm/px, {} levels",
            base.img_w, base.img_h, base.mpp_x, src_levels.len());
    }

    // Compute output pyramid levels
    let output_levels = compute_output_levels(&src_levels, args.mpp, args.verbose);
    if output_levels.is_empty() {
        eprintln!("  [warn] No output levels produced for {src_path}");
        return;
    }

    let total_tiles: u64 = output_levels.iter()
        .map(|lv| src_levels[lv.src_idx].n_tiles as u64)
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

    // Determine output channels from SamplesPerPixel, not from photometric.
    // Aperio JP2K (33005) stores RGB data but marks photometric as MINISBLACK,
    // so photometric alone cannot be trusted to determine the channel count.
    let base_src = &src_levels[output_levels[0].src_idx];
    let out_spp: u32 = if base_src.spp >= 3 { 3 } else { 1 };
    let out_photometric = if out_spp == 1 { PHOTOMETRIC_MINISBLACK } else { PHOTOMETRIC_YCBCR };

    // Setup fast_image_resize once
    let fir_alg = match args.filter {
        FilterType::Nearest    => fir::ResizeAlg::Nearest,
        FilterType::Triangle   => fir::ResizeAlg::Convolution(fir::FilterType::Bilinear),
        FilterType::CatmullRom => fir::ResizeAlg::Convolution(fir::FilterType::CatmullRom),
        FilterType::Gaussian   => fir::ResizeAlg::Convolution(fir::FilterType::Lanczos3),
        FilterType::Lanczos3   => fir::ResizeAlg::Convolution(fir::FilterType::Lanczos3),
    };
    let fir_pixel_type = if out_spp == 1 { fir::PixelType::U8 } else { fir::PixelType::U8x3 };
    let resize_opts = fir::ResizeOptions::new().resize_alg(fir_alg);

    // Open source and destination TIFFs
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
            let src_lv     = &src_levels[lv_out.src_idx];
            let is_base    = lv_idx == 0;
            let subfile    = if is_base { 0u32 } else { FILETYPE_REDUCEDIMAGE };

            // Navigate source TIFF to the chosen level
            navigate_to_level(src_tiff, &src_lv.nav);

            // Subsampling info from source (for passthrough JPEG)
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

            // Write IFD header to destination
            TIFFSetField(dst_tiff, TIFFTAG_SUBFILETYPE,      subfile);
            TIFFSetField(dst_tiff, TIFFTAG_IMAGEWIDTH,       lv_out.out_img_w);
            TIFFSetField(dst_tiff, TIFFTAG_IMAGELENGTH,      lv_out.out_img_h);
            TIFFSetField(dst_tiff, TIFFTAG_TILEWIDTH,
                tile_align(lv_out.out_tile_w, 16));
            TIFFSetField(dst_tiff, TIFFTAG_TILELENGTH,
                tile_align(lv_out.out_tile_h, 16));
            TIFFSetField(dst_tiff, TIFFTAG_SAMPLESPERPIXEL,  out_spp);
            TIFFSetField(dst_tiff, TIFFTAG_BITSPERSAMPLE,    8u32);
            TIFFSetField(dst_tiff, TIFFTAG_SAMPLEFORMAT,     SAMPLEFORMAT_UINT as u32);
            TIFFSetField(dst_tiff, TIFFTAG_PLANARCONFIG,     PLANARCONFIG_CONTIG as u32);
            TIFFSetField(dst_tiff, TIFFTAG_ORIENTATION,      ORIENTATION_TOPLEFT as u32);
            TIFFSetField(dst_tiff, TIFFTAG_RESOLUTIONUNIT,   RESUNIT_CENTIMETER as u32);
            TIFFSetField(dst_tiff, TIFFTAG_XRESOLUTION,      1e4 / lv_out.actual_mpp_x);
            TIFFSetField(dst_tiff, TIFFTAG_YRESOLUTION,      1e4 / lv_out.actual_mpp_y);

            if lv_out.passthrough {
                // Passthrough: preserve source compression and photometric
                TIFFSetField(dst_tiff, TIFFTAG_COMPRESSION,  src_lv.compression as u32);
                TIFFSetField(dst_tiff, TIFFTAG_PHOTOMETRIC,  src_lv.photometric as u32);
                if src_lv.photometric as u32 == PHOTOMETRIC_YCBCR {
                    TIFFSetField(dst_tiff, TIFFTAG_YCBCRSUBSAMPLING,
                        src_subsamp_h as u32, src_subsamp_v as u32);
                }
            } else {
                // Resample: output is JPEG-encoded
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
                if let Some(ref icc) = icc_profile {
                    TIFFSetField(dst_tiff, TIFFTAG_ICCPROFILE,
                        icc.len() as u32, icc.as_ptr() as *const c_void);
                }
            }

            // Pre-allocate read buffer (raw JPEG or decoded pixels)
            let raw_buf_size = (TIFFTileSize(src_tiff) as usize)
                .max(src_lv.tile_w as usize * src_lv.tile_h as usize * src_lv.spp as usize)
                .max(1 << 17);

            let n_tiles  = src_lv.n_tiles;
            let tile_ids: Vec<u32> = (0..n_tiles).collect();

            let pix_size    = src_lv.tile_w as usize
                * src_lv.tile_h as usize
                * src_lv.spp as usize;
            let src_is_jp2k   = is_jp2k(src_lv.compression as u32);
            // Compression 33003 (J2K/YUV16) stores YCbCr data in the J2K
            // codestream regardless of the TIFF photometric tag (which is often
            // PHOTOMETRIC_RGB on Aperio files).  When OpenJPEG applies inverse ICT
            // it returns SRGB; otherwise the components are still YCbCr and need
            // a manual convert.  We also check for color_space == SYCC at decode
            // time to catch standard JPEG-2000 YCbCr streams.
            let src_jp2k_is_ycbcr =
                src_is_jp2k
                    && src_lv.compression as u32 == COMPRESSION_APERIO_JP2_YCBCR;

            for chunk in tile_ids.chunks(chunk_size) {
                // Sequential: read tiles from source.
                // JP2K tiles are read as raw bytes for parallel decode by jpeg2k.
                // JPEG and other compressions use TIFFReadEncodedTile so libtiff
                // handles JPEGTABLES, partial SOF/SOS splits, and color conversion.
                // (tile_num, bytes, is_jp2k_raw)
                let raw_chunk: Vec<(u32, Vec<u8>, bool)> = chunk.iter()
                    .map(|&tile_num| {
                        if lv_out.passthrough || src_is_jp2k {
                            // Passthrough or JP2K: always raw bytes
                            let mut buf = vec![0u8; raw_buf_size];
                            let n = TIFFReadRawTile(src_tiff, tile_num,
                                buf.as_mut_ptr() as *mut c_void, buf.len() as i64);
                            if n > 0 {
                                buf.truncate(n as usize);
                                let is_jp2k_raw = src_is_jp2k && !lv_out.passthrough;
                                (tile_num, buf, is_jp2k_raw)
                            } else {
                                (tile_num, Vec::new(), false)
                            }
                        } else {
                            // JPEG and all other compressions: let libtiff decode.
                            // TIFFReadEncodedTile handles JPEGTABLES reconstruction
                            // and always returns packed RGB (or grayscale) pixels.
                            let mut buf = vec![0u8; pix_size];
                            let n = TIFFReadEncodedTile(src_tiff, tile_num,
                                buf.as_mut_ptr() as *mut c_void, buf.len() as i64);
                            if n > 0 { (tile_num, buf, false) }
                            else { (tile_num, Vec::new(), false) }
                        }
                    })
                    .collect();

                // Parallel: decode → resize → encode
                let quality              = args.quality;
                let src_tile_w           = src_lv.tile_w;
                let src_tile_h           = src_lv.tile_h;
                let out_tile_w           = lv_out.out_tile_w;
                let out_tile_h           = lv_out.out_tile_h;
                let passthrough          = lv_out.passthrough;
                let spp                  = out_spp;
                let resize_opts          = resize_opts.clone();
                let fpt                  = fir_pixel_type;
                let src_jp2k_is_ycbcr    = src_jp2k_is_ycbcr;

                let encoded: Vec<Option<(u32, Vec<u8>)>> = raw_chunk
                    .par_iter()
                    .map(|(tile_num, data, is_jp2k_raw)| {
                        if data.is_empty() { return None; }

                        if passthrough {
                            return Some((*tile_num, data.clone()));
                        }

                        let (pixels, pw, ph): (Vec<u8>, u32, u32) = if *is_jp2k_raw {
                            // Decode raw JP2K bytes with OpenJPEG (same logic as lib.rs)
                            let img = jpeg2k::Image::from_bytes(data).ok()?;
                            let comps = img.components();
                            if comps.is_empty() { return None; }
                            let luma_w = comps[0].width() as usize;
                            let luma_h = comps[0].height() as usize;
                            if luma_w == 0 || luma_h == 0 { return None; }

                            // Use data_u8() for correct scaling across all bit
                            // depths (8, 12, 16 …) and proper signed/unsigned handling.
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
                                        let y = y_u8[row * luma_w + col];
                                        let cb_col = (col * cb_w / luma_w).min(cb_w.saturating_sub(1));
                                        let cb_row = (row * cb_h / luma_h).min(cb_h.saturating_sub(1));
                                        let cb = cb_u8[cb_row * cb_w + cb_col];
                                        let cr_col = (col * cr_w / luma_w).min(cr_w.saturating_sub(1));
                                        let cr_row = (row * cr_h / luma_h).min(cr_h.saturating_sub(1));
                                        let cr = cr_u8[cr_row * cr_w + cr_col];
                                        buf.extend_from_slice(&[y, cb, cr]);
                                    }
                                }
                                buf
                            };
                            // Apply BT.601 YCbCr → RGB when:
                            //  a) OpenJPEG explicitly signals SYCC (YCbCr) output, OR
                            //  b) source compression is 33003 (J2K/YUV16) AND OpenJPEG
                            //     did NOT already apply inverse ICT (color_space != SRGB).
                            // This is checked against the decoded color_space, NOT the TIFF
                            // photometric tag, because Aperio 33003 files store YCbCr data
                            // internally but declare PHOTOMETRIC_RGB in the TIFF header.
                            let color_space = img.color_space();
                            let needs_ycbcr_cvt = spp == 3 && (
                                matches!(color_space, jpeg2k::ColorSpace::SYCC)
                                || (src_jp2k_is_ycbcr
                                    && !matches!(color_space, jpeg2k::ColorSpace::SRGB))
                            );
                            if needs_ycbcr_cvt {
                                for c in pix.chunks_mut(3) {
                                    let y  = c[0] as f32;
                                    let cb = c[1] as f32 - 128.0;
                                    let cr = c[2] as f32 - 128.0;
                                    c[0] = (y + 1.40200 * cr).clamp(0.0, 255.0) as u8;
                                    c[1] = (y - 0.34414 * cb - 0.71414 * cr).clamp(0.0, 255.0) as u8;
                                    c[2] = (y + 1.77200 * cb).clamp(0.0, 255.0) as u8;
                                }
                            }
                            (pix, luma_w as u32, luma_h as u32)
                        } else {
                            // Decoded pixels from TIFFReadEncodedTile
                            (data.clone(), src_tile_w, src_tile_h)
                        };

                        // Resize
                        let src_fir = fir::images::Image::from_vec_u8(
                            pw, ph, pixels, fpt).ok()?;
                        let mut dst_fir = fir::images::Image::new(out_tile_w, out_tile_h, fpt);
                        fir::Resizer::new().resize(&src_fir, &mut dst_fir, &resize_opts).ok()?;
                        let resized = dst_fir.into_vec();

                        // JPEG encode
                        let jpeg = if spp == 1 {
                            let img = turbojpeg::Image::<&[u8]> {
                                pixels: &resized,
                                width:  out_tile_w as usize,
                                pitch:  out_tile_w as usize,
                                height: out_tile_h as usize,
                                format: turbojpeg::PixelFormat::GRAY,
                            };
                            turbojpeg::compress(img, quality as i32, turbojpeg::Subsamp::Gray)
                                .ok()?.to_vec()
                        } else {
                            let img = turbojpeg::Image::<&[u8]> {
                                pixels: &resized,
                                width:  out_tile_w as usize,
                                pitch:  out_tile_w as usize * 3,
                                height: out_tile_h as usize,
                                format: turbojpeg::PixelFormat::RGB,
                            };
                            turbojpeg::compress(img, quality as i32, turbojpeg::Subsamp::Sub2x2)
                                .ok()?.to_vec()
                        };

                        Some((*tile_num, jpeg))
                    })
                    .collect();

                // Sequential: write tiles + update progress bar
                for item in encoded {
                    if let Some((tile_num, jpeg_bytes)) = item {
                        TIFFWriteRawTile(
                            dst_tiff, tile_num,
                            jpeg_bytes.as_ptr() as *mut c_void,
                            jpeg_bytes.len() as i64,
                        );
                    }
                    pb.inc(1);
                }
            }

            TIFFWriteDirectory(dst_tiff);
        }

        TIFFClose(dst_tiff);
        TIFFClose(src_tiff);
    }

    // Atomic rename: .tmp → final path
    if let Err(e) = std::fs::rename(&tmp_path, out_path) {
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

    // Check for SubIFD pyramid (OME-TIFF from dcm2tiff)
    let mut n_sub: u16 = 0;
    let mut sub_ptr: *const u64 = std::ptr::null();
    let has_subifds = unsafe {
        TIFFGetField(tiff, TIFFTAG_SUBIFD,
            &mut n_sub as *mut u16,
            &mut sub_ptr as *mut *const u64) != 0
    } && n_sub > 0 && !sub_ptr.is_null();

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
                unsafe { TIFFSetDirectory(tiff, 0); }  // reset before next SubDir
            }
        }
    } else {
        // Sequential IFDs (SVS / generic pyramid): collect all tiled IFDs
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

    // Sort by MPP ascending (highest resolution = lowest MPP first)
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
        // Not tiled → skip (strip images like thumbnail, label, macro)
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
        nav: LevelNav::Dir(0), // placeholder, overwritten by caller
    })
}

fn mpp_from_resolution(xres: f64, yres: f64, resunit: u32) -> (f64, f64) {
    if xres <= 0.0 || yres <= 0.0 {
        return (0.25, 0.25);
    }
    if resunit == RESUNIT_CENTIMETER {
        (10000.0 / xres, 10000.0 / yres)
    } else if resunit == RESUNIT_INCH {
        (25400.0 / xres, 25400.0 / yres)
    } else {
        (0.25, 0.25)
    }
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
//
// Mirrors dcm2tiff's write_resampled_tiff logic:
// For each source level i, compute target_lv_mpp = target_mpp * (src_level_i_mpp / src_base_mpp),
// then find the source level whose MPP is closest to that target.
// This preserves the existing pyramid structure (not necessarily 2x) and reuses
// the same level-selection and passthrough logic as dcm2tiff.

fn compute_output_levels(
    src_levels: &[LevelDesc],
    target_mpp: f64,
    verbose: bool,
) -> Vec<OutputLevel> {
    let base_mpp = src_levels[0].mpp_x;
    let mut out  = Vec::new();

    for (i, src_lv_i) in src_levels.iter().enumerate() {
        // Target MPP for this output level: same ratio as the source pyramid.
        let target_lv_mpp_x = target_mpp * (src_lv_i.mpp_x / base_mpp);
        let target_lv_mpp_y = target_mpp * (src_lv_i.mpp_y / base_mpp);

        // Find the source level with MPP closest to target_lv_mpp_x.
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
            && aligned;

        let (out_img_w, out_img_h, out_tile_w, out_tile_h, actual_mpp_x, actual_mpp_y) =
            if passthrough {
                (best.img_w, best.img_h, best.tile_w, best.tile_h, best.mpp_x, best.mpp_y)
            } else {
                let otw = nearest_16(best.tile_w as f64 * best.mpp_x / target_lv_mpp_x);
                let oth = nearest_16(best.tile_h as f64 * best.mpp_y / target_lv_mpp_y);
                let sx  = if best.tile_w > 0 { otw as f64 / best.tile_w as f64 } else { 1.0 };
                let sy  = if best.tile_h > 0 { oth as f64 / best.tile_h as f64 } else { 1.0 };
                let oiw = (best.img_w as f64 * sx).round() as u32;
                let oih = (best.img_h as f64 * sy).round() as u32;
                let amx = if otw > 0 { best.mpp_x * best.tile_w as f64 / otw as f64 } else { best.mpp_x };
                let amy = if oth > 0 { best.mpp_y * best.tile_h as f64 / oth as f64 } else { best.mpp_y };
                (oiw, oih, otw, oth, amx, amy)
            };

        if out_img_w.max(out_img_h) < MIN_PYRAMID_SIDE {
            if verbose {
                eprintln!("  [skip] level {i}: {}x{} below MIN_PYRAMID_SIDE ({})",
                    out_img_w, out_img_h, MIN_PYRAMID_SIDE);
            }
            continue;
        }

        if verbose {
            eprintln!("  [level {}] {}{}x{} @ {:.4} µm/px  (src level {} @ {:.4} µm/px, tile {}x{} → {}x{})",
                i,
                if passthrough { "passthrough " } else { "" },
                out_img_w, out_img_h, actual_mpp_x,
                best_idx, best.mpp_x,
                best.tile_w, best.tile_h, out_tile_w, out_tile_h);
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
