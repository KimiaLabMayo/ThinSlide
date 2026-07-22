pub(crate) mod icc;
pub(crate) mod encode;
pub(crate) mod writer;
pub(crate) mod ome;

use std::path::Path;
use std::sync::{Arc, Mutex, Condvar};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::time::Duration;
use walkdir::WalkDir;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use crate::source::dicom::{
    DicomSource, DcmMetadata, extract_metadata, is_wsi_dicom,
    map_transfer_syntax_to_compression, is_jpeg2000,
};
use crate::source::SlideSource;
use crate::Args;
use crate::logger::{ConversionDetail, ConversionLogger, ConversionStats, datetime_display, timestamp_string};
use crate::tiffds;

// Strips directory components and rejects characters unsafe in filenames so
// that DICOM metadata values (SeriesInstanceUID, directory names) cannot cause
// writes outside the intended output directory.
fn sanitize_file_stem(stem: &str) -> String {
    let base = Path::new(stem)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("");
    let sanitized: String = base
        .chars()
        .filter(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | '.'))
        .collect();
    let sanitized = sanitized.trim_start_matches('.').to_string();
    if sanitized.is_empty() { "unnamed".to_string() } else { sanitized }
}

// Converts one WSI series (all resolution levels) to the appropriate output
// format and writes the result atomically via a .tmp file.
fn convert_one_series(
    series_meta: Vec<DcmMetadata>,
    series_idx: usize,
    args: &Args,
    mp: &MultiProgress,
    logger: &ConversionLogger,
    stats: &ConversionStats,
) {
    let convert_start = std::time::Instant::now();

    let input_bytes: u64 = series_meta.iter()
        .filter_map(|m| std::fs::metadata(&m.file_path).ok())
        .map(|fm| fm.len()).sum();
    let series_uid_for_log = series_meta.first()
        .map(|m| m.series_instance_uid.clone())
        .unwrap_or_default();

    let src = match DicomSource::from_series(series_meta) {
        Some(s) => s,
        None => {
            stats.fail.fetch_add(1, Ordering::Relaxed);
            logger.log_fail(series_idx, &series_uid_for_log, "failed to build DicomSource");
            return;
        }
    };

    let series_id   = src.metadata().name.clone();
    let ts_uid      = src.slide_levels[0].transfer_syntax_uid.clone();
    let comp        = map_transfer_syntax_to_compression(&ts_uid);
    let src_mpp_opt = src.slide_levels[0].mpp_x.filter(|&v| v > 0.0);
    let src_mpp     = src_mpp_opt.unwrap_or(0.0);

    let mut decode_shift: u32 = 0;
    let effective_mpp: Option<f64> = if args.quarter() {
        decode_shift = 2;
        Some(src_mpp * 4.0)
    } else if args.half() {
        decode_shift = 1;
        Some(src_mpp * 2.0)
    } else if args.mag_20x() {
        match src_mpp_opt.and_then(crate::factor_to_20x) {
            None => {
                stats.skipped.fetch_add(1, Ordering::Relaxed);
                logger.log_skip(series_idx, &series_id);
                if args.verbose {
                    eprintln!("  [skip ] {}  source MPP unknown or ≥0.7 µm/px (--scale 20x cannot upscale)", series_id);
                }
                return;
            }
            Some(1) => None,
            Some(f) => {
                decode_shift = f.trailing_zeros();
                Some(src_mpp * f as f64)
            }
        }
    } else {
        if src_mpp_opt.is_none() {
            if args.verbose {
                eprintln!("  [warn ] source MPP unknown; skipping (--scale <mpp> requires known source MPP)");
            }
            None
        } else {
            let mut em = if let Some(t) = args.mpp() {
                if t <= src_mpp {
                    eprintln!(
                        "  [warn ] requested MPP {:.4} µm/px ≤ source {:.4} µm/px (upscaling not supported); skipping",
                        t, src_mpp
                    );
                    None
                } else {
                    Some(t)
                }
            } else {
                None
            };
            if let Some(val) = em {
                if (val - src_mpp).abs() / src_mpp < 0.1 {
                    if args.verbose {
                        eprintln!(
                            "  [warn ] requested MPP {:.4} µm/px within 10% of source {:.4} µm/px; skipping",
                            val, src_mpp
                        );
                    }
                    em = None;
                }
            }
            em
        }
    };

    if args.verbose {
        let mode = match effective_mpp {
            Some(m) => format!("→ {:.4} µm/px", m),
            None    => "passthrough".to_string(),
        };
        eprintln!("({}) {}  {}  {:.4} µm/px  {} levels  {}",
            series_idx, series_id, comp, src_mpp, src.slide_levels.len(), mode);
    }

    // When --legacy is set, the source is JP2K, and a matching level exists,
    // passthrough as SVS without decoding.
    let jp2k_svs_skip: Option<usize> = if !args.legacy || args.icc_bake { None } else {
        effective_mpp.and_then(|target| {
            if !is_jpeg2000(&comp) { return None; }
            let skip = src.slide_levels.iter()
                .take_while(|m| m.mpp_x.unwrap_or(f64::MAX) < target * 0.9)
                .count();
            let has_match = src.slide_levels.get(skip)
                .and_then(|m| m.mpp_x)
                .map(|mpp| (mpp - target).abs() / target < 0.1)
                .unwrap_or(false);
            if skip > 0 && has_match { Some(skip) } else { None }
        })
    };

    let file_stem: String = if args.use_parent_name {
        let raw = Path::new(&src.slide_levels[0].file_path)
            .parent()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            .unwrap_or(series_id.as_str());
        sanitize_file_stem(raw)
    } else {
        sanitize_file_stem(series_id.as_str())
    };

    let output_path = if jp2k_svs_skip.is_some() {
        format!("{}/{}.svs", args.output_dir, file_stem)
    } else if effective_mpp.is_some() {
        if args.legacy {
            format!("{}/{}.tiff", args.output_dir, file_stem)
        } else {
            format!("{}/{}.ome.tiff", args.output_dir, file_stem)
        }
    } else if args.legacy && is_jpeg2000(&comp) {
        format!("{}/{}.svs", args.output_dir, file_stem)
    } else if args.legacy {
        format!("{}/{}.tiff", args.output_dir, file_stem)
    } else {
        format!("{}/{}.ome.tiff", args.output_dir, file_stem)
    };

    let fname    = Path::new(&output_path)
        .file_name().and_then(|n| n.to_str()).unwrap_or(series_id.as_str());
    let prefix   = format!("({})", series_idx);
    let max_name = 52usize.saturating_sub(prefix.len() + 1);
    let name_str = if fname.len() > max_name {
        format!("…{}", &fname[fname.len() - max_name.saturating_sub(1)..])
    } else {
        fname.to_string()
    };
    let pb_msg = format!("{} {}", prefix, name_str);

    let pb = mp.add(ProgressBar::new(0));
    pb.set_style(
        ProgressStyle::with_template(
            "  {msg:<52} [{bar:35.green/white}] {pos:>6}/{len} Tiles"
        ).unwrap().progress_chars("=>-"),
    );
    pb.set_message(pb_msg.clone());

    if Path::new(&output_path).exists() {
        stats.skipped.fetch_add(1, Ordering::Relaxed);
        logger.log_skip(series_idx, fname);
        pb.finish_and_clear();
        return;
    }

    let tmp_path = format!("{}.tmp", output_path);

    if let Some(skip) = jp2k_svs_skip {
        writer::write_svs(
            &src.slide_levels[skip..],
            src.thumbnail.as_ref(),
            src.label.as_ref(),
            src.overview.as_ref(),
            &tmp_path,
            Some(&pb),
            args.verbose,
            args.quality,
            args.icc_bake,
        );
    } else if let Some(target_mpp) = effective_mpp {
        writer::write_resampled_tiff(
            &src.slide_levels, &tmp_path,
            target_mpp, args.quality, args.filter,
            !args.legacy,
            Some(&pb),
            args.verbose,
            decode_shift,
            args.icc_bake,
        );
    } else if args.legacy {
        if is_jpeg2000(&comp) {
            writer::write_svs(
                &src.slide_levels,
                src.thumbnail.as_ref(),
                src.label.as_ref(),
                src.overview.as_ref(),
                &tmp_path,
                Some(&pb),
                args.verbose,
                args.quality,
                args.icc_bake,
            );
        } else {
            writer::write_flat_multipage_tiff(
                &src.slide_levels,
                &tmp_path,
                Some(&pb),
                args.verbose,
                args.quality,
                args.icc_bake,
            );
        }
    } else {
        writer::write_ome_tiff(
            &src.slide_levels,
            src.thumbnail.as_ref(),
            src.overview.as_ref(),
            src.label.as_ref(),
            &tmp_path,
            Some(&pb),
            args.verbose,
            args.quality,
            args.icc_bake,
        );
    }

    if let Err(e) = std::fs::rename(&tmp_path, &output_path) {
        let _ = std::fs::remove_file(&tmp_path);
        stats.fail.fetch_add(1, Ordering::Relaxed);
        logger.log_fail(series_idx, &series_id, &format!("rename failed: {}", e));
        pb.finish_and_clear();
        return;
    }

    let elapsed_s = convert_start.elapsed().as_millis() as f64 / 1000.0;
    let out_bytes = std::fs::metadata(&output_path).map(|m| m.len()).unwrap_or(0);
    stats.ok.fetch_add(1, Ordering::Relaxed);
    stats.in_bytes.fetch_add(input_bytes, Ordering::Relaxed);
    stats.out_bytes.fetch_add(out_bytes, Ordering::Relaxed);

    let base        = &src.slide_levels[0];
    let in_tile     = base.tile_size;
    let in_dim      = base.px_columns.zip(base.px_rows);
    let (out_tile, out_dim, out_mpp_log) = if let Some(skip) = jp2k_svs_skip {
        let lv = &src.slide_levels[skip];
        (lv.tile_size, lv.px_columns.zip(lv.px_rows), lv.mpp_x.unwrap_or(src_mpp))
    } else if let Some(target) = effective_mpp {
        let (tw, th) = in_tile.unwrap_or((0, 0));
        let out_t = if tw > 0 && th > 0 {
            Some((
                crate::nearest_16(tw as f64 * src_mpp / target * 2.0),
                crate::nearest_16(th as f64 * src_mpp / target * 2.0),
            ))
        } else { None };
        let out_d = in_dim.map(|(w, h)| (
            (w as f64 * src_mpp / target).round() as u32,
            (h as f64 * src_mpp / target).round() as u32,
        ));
        (out_t, out_d, target)
    } else {
        (in_tile, in_dim, src_mpp)
    };
    let input_dir = Path::new(&base.file_path)
        .parent().and_then(|p| p.to_str()).unwrap_or("").to_string();
    let detail = ConversionDetail {
        input_path:  input_dir,
        output_path: output_path.clone(),
        encoding:    comp.to_string(),
        in_tile, out_tile, in_dim, out_dim,
        in_mpp:  src_mpp,
        out_mpp: out_mpp_log,
    };

    logger.log_ok(series_idx, fname, elapsed_s, input_bytes, out_bytes, detail);
    mp.println(format!("  {} {:.2}s", pb_msg, elapsed_s)).ok();
    pb.finish_and_clear();
}

