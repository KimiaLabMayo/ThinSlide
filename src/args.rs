use std::path::Path;
use image::imageops::FilterType;
use clap::Parser;

#[derive(Parser)]
#[command(name = "thinslide", about = "Whole Slide Image Optimizer")]
pub struct Args {
    /// Input directory containing DICOM/VSI/MRXS files, or a direct path to a
    /// single TIFF/SVS file (must exist)
    #[arg(value_parser = parse_input_dir)]
    pub input_dir: String,

    /// Output directory (created if it does not exist; parent must exist)
    #[arg(value_parser = parse_output_dir)]
    pub output_dir: String,

    /// Use OpenSlide-compatible format (SVS / generic BigTIFF instead of OME-TIFF)
    #[arg(long)]
    pub openslide: bool,

    /// Enable verbose output
    #[arg(short = 'v', long)]
    pub verbose: bool,

    /// Use parent directory name instead of Series Instance UID
    #[arg(long)]
    pub use_parent_name: bool,

    /// Downsampling target - pick one:
    ///   20x       auto-detect from source MPP (80x->quarter, 40x->half, 20x->copy through, 10x->skip)
    ///   half      halve both dimensions unconditionally, even if source MPP is unknown
    ///   quarter   quarter both dimensions unconditionally; usually a precomputed
    ///             pyramid level, so typically faster than half
    ///   <number>  explicit target resolution in microns-per-pixel [0.001..=2.0], e.g. 0.5
    #[arg(long, value_parser = parse_scale, verbatim_doc_comment)]
    pub scale: Option<Scale>,

    /// Apply ICC color profile and convert to sRGB
    #[arg(long)]
    pub icc_bake: bool,

    /// Number of parallel threads (>= 1)
    #[arg(short = 'j', long, value_parser = parse_jobs)]
    pub jobs: Option<usize>,

    /// JPEG quality for resampling [20..=100]
    #[arg(long, default_value_t = 87, value_parser = clap::value_parser!(u8).range(20..=100))]
    pub quality: u8,

    /// Resampling kernel for --scale <number> [nearest, triangle, catmullrom, gaussian, lanczos3].
    /// Ignored with --scale 20x/half/quarter: decode-side downsampling (DCT 1/2 or 1/4 / DWT
    /// level-1 or level-2) produces the exact target size, so no pixel-domain resize step is performed.
    #[arg(long, default_value = "nearest", value_parser = parse_kernel)]
    pub kernel: FilterType,

    /// Override the default log file path (parent directory must exist)
    #[arg(long, value_parser = parse_log_file)]
    pub log_file: Option<String>,
}

#[derive(Clone, Copy, Debug)]
pub enum Scale {
    Mag20x,
    Half,
    Quarter,
    Mpp(f64),
}

impl Args {
    pub fn mag_20x(&self) -> bool { matches!(self.scale, Some(Scale::Mag20x)) }
    pub fn half(&self) -> bool { matches!(self.scale, Some(Scale::Half)) }
    pub fn quarter(&self) -> bool { matches!(self.scale, Some(Scale::Quarter)) }
    pub fn mpp(&self) -> Option<f64> {
        match self.scale {
            Some(Scale::Mpp(v)) => Some(v),
            _ => None,
        }
    }
}

fn parse_scale(s: &str) -> Result<Scale, String> {
    match s.to_lowercase().as_str() {
        "20x"     => Ok(Scale::Mag20x),
        "half"    => Ok(Scale::Half),
        "quarter" => Ok(Scale::Quarter),
        _ => {
            let v: f64 = s.parse().map_err(|_| format!(
                "'{}' is not a valid --scale value [20x, half, quarter, or a µm/px number in 0.001..=2.0]", s
            ))?;
            if !(0.001..=2.0).contains(&v) {
                return Err(format!("--scale must be 20x, half, quarter, or a number between 0.001 and 2.0, got {}", v));
            }
            Ok(Scale::Mpp(v))
        }
    }
}

fn parse_jobs(s: &str) -> Result<usize, String> {
    let v: usize = s.parse().map_err(|_| format!("'{}' is not a valid integer", s))?;
    if v == 0 { return Err("--jobs must be >= 1".to_string()); }
    Ok(v)
}

fn parse_input_dir(s: &str) -> Result<String, String> {
    let p = Path::new(s);
    if !p.exists() { return Err(format!("'{}' does not exist", s)); }
    if p.is_dir() { return Ok(s.to_string()); }
    // DICOM/VSI/MRXS are split across multiple files and need a directory, but a
    // single TIFF/SVS file may be passed directly.
    let ext = p.extension().and_then(|e| e.to_str()).unwrap_or("").to_lowercase();
    if matches!(ext.as_str(), "tiff" | "svs") {
        return Ok(s.to_string());
    }
    Err(format!("'{}' is not a directory (only a .tiff/.svs file may be passed directly)", s))
}

fn parse_output_dir(s: &str) -> Result<String, String> {
    let p = Path::new(s);
    let parent = p.parent().unwrap_or(Path::new("."));
    if !parent.as_os_str().is_empty() && !parent.exists() {
        return Err(format!("parent directory '{}' does not exist", parent.display()));
    }
    Ok(s.to_string())
}

fn parse_kernel(s: &str) -> Result<FilterType, String> {
    match s.to_lowercase().as_str() {
        "nearest"                => Ok(FilterType::Nearest),
        "triangle" | "bilinear" => Ok(FilterType::Triangle),
        "catmullrom"| "bicubic" => Ok(FilterType::CatmullRom),
        "gaussian"              => Ok(FilterType::Gaussian),
        "lanczos3"              => Ok(FilterType::Lanczos3),
        _ => Err(format!(
            "'{}' is not a valid kernel [nearest, triangle, catmullrom, gaussian, lanczos3]", s
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
