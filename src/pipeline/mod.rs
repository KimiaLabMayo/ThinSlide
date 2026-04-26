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
    skipped: &AtomicUsize,
) {
    let convert_start = std::time::Instant::now();

    let src = match DicomSource::from_series(series_meta) {
        Some(s) => s,
        None    => return,
    };

    let series_id   = src.metadata().name.clone();
    let ts_uid      = src.slide_levels[0].transfer_syntax_uid.clone();
    let comp        = map_transfer_syntax_to_compression(&ts_uid);
    let src_mpp_opt = src.slide_levels[0].mpp_x.filter(|&v| v > 0.0);
    let src_mpp     = src_mpp_opt.unwrap_or(0.0);

    let effective_mpp: Option<f64> = if args.half {
        Some(src_mpp * 2.0)
    } else {
        if src_mpp_opt.is_none() {
            if args.verbose {
                eprintln!("  [warn ] source MPP unknown; skipping (--mpp requires known source MPP)");
            }
            None
        } else {
            let mut em = if let Some(t) = args.mpp {
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
        skipped.fetch_add(1, Ordering::Relaxed);
        pb.finish_and_clear();
        return;
    }

    let tmp_path = format!("{}.tmp", output_path);

    if let Some(skip) = jp2k_svs_skip {
        crate::write_svs(
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
        crate::write_resampled_tiff(
            &src.slide_levels, &tmp_path,
            target_mpp, args.quality, args.filter,
            !args.legacy,
            Some(&pb),
            args.verbose,
            args.half,
            args.icc_bake,
        );
    } else if args.legacy {
        if is_jpeg2000(&comp) {
            crate::write_svs(
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
            crate::write_flat_multipage_tiff(
                &src.slide_levels,
                &tmp_path,
                Some(&pb),
                args.verbose,
                args.quality,
                args.icc_bake,
            );
        }
    } else {
        crate::write_ome_tiff(
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

    std::fs::rename(&tmp_path, &output_path)
        .expect("Failed to rename tmp file to output");

    let elapsed = convert_start.elapsed();
    mp.println(format!("  {} {:.2}s", pb_msg, elapsed.as_millis() as f64 / 1000.0)).ok();
    pb.finish_and_clear();
}

pub fn run(args: Args) {
    if let Some(n) = args.jobs {
        let _ = rayon::ThreadPoolBuilder::new()
            .num_threads(n)
            .build_global();
    }

    if args.verbose {
        eprintln!("[src] {}", args.input_dir);
        eprintln!("[out] {}", args.output_dir);
    }

    if !Path::new(&args.output_dir).exists() {
        std::fs::create_dir_all(&args.output_dir).expect("Failed to create output directory");
    }

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
    let mut last_dir_count   = 0usize;
    let mut total_file_count = 0usize;
    for entry in WalkDir::new(&args.input_dir).into_iter().filter_map(|e| e.ok()) {
        if !entry.file_type().is_file() { continue; }
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
    let skipped_count  = AtomicUsize::new(0);

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

    let mp_ref      = &mp;
    let args_ref    = &args;
    let skipped_ref = &skipped_count;

    rayon::scope(|s| {
        let n_concurrent = rayon::current_num_threads();
        let sem: Arc<(Mutex<usize>, Condvar)> = Arc::new((Mutex::new(0), Condvar::new()));

        for series_meta in rx {
            let series_idx = series_counter.fetch_add(1, Ordering::SeqCst) + 1;
            if args_ref.mpp.is_some() || args_ref.half || args_ref.icc_bake {
                convert_one_series(series_meta, series_idx, args_ref, mp_ref, skipped_ref);
            } else if n_concurrent <= 1 {
                convert_one_series(series_meta, series_idx, args_ref, mp_ref, skipped_ref);
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
                    convert_one_series(series_meta, series_idx, args_ref, mp_ref, skipped_ref);
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

    let total_processed = series_counter.load(Ordering::Relaxed);
    let skipped = skipped_count.load(Ordering::Relaxed);
    if skipped > 0 {
        println!("  {} of {} series skipped (output already exists).",
            skipped, total_processed);
    }

    if !tiff_paths.is_empty() {
        if args.mpp.is_some() || args.half || args.icc_bake {
            tiff_paths.sort();
            tiffds::process_files(&tiff_paths, &args, &mp);
        } else {
            eprintln!("  {} TIFF/SVS file(s) found; specify --mpp, --half, or --icc-bake to process them.",
                tiff_paths.len());
        }
    }
}