pub fn run(args: Args) {
    if let Some(n) = args.jobs {
        let _ = rayon::ThreadPoolBuilder::new()
            .num_threads(n)
            .build_global();
    }

    if (args.mag_20x() || args.half() || args.quarter()) && !matches!(args.filter, image::imageops::FilterType::Nearest) {
        eprintln!("[warn] --filter is ignored with --scale 20x/half/quarter: decode-side downsampling skips the resize step");
    }

    if args.verbose {
        eprintln!("[src] {}", args.input_dir);
        eprintln!("[out] {}", args.output_dir);
    }

    if !Path::new(&args.output_dir).exists() {
        std::fs::create_dir_all(&args.output_dir).expect("Failed to create output directory");
    }

    let run_start = std::time::Instant::now();
    let start_dt  = datetime_display();
    let log_path  = args.log_file.clone()
        .unwrap_or_else(|| format!("{}/conversion_{}.log", args.output_dir, timestamp_string()));
    let logger = ConversionLogger::open(&log_path, &args, &start_dt)
        .expect("Failed to open log file");
    let stats  = ConversionStats::new();

    for entry in std::fs::read_dir(&args.output_dir).into_iter().flatten().flatten() {
        let p = entry.path();
        if p.extension().map_or(false, |e| e == "tmp") {
            let _ = std::fs::remove_file(&p);
        }
    }

    let mp = MultiProgress::new();

    // Phase 1: discover .dcm paths grouped by directory (fast, no I/O).
    let scan_pb = mp.add(ProgressBar::new_spinner());
    scan_pb.set_style(
        ProgressStyle::with_template("  Scanning...  {msg}").unwrap()
    );
    scan_pb.enable_steady_tick(Duration::from_millis(100));

    let mut dir_map: std::collections::HashMap<std::path::PathBuf, Vec<String>> =
        std::collections::HashMap::new();
    let mut tiff_paths: Vec<std::path::PathBuf> = Vec::new();
    let mut vsi_paths: Vec<std::path::PathBuf> = Vec::new();
    let mut mrxs_paths: Vec<std::path::PathBuf> = Vec::new();
    let mut last_dir_count   = 0usize;
    let mut total_file_count = 0usize;
    for entry in WalkDir::new(&args.input_dir).follow_links(true).into_iter().filter_map(|e| e.ok()) {
        if !entry.path().is_file() { continue; }
        let ext = entry.path().extension()
            .and_then(|e| e.to_str()).unwrap_or("").to_lowercase();
        match ext.as_str() {
            "dcm" => {
                let parent = entry.path().parent()
                    .unwrap_or(Path::new(".")).to_path_buf();
                dir_map.entry(parent).or_default()
                    .push(entry.path().to_string_lossy().into_owned());
                total_file_count += 1;
                let n_dirs = dir_map.len();
                if n_dirs != last_dir_count {
                    scan_pb.set_message(format!("{} DCM in {} dirs, {} TIFF/SVS",
                        total_file_count, n_dirs, tiff_paths.len()));
                    last_dir_count = n_dirs;
                }
            }
            "tiff" | "svs" => {
                tiff_paths.push(entry.path().to_owned());
                scan_pb.set_message(format!("{} DCM in {} dirs, {} TIFF/SVS",
                    total_file_count, dir_map.len(), tiff_paths.len()));
            }
            "vsi" => {
                vsi_paths.push(entry.path().to_owned());
            }
            "mrxs" => {
                mrxs_paths.push(entry.path().to_owned());
            }
            _ => {}
        }
    }
    scan_pb.finish_and_clear();

    let mut dir_groups: Vec<Vec<String>> = dir_map
        .into_iter()
        .map(|(_, mut files)| { files.sort(); files })
        .collect();
    dir_groups.sort_by(|a, b| a[0].cmp(&b[0]));
    let total_files = total_file_count as u64;

    if args.verbose {
        eprintln!("Found {} DICOM files in {} directories", total_files, dir_groups.len());
        if !tiff_paths.is_empty() {
            eprintln!("Found {} TIFF/SVS files", tiff_paths.len());
        }
    }

    // Phase 2+3: metadata extraction pipelined with conversion.
    let meta_pb = mp.add(ProgressBar::new(total_files));
    meta_pb.set_style(
        ProgressStyle::with_template(
            "  Extracting metadata [{bar:35.cyan/white}] {pos}/{len} ({elapsed})"
        ).unwrap().progress_chars("=>-"),
    );

    let series_counter = AtomicUsize::new(0);

    let (tx, rx) = mpsc::channel::<Vec<DcmMetadata>>();

    let meta_pb_clone = meta_pb.clone();
    let scanner = std::thread::spawn(move || {
        for files in dir_groups {
            let n = files.len() as u64;
            let metas: Vec<DcmMetadata> = files.iter()
                .filter_map(|p| match extract_metadata(p) {
                    Ok(m) => Some(m),
                    Err(e) => { eprintln!("  [skip] {}: {}", p, e); None }
                })
                .collect();
            meta_pb_clone.inc(n);

            let mut by_series: std::collections::HashMap<String, Vec<DcmMetadata>> =
                std::collections::HashMap::new();
            for m in metas {
                if is_wsi_dicom(&m) {
                    by_series.entry(m.series_instance_uid.clone())
                        .or_default().push(m);
                }
            }
            for (_, series_metas) in by_series {
                tx.send(series_metas).ok();
            }
        }
    });

    let mp_ref     = &mp;
    let args_ref   = &args;
    let logger_ref = &logger;
    let stats_ref  = &stats;

    rayon::scope(|s| {
        let n_concurrent = rayon::current_num_threads();
        let sem: Arc<(Mutex<usize>, Condvar)> = Arc::new((Mutex::new(0), Condvar::new()));

        for series_meta in rx {
            let series_idx = series_counter.fetch_add(1, Ordering::SeqCst) + 1;
            if args_ref.mpp().is_some() || args_ref.mag_20x() || args_ref.half() || args_ref.quarter() {
                convert_one_series(series_meta, series_idx, args_ref, mp_ref, logger_ref, stats_ref);
            } else if n_concurrent <= 1 {
                convert_one_series(series_meta, series_idx, args_ref, mp_ref, logger_ref, stats_ref);
            } else {
                if args.verbose {
                    println!("Passthrough mode");
                }
                {
                    let (lock, cvar) = &*sem;
                    let mut active = lock.lock().unwrap();
                    while *active >= n_concurrent {
                        active = cvar.wait(active).unwrap();
                    }
                    *active += 1;
                }
                let sem_clone = Arc::clone(&sem);
                s.spawn(move |_| {
                    convert_one_series(series_meta, series_idx, args_ref, mp_ref, logger_ref, stats_ref);
                    let (lock, cvar) = &*sem_clone;
                    let mut active = lock.lock().unwrap();
                    *active -= 1;
                    cvar.notify_one();
                });
            }
        }
    });

    scanner.join().unwrap();
    meta_pb.finish_and_clear();

    if !tiff_paths.is_empty() {
        if args.mpp().is_some() || args.mag_20x() || args.half() || args.quarter() || args.icc_bake {
            tiff_paths.sort();
            tiffds::process_files(&tiff_paths, &args, &mp, &stats);
        } else {
            eprintln!("  {} TIFF/SVS file(s) found; specify --scale or --icc-bake to process them.",
                tiff_paths.len());
        }
    }

    if !vsi_paths.is_empty() {
        vsi_paths.sort();
        convert_vsi_files(&vsi_paths, &args, &mp, &stats);
    }

    if !mrxs_paths.is_empty() {
        mrxs_paths.sort();
        convert_mrxs_files(&mrxs_paths, &args, &mp, &stats);
    }

    // Report after every format has been converted, not just DICOM.
    let ok      = stats.ok.load(Ordering::Relaxed);
    let fail    = stats.fail.load(Ordering::Relaxed);
    let skipped = stats.skipped.load(Ordering::Relaxed);
    println!("Total: {}  OK: {}  FAIL: {}  SKIP: {}", ok + fail + skipped, ok, fail, skipped);
    let duration_s = run_start.elapsed().as_millis() as f64 / 1000.0;
    logger.write_summary(&stats, duration_s);
    println!("Log: {}", log_path);
}

