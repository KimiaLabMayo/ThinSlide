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
    TIFFTAG_JPEGTABLES, TIFFTAG_YCBCRSUBSAMPLING, TIFFTAG_SUBIFD,
    TIFFTAG_XRESOLUTION, TIFFTAG_YRESOLUTION, TIFFTAG_RESOLUTIONUNIT,
    COMPRESSION_JPEG,
    PHOTOMETRIC_RGB, PHOTOMETRIC_YCBCR, PHOTOMETRIC_MINISBLACK,
    RESUNIT_CENTIMETER, RESUNIT_INCH,
    SAMPLEFORMAT_UINT, PLANARCONFIG_CONTIG, ORIENTATION_TOPLEFT,
    FILETYPE_REDUCEDIMAGE,
};
use wsi_tools::{tile_align, nearest_16, MIN_PYRAMID_SIDE, xml_escape, split_jpeg_to_tables_and_tile};

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
    if base.mpp_x <= 0.0 {
        eprintln!("  [error] Cannot determine resolution for {src_path}: \
            no XRESOLUTION tag and no 'MPP = <value>' in ImageDescription. Skipping.");
        return;
    }
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

            // Navigate source TIFF to the chosen level.
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
                // JPEG tiles store quantization/Huffman tables in JPEGTABLES rather than
                // inside each tile.  Copy the tag verbatim so readers (libtiff, OpenSlide)
                // can decode the raw tile bytes we are about to copy unchanged.
                if src_lv.compression as u32 == COMPRESSION_JPEG {
                    let mut tlen: u32 = 0;
                    let mut tptr: *const u8 = std::ptr::null();
                    let ok = TIFFGetField(src_tiff, TIFFTAG_JPEGTABLES,
                        &mut tlen as *mut u32,
                        &mut tptr as *mut *const u8);
                    if ok != 0 && !tptr.is_null() && tlen > 2 {
                        TIFFSetField(dst_tiff, TIFFTAG_JPEGTABLES,
                            tlen, tptr);
                    }
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
            let src_is_jpeg   = src_lv.compression as u32 == COMPRESSION_JPEG;
            // Compression 33003 (J2K/YUV16) stores YCbCr data in the J2K
            // codestream regardless of the TIFF photometric tag (which is often
            // PHOTOMETRIC_RGB on Aperio files).  When OpenJPEG applies inverse ICT
            // it returns SRGB; otherwise the components are still YCbCr and need
            // a manual convert.  We also check for color_space == SYCC at decode
            // time to catch standard JPEG-2000 YCbCr streams.
            let src_jp2k_is_ycbcr =
                src_is_jp2k
                    && src_lv.compression as u32 == COMPRESSION_APERIO_JP2_YCBCR;


            // JP2K DWT reduction: decode at 1/2^n resolution to reduce work.
            // out_tile_w = nat_otw * 2 for stitch mode, so the effective scale per
            // source tile is src_tile / nat_otw = src_tile / (out_tile / 2).
            let n_reduce: u32 = if src_is_jp2k && !lv_out.passthrough {
                let nat_otw = (lv_out.out_tile_w / 2).max(1);
                let nat_oth = (lv_out.out_tile_h / 2).max(1);
                let scale_down = (src_lv.tile_w as f64 / nat_otw as f64)
                    .min(src_lv.tile_h as f64 / nat_oth as f64);
                if scale_down > 1.0 { scale_down.log2().floor() as u32 } else { 0 }
            } else {
                0
            };

            // JPEG TIFF stores quantization/Huffman tables in the JPEGTABLES tag
            // separately from the tile data.  Each raw tile contains SOI+SOF+SOS+data+EOI
            // but is missing DQT, so it is not a self-contained JPEG stream.
            // We read the tables once per level and prepend them when building the
            // complete stream for turbojpeg:
            //   combined = JPEGTABLES[0..len-2] + tile[2..]
            //            = SOI + DQT + DHT + SOF + SOS + data + EOI  ✓
            let jpeg_tables: Option<Vec<u8>> = if src_is_jpeg && !lv_out.passthrough {
                let mut tlen: u32 = 0;
                let mut tptr: *const u8 = std::ptr::null();
                let ok = TIFFGetField(src_tiff, TIFFTAG_JPEGTABLES,
                    &mut tlen as *mut u32,
                    &mut tptr as *mut *const u8);
                if ok != 0 && !tptr.is_null() && tlen > 2 {
                    Some(std::slice::from_raw_parts(tptr, tlen as usize).to_vec())
                } else {
                    None
                }
            } else {
                None
            };
            let jpeg_tables_ref: Option<&[u8]> = jpeg_tables.as_deref();
            // For resampled levels, JPEGTABLES is set once per IFD on the first tile.
            // Passthrough levels already have JPEGTABLES copied from the source IFD.
            let mut jpegtables_set = lv_out.passthrough;

            // Variables captured by decode/encode closures
            let quality           = args.quality;
            let src_tile_w        = src_lv.tile_w;
            let src_tile_h        = src_lv.tile_h;
            let out_tile_w        = lv_out.out_tile_w;
            let out_tile_h        = lv_out.out_tile_h;
            let spp               = out_spp;
            let resize_opts       = resize_opts.clone();
            let fpt               = fir_pixel_type;
            let src_jp2k_is_ycbcr = src_jp2k_is_ycbcr;
            let src_photometric   = src_lv.photometric as u32;
            let n_reduce          = n_reduce;
            let jpeg_tables_ref   = jpeg_tables_ref;

            if lv_out.passthrough {
                // Passthrough: copy source tiles to the output IFD unchanged.
                // JPEGTABLES was already copied from the source IFD above.
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
                // Resample: stitch a 2×2 block of source tiles into each output tile.
                // Output tile (oc, or) is produced from source tiles (2*oc+dc, 2*or+dr)
                // for dc,dr ∈ {0,1}.  Source tiles out of image bounds are left as zero.
                let src_ntx = (src_lv.img_w + src_tile_w - 1) / src_tile_w;
                let src_nty = (src_lv.img_h + src_tile_h - 1) / src_tile_h;
                let out_ntx = (lv_out.out_img_w + out_tile_w - 1) / out_tile_w;
                let out_nty = (lv_out.out_img_h + out_tile_h - 1) / out_tile_h;
                let out_tile_ids: Vec<u32> = (0..out_ntx * out_nty).collect();

                for chunk in out_tile_ids.chunks(chunk_size) {
                    // Sequential: for each output tile read up to 4 source tiles.
                    // qi = dr*2 + dc  →  (dc=qi%2, dr=qi/2)
                    // Each slot: Option<(raw_bytes, is_raw_decode)>
                    let raw_chunk: Vec<(u32, [Option<(Vec<u8>, bool)>; 4])> = chunk.iter()
                        .map(|&out_id| {
                            let oc  = out_id % out_ntx;
                            let or_ = out_id / out_ntx;
                            let mut quads: [Option<(Vec<u8>, bool)>; 4] =
                                [None, None, None, None];
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
                                    // Try raw read first (complete JPEG stream).
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
                                        // Fall back to libtiff decode (JPEGTABLES path).
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

                    // Parallel: decode all 4 quads → stitch canvas → resize → encode.
                    // Decode first so canvas is sized to actual decoded tile dimensions.
                    // JP2K n_reduce > 0 produces smaller decoded tiles; canvas must use
                    // decoded size (not src_tile_w) to avoid black padding.
                    let encoded: Vec<Option<(u32, Vec<u8>)>> = raw_chunk
                        .par_iter()
                        .map(|(out_id, quads)| {
                            let ch = spp as usize;

                            // ── Decode each quadrant → Option<(pixels, pw, ph)> ─
                            const APP14_ADOBE_RGB: [u8; 16] = [
                                0xFF, 0xEE, 0x00, 0x0E,
                                b'A', b'd', b'o', b'b', b'e',
                                0x00, 0x64, 0x00, 0x00, 0x00, 0x00, 0x00,
                            ];
                            let decoded: [Option<(Vec<u8>, u32, u32)>; 4] =
                                std::array::from_fn(|qi| {
                                    let (data, is_raw_decode) = quads[qi].as_ref()?;
                                    if *is_raw_decode && src_is_jpeg {
                                        let inject_app14 =
                                            spp == 3 && src_photometric == PHOTOMETRIC_RGB;
                                        let combined: Vec<u8> =
                                            if let Some(tables) = jpeg_tables_ref {
                                                let app14_len = if inject_app14 {
                                                    APP14_ADOBE_RGB.len()
                                                } else { 0 };
                                                let mut v = Vec::with_capacity(
                                                    2 + app14_len
                                                    + (tables.len() - 4)
                                                    + (data.len() - 2));
                                                v.extend_from_slice(&tables[0..2]);
                                                if inject_app14 {
                                                    v.extend_from_slice(&APP14_ADOBE_RGB);
                                                }
                                                v.extend_from_slice(
                                                    &tables[2..tables.len()-2]);
                                                v.extend_from_slice(&data[2..]);
                                                v
                                            } else { data.clone() };
                                        let fmt = if spp == 1 {
                                            turbojpeg::PixelFormat::GRAY
                                        } else {
                                            turbojpeg::PixelFormat::RGB
                                        };
                                        let dec =
                                            turbojpeg::decompress(&combined, fmt).ok()?;
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
                                    } else if *is_raw_decode {
                                        // JP2K
                                        let params = jpeg2k::DecodeParameters::default()
                                            .reduce(n_reduce);
                                        let img =
                                            jpeg2k::Image::from_bytes_with(data, params)
                                                .ok()?;
                                        let comps = img.components();
                                        if comps.is_empty() { return None; }
                                        let luma_w = comps[0].width() as usize;
                                        let luma_h = comps[0].height() as usize;
                                        if luma_w == 0 || luma_h == 0 { return None; }
                                        let mut pix: Vec<u8> =
                                            if spp == 1 || comps.len() < 3 {
                                                comps[0].data_u8().collect()
                                            } else {
                                                let y_u8: Vec<u8> =
                                                    comps[0].data_u8().collect();
                                                let cb_u8: Vec<u8> =
                                                    comps[1].data_u8().collect();
                                                let cr_u8: Vec<u8> =
                                                    comps[2].data_u8().collect();
                                                let cb_w = comps[1].width() as usize;
                                                let cb_h = comps[1].height() as usize;
                                                let cr_w = comps[2].width() as usize;
                                                let cr_h = comps[2].height() as usize;
                                                let mut buf = Vec::with_capacity(
                                                    luma_w * luma_h * 3);
                                                for row in 0..luma_h {
                                                    for col in 0..luma_w {
                                                        let y = y_u8[row*luma_w+col];
                                                        let cb_col = (col*cb_w/luma_w)
                                                            .min(cb_w.saturating_sub(1));
                                                        let cb_row = (row*cb_h/luma_h)
                                                            .min(cb_h.saturating_sub(1));
                                                        let cb = cb_u8[cb_row*cb_w+cb_col];
                                                        let cr_col = (col*cr_w/luma_w)
                                                            .min(cr_w.saturating_sub(1));
                                                        let cr_row = (row*cr_h/luma_h)
                                                            .min(cr_h.saturating_sub(1));
                                                        let cr = cr_u8[cr_row*cr_w+cr_col];
                                                        buf.extend_from_slice(&[y, cb, cr]);
                                                    }
                                                }
                                                buf
                                            };
                                        let color_space = img.color_space();
                                        let needs_ycbcr_cvt = spp == 3 && (
                                            matches!(color_space, jpeg2k::ColorSpace::SYCC)
                                            || (src_jp2k_is_ycbcr && !matches!(
                                                color_space, jpeg2k::ColorSpace::SRGB))
                                        );
                                        if needs_ycbcr_cvt {
                                            for c in pix.chunks_mut(3) {
                                                let y  = c[0] as f32;
                                                let cb = c[1] as f32 - 128.0;
                                                let cr = c[2] as f32 - 128.0;
                                                c[0] = (y + 1.40200 * cr)
                                                    .clamp(0.0, 255.0) as u8;
                                                c[1] = (y - 0.34414*cb - 0.71414*cr)
                                                    .clamp(0.0, 255.0) as u8;
                                                c[2] = (y + 1.77200 * cb)
                                                    .clamp(0.0, 255.0) as u8;
                                            }
                                        }
                                        Some((pix, luma_w as u32, luma_h as u32))
                                    } else {
                                        // Pre-decoded pixels from TIFFReadEncodedTile
                                        Some((data.clone(), src_tile_w, src_tile_h))
                                    }
                                });

                            if decoded.iter().all(|d| d.is_none()) { return None; }

                            // ── Canvas sized to decoded tile dims, not src_tile_w ─
                            let (slot_w, slot_h) = decoded.iter()
                                .filter_map(|d| d.as_ref().map(|(_, pw, ph)| (*pw, *ph)))
                                .fold((1u32, 1u32), |(mw, mh), (w, h)|
                                    (mw.max(w), mh.max(h)));
                            let canvas_w = slot_w * 2;
                            let canvas_h = slot_h * 2;
                            let mut canvas =
                                vec![0u8; canvas_w as usize * canvas_h as usize * ch];

                            // ── Paste each decoded quadrant into the canvas ─────
                            for qi in 0..4usize {
                                let Some((pixels, pw, ph)) = &decoded[qi]
                                    else { continue; };
                                let dc = qi % 2;
                                let dr = qi / 2;
                                let ox = dc * slot_w as usize;
                                let oy = dr * slot_h as usize;
                                for row in 0..(*ph as usize) {
                                    let src_start = row * *pw as usize * ch;
                                    let dst_start =
                                        (oy + row) * canvas_w as usize * ch + ox * ch;
                                    let copy_len = *pw as usize * ch;
                                    canvas[dst_start..dst_start + copy_len]
                                        .copy_from_slice(
                                            &pixels[src_start..src_start + copy_len]);
                                }
                            }

                            // Resize stitch canvas → output tile dimensions
                            let src_fir = fir::images::Image::from_vec_u8(
                                canvas_w, canvas_h, canvas, fpt).ok()?;
                            let mut dst_fir =
                                fir::images::Image::new(out_tile_w, out_tile_h, fpt);
                            fir::Resizer::new()
                                .resize(&src_fir, &mut dst_fir, &resize_opts).ok()?;
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
                                turbojpeg::compress(
                                    img, quality as i32, turbojpeg::Subsamp::Gray)
                                    .ok()?.to_vec()
                            } else {
                                let img = turbojpeg::Image::<&[u8]> {
                                    pixels: &resized,
                                    width:  out_tile_w as usize,
                                    pitch:  out_tile_w as usize * 3,
                                    height: out_tile_h as usize,
                                    format: turbojpeg::PixelFormat::RGB,
                                };
                                turbojpeg::compress(
                                    img, quality as i32, turbojpeg::Subsamp::Sub2x2)
                                    .ok()?.to_vec()
                            };

                            Some((*out_id, jpeg))
                        })
                        .collect();

                    // Sequential: write tiles + update JPEGTABLES + progress bar.
                    // DQT+DHT extracted from the first tile into the IFD JPEGTABLES tag.
                    for item in encoded {
                        if let Some((out_id, jpeg_bytes)) = item {
                            let write_bytes: std::borrow::Cow<[u8]> =
                                match split_jpeg_to_tables_and_tile(&jpeg_bytes) {
                                    Some((tables, stripped)) => {
                                        if !jpegtables_set {
                                            TIFFSetField(dst_tiff, TIFFTAG_JPEGTABLES,
                                                tables.len() as u32, tables.as_ptr());
                                            jpegtables_set = true;
                                        }
                                        std::borrow::Cow::Owned(stripped)
                                    }
                                    None => std::borrow::Cow::Borrowed(jpeg_bytes.as_slice()),
                                };
                            TIFFWriteRawTile(dst_tiff, out_id,
                                write_bytes.as_ptr() as *mut c_void,
                                write_bytes.len() as i64);
                        }
                        pb.inc(1);
                    }
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

    // If XRESOLUTION/YRESOLUTION are absent (common in SVS), try ImageDescription.
    // Aperio format: "... |MPP = 0.4990| ..."
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

    // Sort by image width descending to reliably identify the base (highest-resolution) level.
    // This is more robust than sorting by mpp_x because sub-levels in SVS often lack
    // XRESOLUTION/YRESOLUTION tags, causing mpp_from_resolution to return the same
    // fallback value (0.25) for every level.
    levels.sort_by(|a, b| b.img_w.cmp(&a.img_w));

    // Derive sub-level MPPs from image-dimension ratios relative to the base level.
    // The base level (largest image) has reliable XRESOLUTION; sub-levels often do not.
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

    // Re-sort by corrected MPP ascending (highest resolution = lowest MPP first)
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
        // Signal "unknown": caller must try other sources (e.g. ImageDescription)
        return (0.0, 0.0);
    }
    if resunit == RESUNIT_CENTIMETER {
        (10000.0 / xres, 10000.0 / yres)
    } else if resunit == RESUNIT_INCH {
        (25400.0 / xres, 25400.0 / yres)
    } else {
        // Unknown unit — signal unknown rather than guessing
        (0.0, 0.0)
    }
}

/// Parse `MPP = <value>` from an Aperio-style ImageDescription string.
fn parse_mpp_from_image_description(tiff: *mut TIFF) -> Option<f64> {
    let mut desc_ptr: *const std::os::raw::c_char = std::ptr::null();
    let ok = unsafe {
        TIFFGetField(tiff, TIFFTAG_IMAGEDESCRIPTION,
            &mut desc_ptr as *mut *const std::os::raw::c_char)
    };
    if ok == 0 || desc_ptr.is_null() { return None; }
    let desc = unsafe { std::ffi::CStr::from_ptr(desc_ptr) }.to_string_lossy();
    // Look for "MPP = <float>" (case-sensitive, Aperio convention)
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
                // Natural single-tile size (1 source tile → 1 output tile at this scale).
                let nat_otw = nearest_16(best.tile_w as f64 * best.mpp_x / target_lv_mpp_x);
                let nat_oth = nearest_16(best.tile_h as f64 * best.mpp_y / target_lv_mpp_y);
                // Output image size and actual MPP from the natural scale (unchanged).
                let sx  = if best.tile_w > 0 { nat_otw as f64 / best.tile_w as f64 } else { 1.0 };
                let sy  = if best.tile_h > 0 { nat_oth as f64 / best.tile_h as f64 } else { 1.0 };
                let oiw = (best.img_w as f64 * sx).round() as u32;
                let oih = (best.img_h as f64 * sy).round() as u32;
                let amx = if nat_otw > 0 { best.mpp_x * best.tile_w as f64 / nat_otw as f64 } else { best.mpp_x };
                let amy = if nat_oth > 0 { best.mpp_y * best.tile_h as f64 / nat_oth as f64 } else { best.mpp_y };
                // Output tile: 2× the natural size so that each output tile is produced
                // by stitching a 2×2 block of source tiles.  This keeps tile sizes close
                // to the source tile size and avoids small tiles (e.g. 128→256 for 2× scale).
                let otw = nat_otw * 2;
                let oth = nat_oth * 2;
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
