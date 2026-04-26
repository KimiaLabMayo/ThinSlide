use image::imageops::FilterType;

pub struct Args {
    pub input_dir:       String,
    pub output_dir:      String,
    pub legacy:          bool,
    pub verbose:         bool,
    pub jobs:            Option<usize>,
    /// Target resolution in microns-per-pixel.  When set, tiles are decoded,
    /// resampled to the nearest valid tile size, and re-encoded as JPEG.
    pub mpp:             Option<f64>,
    /// JPEG quality used when resampling (--mpp).  Default 87.
    pub quality:         u8,
    /// Resampling filter used when resizing tiles (--mpp).  Default Nearest.
    pub filter:          FilterType,
    /// When true, use the parent directory name of the DICOM files as the
    /// output filename instead of the Series Instance UID.
    pub use_parent_name: bool,
    /// When true, halve both width and height (1/4 area).
    /// JPEG tiles are decoded at 1/2 via DCT-domain scaling; JP2K uses n_reduce=1.
    /// Mutually exclusive with --mpp.
    pub half:            bool,
    /// Apply ICC color profile to pixel data, converting to sRGB.
    /// The ICC profile tag is omitted from the output.
    pub icc_bake:        bool,
}

impl Args {
    pub fn build(args: impl Iterator<Item = String>) -> Result<Args, &'static str> {
        let all: Vec<String> = args.collect();
        let legacy          = all.iter().any(|a| a == "--legacy");
        let verbose         = all.iter().any(|a| a == "-v" || a == "--verbose");
        let use_parent_name = all.iter().any(|a| a == "--use-parent-name");
        let half            = all.iter().any(|a| a == "--half");
        let icc_bake        = all.iter().any(|a| a == "--icc-bake");

        // Parse --jobs N or -j N
        let jobs = all.windows(2).find_map(|w| {
            if w[0] == "--jobs" || w[0] == "-j" {
                w[1].parse::<usize>().ok()
            } else {
                None
            }
        });

        // Parse --mpp N
        let mpp = all.windows(2).find_map(|w| {
            if w[0] == "--mpp" {
                w[1].parse::<f64>().ok()
            } else {
                None
            }
        });

        // Parse --quality N (default 87)
        let quality = all.windows(2).find_map(|w| {
            if w[0] == "--quality" {
                w[1].parse::<u8>().ok()
            } else {
                None
            }
        }).unwrap_or(87);

        // Parse --filter NAME (default: nearest)
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

        // Collect positional args, skipping flags and their values
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
        let input_dir  = positional.first().ok_or("Didn't get an input directory path")?.to_string();
        let output_dir = positional.get(1).ok_or("Didn't get an output directory path")?.to_string();
        if half && mpp.is_some() {
            return Err("--half and --mpp are mutually exclusive");
        }
        Ok(Args { input_dir, output_dir, legacy, verbose, jobs, mpp, quality, filter,
                  use_parent_name, half, icc_bake })
    }
}
