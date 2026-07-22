use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use sysinfo::{CpuRefreshKind, MemoryRefreshKind, RefreshKind, System};
use crate::Args;

fn days_to_ymd(mut days: u64) -> (u32, u32, u32) {
    let y400 = days / 146097; days %= 146097;
    let y100 = (days / 36524).min(3); days -= y100 * 36524;
    let y4   = days / 1461; days %= 1461;
    let y1   = (days / 365).min(3); days -= y1 * 365;
    let year = (y400 * 400 + y100 * 100 + y4 * 4 + y1 + 1970) as u32;
    let leap = (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
    let month_days: [u32; 12] = [31, if leap { 29 } else { 28 }, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    let mut month = 1u32;
    let mut d = days as u32;
    for len in month_days {
        if d < len { break; }
        d -= len;
        month += 1;
    }
    (year, month, d + 1)
}

fn now_ymdhms() -> (u32, u32, u32, u32, u32, u32) {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
    let days = secs / 86400;
    let rem  = secs % 86400;
    let (h, m, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let (y, mo, d) = days_to_ymd(days);
    (y, mo, d, h as u32, m as u32, s as u32)
}

pub fn timestamp_string() -> String {
    let (y, mo, d, h, m, s) = now_ymdhms();
    format!("{:04}{:02}{:02}_{:02}{:02}{:02}", y, mo, d, h, m, s)
}

pub fn datetime_display() -> String {
    let (y, mo, d, h, m, s) = now_ymdhms();
    format!("{:04}-{:02}-{:02} {:02}:{:02}:{:02}", y, mo, d, h, m, s)
}

pub struct ConversionStats {
    pub ok:        AtomicUsize,
    pub fail:      AtomicUsize,
    pub skipped:   AtomicUsize,
    pub in_bytes:  AtomicU64,
    pub out_bytes: AtomicU64,
}

impl ConversionStats {
    pub fn new() -> Self {
        Self {
            ok:        AtomicUsize::new(0),
            fail:      AtomicUsize::new(0),
            skipped:   AtomicUsize::new(0),
            in_bytes:  AtomicU64::new(0),
            out_bytes: AtomicU64::new(0),
        }
    }
}

pub struct ConversionDetail {
    pub input_path:  String,
    pub output_path: String,
    pub encoding:    String,
    pub in_tile:     Option<(u32, u32)>,
    pub out_tile:    Option<(u32, u32)>,
    pub in_dim:      Option<(u32, u32)>,
    pub out_dim:     Option<(u32, u32)>,
    pub in_mpp:      f64,
    pub out_mpp:     f64,
}

pub struct ConversionLogger {
    writer: Mutex<BufWriter<File>>,
}

impl ConversionLogger {
    pub fn open(log_path: &str, args: &Args, start_dt: &str) -> std::io::Result<Self> {
        let file = OpenOptions::new()
            .create(true).write(true).truncate(true)
            .open(log_path)?;
        let logger = ConversionLogger { writer: Mutex::new(BufWriter::new(file)) };
        logger.write_header(args, start_dt);
        Ok(logger)
    }

    fn write_line(&self, line: &str) {
        if let Ok(mut w) = self.writer.lock() {
            let _ = writeln!(w, "{}", line);
            let _ = w.flush();
        }
    }

    fn write_header(&self, args: &Args, start_dt: &str) {
        use image::imageops::FilterType;
        let kernel_name = match args.kernel {
            FilterType::Nearest    => "nearest",
            FilterType::Triangle   => "triangle",
            FilterType::CatmullRom => "catmullrom",
            FilterType::Gaussian   => "gaussian",
            FilterType::Lanczos3   => "lanczos3",
        };

        let sys = System::new_with_specifics(
            RefreshKind::new()
                .with_cpu(CpuRefreshKind::everything())
                .with_memory(MemoryRefreshKind::everything()),
        );
        let cpu_brand    = sys.cpus().first().map(|c| c.brand()).unwrap_or("unknown").trim().to_string();
        let logical      = sys.cpus().len();
        let physical     = sys.physical_core_count().unwrap_or(0);
        let total_ram_mb = sys.total_memory() / (1024 * 1024);
        let os_str = System::long_os_version()
            .or_else(|| System::os_version())
            .unwrap_or_else(|| System::name().unwrap_or_default());

        self.write_line(&format!("=== conversion start: {} ===", start_dt));
        self.write_line(&format!("input:  {}", args.input_dir));
        self.write_line(&format!("output: {}", args.output_dir));
        self.write_line(&format!("os:     {}", os_str));
        self.write_line(&format!("cpu:    {}  logical={}  physical={}", cpu_brand, logical, physical));
        self.write_line(&format!("ram:    {} MB", total_ram_mb));
        self.write_line(&format!(
            "options: openslide={} verbose={} mag_20x={} icc_bake={} use_parent_name={} \
             quality={} jobs={:?} mpp={:?} kernel={}",
            args.openslide, args.verbose, args.mag_20x(), args.icc_bake,
            args.use_parent_name, args.quality, args.jobs, args.mpp(), kernel_name
        ));
        self.write_line("---");
    }

    pub fn log_ok(&self, idx: usize, filename: &str, elapsed_s: f64, in_b: u64, out_b: u64,
                  d: ConversionDetail) {
        let fmt_dim  = |dim: Option<(u32, u32)>| dim.map_or("?x?".into(), |(w, h)| format!("{}x{}", w, h));
        let fmt_tile = |t: Option<(u32, u32)>|   t.map_or("?x?".into(), |(w, h)| format!("{}x{}", w, h));
        self.write_line(&format!(
            "OK   ({:>4}) {:<52}  {:7.2}s  in={} out={}",
            idx, filename, elapsed_s, in_b, out_b
        ));
        self.write_line(&format!(
            "     in:  {}  encode={}  tile={}  dim={}  mpp={:.4}",
            d.input_path, d.encoding, fmt_tile(d.in_tile), fmt_dim(d.in_dim), d.in_mpp
        ));
        self.write_line(&format!(
            "     out: {}  tile={}  dim={}  mpp={:.4}",
            d.output_path, fmt_tile(d.out_tile), fmt_dim(d.out_dim), d.out_mpp
        ));
    }

    pub fn log_fail(&self, idx: usize, name: &str, reason: &str) {
        self.write_line(&format!("FAIL ({:>4}) {:<52}  {}", idx, name, reason));
    }

    pub fn log_skip(&self, idx: usize, filename: &str) {
        self.write_line(&format!(
            "SKIP ({:>4}) {:<52}  (output already exists)", idx, filename
        ));
    }

    pub fn write_summary(&self, stats: &ConversionStats, duration_s: f64) {
        let ok      = stats.ok.load(Ordering::Relaxed);
        let fail    = stats.fail.load(Ordering::Relaxed);
        let skipped = stats.skipped.load(Ordering::Relaxed);
        let in_b    = stats.in_bytes.load(Ordering::Relaxed);
        let out_b   = stats.out_bytes.load(Ordering::Relaxed);
        let ratio   = if in_b > 0 { out_b as f64 / in_b as f64 * 100.0 } else { 0.0 };
        self.write_line("---");
        self.write_line(&format!(
            "Total: {}  OK: {}  FAIL: {}  SKIP: {}",
            ok + fail + skipped, ok, fail, skipped
        ));
        self.write_line(&format!("Input  size: {} bytes", in_b));
        self.write_line(&format!("Output size: {} bytes", out_b));
        self.write_line(&format!("Ratio: {:.1}%", ratio));
        self.write_line(&format!("Duration: {:.1} s", duration_s));
    }
}