// Experimental: transcode the main 2D pyramid of each CellSens .vsi to TIFF.
fn convert_vsi_files(paths: &[std::path::PathBuf], args: &Args, mp: &MultiProgress,
                     stats: &ConversionStats) {
    for path in paths {
        let src = path.to_string_lossy().to_string();
        let stem = sanitize_file_stem(
            path.file_stem().and_then(|s| s.to_str()).unwrap_or("image"));
        let out_path = if args.legacy {
            format!("{}/{}.tiff", args.output_dir, stem)
        } else {
            format!("{}/{}.ome.tiff", args.output_dir, stem)
        };
        if Path::new(&out_path).exists() {
            if args.verbose { eprintln!("  [skip ] exists: {}", out_path); }
            stats.skipped.fetch_add(1, Ordering::Relaxed);
            continue;
        }

        let pb = mp.add(ProgressBar::new(0));
        pb.set_style(ProgressStyle::with_template(
            "  {msg:<52} [{bar:35.green/white}] {pos:>6}/{len} Tiles"
        ).unwrap().progress_chars("=>-"));
        pb.set_message(stem.clone());

        let tmp_path = format!("{}.tmp", out_path);
        match crate::source::vsi::convert_vsi(
            &src, &tmp_path, args.legacy, args.quality, args.mag_20x(), args.half(), args.quarter(), args.verbose, Some(&pb),
        ) {
            Ok(()) => {
                if let Err(e) = std::fs::rename(&tmp_path, &out_path) {
                    let _ = std::fs::remove_file(&tmp_path);
                    eprintln!("  [error] rename failed for {}: {}", stem, e);
                    stats.fail.fetch_add(1, Ordering::Relaxed);
                } else {
                    mp.println(format!("  {} (vsi)", stem)).ok();
                    let in_b  = crate::source::vsi::input_size(&src);
                    let out_b = std::fs::metadata(&out_path).map(|m| m.len()).unwrap_or(0);
                    stats.ok.fetch_add(1, Ordering::Relaxed);
                    stats.in_bytes.fetch_add(in_b, Ordering::Relaxed);
                    stats.out_bytes.fetch_add(out_b, Ordering::Relaxed);
                }
            }
            Err(e) => {
                let _ = std::fs::remove_file(&tmp_path);
                eprintln!("  [skip ] {}: {}", stem, e);
                stats.fail.fetch_add(1, Ordering::Relaxed);
            }
        }
        pb.finish_and_clear();
    }
}

