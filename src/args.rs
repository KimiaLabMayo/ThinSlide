use std::path::Path;
use image::imageops::FilterType;
use clap::Parser;

#[derive(Parser)]
#[command(name = "slean", about = "Whole Slide Image Optimizer")]
pub struct Args {
    /// Input directory containing DICOM files (must exist)
    #[arg(value_parser = parse_input_dir)]
    pub input_dir: String,

    /// Output directory (created if it does not exist; parent must exist)
    #[arg(value_parser = parse_output_dir)]
    pub output_dir: String,

    /// Use legacy format (SVS / generic BigTIFF instead of OME-TIFF)
    #[arg(long)]
    pub legacy: bool,

    /// Enable verbose output
    #[arg(short = 'v', long)]
    pub verbose: bool,

    /// Use parent directory name instead of Series Instance UID
    #[arg(long)]
    pub use_parent_name: bool,

    /// Halve both width and height (1/4 area); mutually exclusive with --mpp
    #[arg(long, conflicts_with = "mpp")]
    pub half: bool,

    /// Apply ICC color profile and convert to sRGB
    #[arg(long)]
    pub icc_bake: bool,

    /// Number of parallel threads (>= 1)
    #[arg(short = 'j', long, value_parser = parse_jobs)]
    pub jobs: Option<usize>,

    /// Target resolution in microns-per-pixel [0.001..=2.0]
    #[arg(long, value_parser = parse_mpp)]
    pub mpp: Option<f64>,

    /// JPEG quality for resampling [20..=100]
    #[arg(long, default_value_t = 87, value_parser = clap::value_parser!(u8).range(20..=100))]
    pub quality: u8,

    /// Resampling filter for --mpp [nearest, triangle, catmullrom, gaussian, lanczos3].
    /// Ignored with --half: decode-side halving (DCT 1/2 / DWT level-1) produces the
    /// exact target size, so no pixel-domain resize step is performed.
    #[arg(long, default_value = "nearest", value_parser = parse_filter)]
    pub filter: FilterType,

    /// Override the default log file path (parent directory must exist)
    #[arg(long, value_parser = parse_log_file)]
    pub log_file: Option<String>,
}

fn parse_jobs(s: &str) -> Result<usize, String> {
    let v: usize = s.parse().map_err(|_| format!("'{}' is not a valid integer", s))?;
    if v == 0 { return Err("--jobs must be >= 1".to_string()); }
    Ok(v)
}

fn parse_input_dir(s: &str) -> Result<String, String> {
    let p = Path::new(s);
    if !p.exists() { return Err(format!("'{}' does not exist", s)); }
    if !p.is_dir() { return Err(format!("'{}' is not a directory", s)); }
    Ok(s.to_string())
}

fn parse_output_dir(s: &str) -> Result<String, String> {
    let p = Path::new(s);
    let parent = p.parent().unwrap_or(Path::new("."));
    if !parent.as_os_str().is_empty() && !parent.exists() {
        return Err(format!("parent directory '{}' does not exist", parent.display()));
    }
    Ok(s.to_string())
}

fn parse_mpp(s: &str) -> Result<f64, String> {
    let v: f64 = s.parse().map_err(|_| format!("'{}' is not a valid number", s))?;
    if !(0.001..=2.0).contains(&v) {
        return Err(format!("--mpp must be between 0.001 and 2.0, got {}", v));
    }
    Ok(v)
}

fn parse_filter(s: &str) -> Result<FilterType, String> {
    match s.to_lowercase().as_str() {
        "nearest"                => Ok(FilterType::Nearest),
        "triangle" | "bilinear" => Ok(FilterType::Triangle),
        "catmullrom"| "bicubic" => Ok(FilterType::CatmullRom),
        "gaussian"              => Ok(FilterType::Gaussian),
        "lanczos3"              => Ok(FilterType::Lanczos3),
        _ => Err(format!(
            "'{}' is not a valid filter [nearest, triangle, catmullrom, gaussian, lanczos3]", s
        ))
    }
}

fn parse_log_file(s: &str) -> Result<String, String> {
    let p = Path::new(s);
    let parent = p.parent().unwrap_or(Path::new("."));
    if !parent.as_os_str().is_empty() && !parent.exists() {
        return Err(format!("parent directory '{}' does not exist", parent.display()));
    }
    Ok(s.to_string())
}