// Transcode each MIRAX (.mrxs) slide's level-0 placement into a pyramidal TIFF.
fn convert_mrxs_files(paths: &[std::path::PathBuf], args: &Args, mp: &MultiProgress,
                      stats: &ConversionStats) {
    for path in paths {
        let src = path.to_string_lossy().to_string();
        let stem = sanitize_file_stem(
            path.file_stem().and_then(|s| s.to_str()).unwrap_or("image"));
        let out_path = if args.legacy {
            format!("{}/{}.tiff", args.output_dir, stem)
        } else {
            format!("{}/{}.ome.tiff", args.output_dir, stem)
        };
        if Path::new(&out_path).exists() {
            if args.verbose { eprintln!("  [skip ] exists: {}", out_path); }
            stats.skipped.fetch_add(1, Ordering::Relaxed);
            continue;
        }

        let pb = mp.add(ProgressBar::new(0));
        pb.set_style(ProgressStyle::with_template(
            "  {msg:<52} [{bar:35.green/white}] {pos:>6}/{len} Tiles"
        ).unwrap().progress_chars("=>-"));
        pb.set_message(stem.clone());

        let tmp_path = format!("{}.tmp", out_path);
        match crate::source::mrxs::convert_mrxs(
            &src, &tmp_path, args.legacy, args.quality, args.mag_20x(), args.half(), args.quarter(), args.mpp(), args.verbose, Some(&pb),
        ) {
            Ok(()) => {
                if let Err(e) = std::fs::rename(&tmp_path, &out_path) {
                    let _ = std::fs::remove_file(&tmp_path);
                    eprintln!("  [error] rename failed for {}: {}", stem, e);
                    stats.fail.fetch_add(1, Ordering::Relaxed);
                } else {
                    mp.println(format!("  {} (mrxs)", stem)).ok();
                    let in_b  = crate::source::mrxs::input_size(&src);
                    let out_b = std::fs::metadata(&out_path).map(|m| m.len()).unwrap_or(0);
                    stats.ok.fetch_add(1, Ordering::Relaxed);
                    stats.in_bytes.fetch_add(in_b, Ordering::Relaxed);
                    stats.out_bytes.fetch_add(out_b, Ordering::Relaxed);
                }
            }
            Err(e) => {
                let _ = std::fs::remove_file(&tmp_path);
                eprintln!("  [skip ] {}: {}", stem, e);
                stats.fail.fetch_add(1, Ordering::Relaxed);
            }
        }
        pb.finish_and_clear();
    }
}
