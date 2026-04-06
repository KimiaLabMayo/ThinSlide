// WSI dicom to tiff/svs converter
// Convert the whole slide image dicom files to a single pyramidal OME-TIFF (default) or
// legacy format (SVS / generic BigTIFF) when --legacy is passed.

#[allow(non_upper_case_globals)]
#[allow(non_camel_case_types)]
#[allow(non_snake_case)]
mod bindings;
use bindings::{
    TIFF, TIFFOpen, TIFFSetField, TIFFWriteRawTile, TIFFWriteDirectory, TIFFClose,
    TIFFTAG_SUBFILETYPE, TIFFTAG_IMAGEWIDTH, TIFFTAG_IMAGELENGTH, TIFFTAG_TILEWIDTH,
    TIFFTAG_TILELENGTH, TIFFTAG_COMPRESSION, TIFFTAG_PHOTOMETRIC, TIFFTAG_SAMPLESPERPIXEL,
    TIFFTAG_BITSPERSAMPLE, TIFFTAG_SAMPLEFORMAT, TIFFTAG_PLANARCONFIG, TIFFTAG_ORIENTATION,
    TIFFTAG_RESOLUTIONUNIT, TIFFTAG_XRESOLUTION, TIFFTAG_YRESOLUTION,
    PHOTOMETRIC_RGB, PHOTOMETRIC_YCBCR, PHOTOMETRIC_MINISBLACK,
    SAMPLEFORMAT_UINT, TIFFTAG_IMAGEDESCRIPTION, TIFFTAG_YCBCRSUBSAMPLING,
    PLANARCONFIG_CONTIG, TIFFTAG_SUBIFD, TIFFWriteRawStrip,
    ORIENTATION_TOPLEFT,
    RESUNIT_CENTIMETER,
    FILETYPE_REDUCEDIMAGE,
    TIFFTAG_ROWSPERSTRIP,
    TIFFTAG_ICCPROFILE,
};

use std::ffi::CString;
use std::os::raw::c_void;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::time::Duration;
use rayon::prelude::*;
use walkdir::WalkDir;
use dicom_pixeldata::PixelDecoder;
use image::imageops::FilterType;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use std::path::Path;

const PWSI_CLASS_UID: &str = "1.2.840.10008.5.1.4.1.1.77.1.6";

// SVS (Aperio) JPEG 2000 proprietary compression codes recognized by OpenSlide
const COMPRESSION_APERIO_JP2_YCBCR: u32 = 33003;
const COMPRESSION_APERIO_JP2_RGB: u32 = 33005;

/// Minimum length of the longer image side (pixels) required to include a
/// pyramid level in the resampled output.  Levels below this threshold are
/// skipped because they add no useful detail at typical screen resolutions.
const MIN_PYRAMID_SIDE: u32 = 512;

// ─── Compression type ───────────────────────────────────────────────────────

enum CompressionType {
    JpegBaseline,
    JpegExtended,
    JpegLossless,
    JpegLosslessNonHierarchical,
    JpegLSLossless,
    JpegLSNearLossless,
    Jpeg2000Lossless,
    Jpeg2000,
    Jpeg2000Part2MulticomponentLossless,
    Jpeg2000Part2Multicomponent,
    Unknown,
}

fn map_transfer_syntax_to_compression(transfer_syntax_uid: &str) -> CompressionType {
    match transfer_syntax_uid {
        "1.2.840.10008.1.2.4.50" => CompressionType::JpegBaseline,
        "1.2.840.10008.1.2.4.51" => CompressionType::JpegExtended,
        "1.2.840.10008.1.2.4.57" => CompressionType::JpegLossless,
        "1.2.840.10008.1.2.4.70" => CompressionType::JpegLosslessNonHierarchical,
        "1.2.840.10008.1.2.4.80" => CompressionType::JpegLSLossless,
        "1.2.840.10008.1.2.4.81" => CompressionType::JpegLSNearLossless,
        "1.2.840.10008.1.2.4.90" => CompressionType::Jpeg2000Lossless,
        "1.2.840.10008.1.2.4.91" => CompressionType::Jpeg2000,
        "1.2.840.10008.1.2.4.92" => CompressionType::Jpeg2000Part2MulticomponentLossless,
        "1.2.840.10008.1.2.4.93" => CompressionType::Jpeg2000Part2Multicomponent,
        _ => CompressionType::Unknown,
    }
}

fn is_jpeg2000(comp: &CompressionType) -> bool {
    matches!(comp,
        CompressionType::Jpeg2000
        | CompressionType::Jpeg2000Lossless
        | CompressionType::Jpeg2000Part2Multicomponent
        | CompressionType::Jpeg2000Part2MulticomponentLossless
    )
}

// ─── Color space ─────────────────────────────────────────────────────────────

#[derive(PartialEq)]
enum ColorSpace {
    RGB,
    YCbCr,
    Grayscale,
}

// implement std::fmt::Display for ColorSpace {
impl std::fmt::Display for ColorSpace {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        let s = match self {
            ColorSpace::RGB => "RGB",
            ColorSpace::YCbCr => "YCbCr",
            ColorSpace::Grayscale => "Grayscale",
        };
        write!(f, "{}", s)
    }
}


fn find_app14_marker(data: &[u8]) -> Option<u8> {
    // APP14 Adobe structure:
    //   +0,1:  marker 0xFF 0xEE
    //   +2,3:  length (big-endian, includes these 2 bytes)
    //   +4-8:  "Adobe" (5 bytes)
    //   +9,10: DCTEncodeVersion
    //   +11,12: Flags0
    //   +13,14: Flags1
    //   +15:   ColorTransform  ← 0=RGB/unknown, 1=YCbCr, 2=YCCK
    if let Some(idx) = data.windows(2).position(|w| w == [0xFF, 0xEE]) {
        if data.len() < idx + 16 {
            return None;
        }
        if &data[idx + 4..idx + 9] == b"Adobe" {
            let color_transform = data[idx + 15];
            if color_transform <= 2 {
                return Some(color_transform);
            }
        }
    }
    None
}

fn infer_color_space(dcm: &dicom::object::DefaultDicomObject) -> ColorSpace {
    // 1. DICOM PhotometricInterpretation tag (most reliable)
    if let Ok(elem) = dcm.element_by_name("PhotometricInterpretation") {
        if let Ok(s) = elem.to_str() {
            match s.trim() {
                "RGB" => return ColorSpace::RGB,
                // YBR_ICT / YBR_RCT are JPEG 2000 internal transforms;
                // the *decoded* output is RGB, so SVS photometric = RGB.
                "YBR_ICT" | "YBR_RCT" => return ColorSpace::RGB,
                "YBR_FULL" | "YBR_FULL_422" => return ColorSpace::YCbCr,
                "MONOCHROME1" | "MONOCHROME2" => return ColorSpace::Grayscale,
                _ => {}
            }
        }
    }

    // 2. APP14 marker in first JPEG fragment
    if let Ok(px) = dcm.element_by_name("PixelData") {
        if let Some(fragments) = px.fragments() {
            if let Some(first) = fragments.iter().next() {
                if let Some(ct) = find_app14_marker(first) {
                    return if ct == 0 { ColorSpace::RGB } else { ColorSpace::YCbCr };
                }
            }
        }
    }

    // 3. SamplesPerPixel fallback
    if let Ok(elem) = dcm.element_by_name("SamplesPerPixel") {
        if let Ok(s) = elem.to_str() {
            if s.trim() == "1" {
                return ColorSpace::Grayscale;
            }
        }
    }

    // JPEG default for 3-component data is YCbCr
    ColorSpace::YCbCr
}

/// Extract the ICC profile bytes from DICOM Tag (0028,2000), if present.
fn extract_icc_profile(dcm: &dicom::object::DefaultDicomObject) -> Option<Vec<u8>> {
    use dicom_core::Tag;
    dcm.element(Tag(0x0028, 0x2000)).ok()
        .and_then(|e| e.to_bytes().ok())
        .map(|b| b.into_owned())
}

// ─── Metadata ────────────────────────────────────────────────────────────────

#[derive(Clone)]
struct DcmMetadata {
    sop_class_uid: String,
    series_instance_uid: String,
    modality: String,
    transfer_syntax_uid: String,
    n_frames: Option<u32>,
    /// Total pixel matrix dimensions
    px_columns: Option<u32>,
    px_rows: Option<u32>,
    file_path: String,
    image_type: Option<String>,
    /// tile_size = (width, height) i.e. (Columns, Rows)
    tile_size: Option<(u32, u32)>,
    /// Microns per pixel (x / y)
    mpp_x: Option<f64>,
    mpp_y: Option<f64>,
    /// Objective lens power (magnification)
    objective_power: Option<f64>,
}

impl std::fmt::Display for DcmMetadata {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f,
            "SOP Class UID: {}\nSeries: {}\nModality: {}\nFrames: {:?}\n\
             Size: {:?}x{:?}  Tile: {:?}  MPP: {:?}",
            self.sop_class_uid, self.series_instance_uid, self.modality,
            self.n_frames, self.px_columns, self.px_rows,
            self.tile_size, self.mpp_x)
    }
}

fn extract_metadata(dcm_path: &str) -> DcmMetadata {
    let dcm = dicom::object::open_file(dcm_path).unwrap();

    let get_str = |name: &str| -> String {
        dcm.element_by_name(name)
            .map(|e| e.to_str().unwrap_or_default().to_string())
            .unwrap_or_default()
    };
    let get_u32 = |name: &str| -> Option<u32> {
        dcm.element_by_name(name).ok()
            .and_then(|e| e.to_str().ok().and_then(|s| s.trim().parse().ok()))
    };
    let get_f64 = |name: &str| -> Option<f64> {
        dcm.element_by_name(name).ok()
            .and_then(|e| e.to_str().ok().and_then(|s| s.trim().parse().ok()))
    };

    let sop_class_uid      = get_str("SOPClassUID");
    let series_instance_uid= get_str("SeriesInstanceUID");
    let modality           = get_str("Modality");
    let transfer_syntax_uid= dcm.meta().transfer_syntax().to_string();

    let n_frames   = get_u32("NumberOfFrames");
    let px_columns = get_u32("TotalPixelMatrixColumns");
    let px_rows    = get_u32("TotalPixelMatrixRows");

    let image_type = dcm.element_by_name("ImageType").ok()
        .and_then(|e| e.to_str().ok())
        .map(|s| s.to_string());

    // tile_size = (width=Columns, height=Rows)
    let tile_width  = get_u32("Columns");
    let tile_height = get_u32("Rows");
    let tile_size = match (tile_width, tile_height) {
        (Some(w), Some(h)) => Some((w, h)),
        _ => None,
    };

    // MPP from ImagedVolumeWidth/Height [mm] / TotalPixelMatrix [px] * 1000 → µm/px
    let vol_width_mm  = get_f64("ImagedVolumeWidth");
    let vol_height_mm = get_f64("ImagedVolumeHeight");
    let mpp_x = vol_width_mm.zip(px_columns)
        .map(|(w, c)| w * 1000.0 / c as f64);
    let mpp_y = vol_height_mm.zip(px_rows)
        .map(|(h, r)| h * 1000.0 / r as f64);

    // Fallback: PixelSpacing [mm/px] * 1000 → µm/px
    let (mpp_x, mpp_y) = if mpp_x.is_none() || mpp_y.is_none() {
        let ps = dcm.element_by_name("PixelSpacing").ok()
            .and_then(|e| e.to_str().ok().map(|s| s.to_string()));
        if let Some(ps_str) = ps {
            let parts: Vec<f64> = ps_str.split('\\')
                .filter_map(|s| s.trim().parse().ok())
                .collect();
            if parts.len() >= 2 {
                (Some(parts[1] * 1000.0), Some(parts[0] * 1000.0))
            } else {
                (mpp_x, mpp_y)
            }
        } else {
            (mpp_x, mpp_y)
        }
    } else {
        (mpp_x, mpp_y)
    };

    let objective_power = get_f64("ObjectiveLensPower");

    DcmMetadata {
        sop_class_uid,
        series_instance_uid,
        modality,
        transfer_syntax_uid,
        n_frames,
        px_columns,
        px_rows,
        file_path: dcm_path.to_string(),
        image_type,
        tile_size,
        mpp_x,
        mpp_y,
        objective_power,
    }
}

// ─── Helpers ─────────────────────────────────────────────────────────────────

fn is_wsi_dicom(m: &DcmMetadata) -> bool {
    m.sop_class_uid == PWSI_CLASS_UID && m.modality == "SM"
}

fn get_thumbnail_obj<'a>(list: &'a [DcmMetadata]) -> Option<&'a DcmMetadata> {
    list.iter().find(|m| m.image_type.as_deref().unwrap_or("").contains("THUMBNAIL"))
}
fn get_overview_obj<'a>(list: &'a [DcmMetadata]) -> Option<&'a DcmMetadata> {
    list.iter().find(|m| m.image_type.as_deref().unwrap_or("").contains("OVERVIEW"))
}
fn get_label_obj<'a>(list: &'a [DcmMetadata]) -> Option<&'a DcmMetadata> {
    list.iter().find(|m| m.image_type.as_deref().unwrap_or("").contains("LABEL"))
}

fn get_slide_level_obj(list: &[DcmMetadata]) -> Option<Vec<&DcmMetadata>> {
    let mut v: Vec<&DcmMetadata> = list.iter()
        .filter(|m| m.image_type.as_deref().unwrap_or("").contains("VOLUME"))
        .filter(|m| {
            // skip if IFD size is smaller than tile size
            let ifd_w = m.px_columns.unwrap_or(0);
            let ifd_h = m.px_rows.unwrap_or(0);
            let (tile_w, tile_h) = m.tile_size.unwrap_or((ifd_w, ifd_h));
            ifd_w >= tile_w && ifd_h >= tile_h
        })
        .collect();
    v.sort_by(|a, b| b.px_columns.cmp(&a.px_columns));
    if v.is_empty() { None } else { Some(v) }
}


/// Decode a single DICOM frame and encode it as a JPEG byte stream.
/// Returns (jpeg_bytes, width, height).
/// Only used for SVS thumbnail/label/overview IFDs, which require a self-contained JPEG stream (no raw tiles).
fn decode_frame_as_jpeg(dcm_path: &str, frame: u32, _quality: u8) -> Option<(Vec<u8>, u32, u32)> {
    let dcm = dicom::object::open_file(dcm_path).ok()?;
    let decoded = dcm.decode_pixel_data_frame(frame).ok()?;
    let img = decoded.to_dynamic_image(0).ok()?;
    let w = img.width();
    let h = img.height();
    let mut bytes = Vec::new();
    img.write_to(
        &mut std::io::Cursor::new(&mut bytes),
        image::ImageFormat::Jpeg,
    ).ok()?;
    Some((bytes, w, h))
}

// ─── Args ────────────────────────────────────────────────────────────────────

pub struct Args {
    pub input_dir:  String,
    pub output_dir: String,
    pub legacy:     bool,
    pub verbose:    bool,
    pub jobs:       Option<usize>,
    /// Target resolution in microns-per-pixel.  When set, tiles are decoded,
    /// resampled to the nearest valid tile size, and re-encoded as JPEG.
    pub mpp:        Option<f64>,
    /// JPEG quality used when resampling (--mpp).  Default 87.
    pub quality:    u8,
    /// Resampling filter used when resizing tiles (--mpp).  Default Nearest.
    pub filter:          FilterType,
    /// When true, use the parent directory name of the DICOM files as the
    /// output filename instead of the Series Instance UID.
    pub use_parent_name: bool,
}

impl Args {
    pub fn build(args: impl Iterator<Item = String>) -> Result<Args, &'static str> {
        let all: Vec<String> = args.collect();
        let legacy          = all.iter().any(|a| a == "--legacy");
        let verbose         = all.iter().any(|a| a == "-v" || a == "--verbose");
        let use_parent_name = all.iter().any(|a| a == "--use-parent-name");

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
                    "nearest"              => Some(FilterType::Nearest),
                    "triangle" | "bilinear"=> Some(FilterType::Triangle),
                    "catmullrom"| "bicubic"=> Some(FilterType::CatmullRom),
                    "gaussian"             => Some(FilterType::Gaussian),
                    "lanczos3"             => Some(FilterType::Lanczos3),
                    _                      => None,
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
        let input_dir  = positional.get(0).ok_or("Didn't get an input directory path")?.to_string();
        let output_dir = positional.get(1).ok_or("Didn't get an output directory path")?.to_string();
        Ok(Args { input_dir, output_dir, legacy, verbose, jobs, mpp, quality, filter, use_parent_name })
    }
}

// ─── JPEG 2000 MCT detection ─────────────────────────────────────────────────

/// Returns true when the J2K main-header COD marker has the Multiple Component
/// Transform (MCT) flag set (SGcod byte 2 != 0).
///
/// When MCT is set, OpenJPEG applies the inverse ICT/RCT during decoding and
/// the returned pixels are already in RGB — no manual colour conversion needed.
/// When MCT is clear, OpenJPEG returns the raw transformed (YCbCr-like) values
/// and the caller must apply the inverse transform manually.
///
/// Some DICOM encoders encode YBR_ICT frames with MCT=1 in the COD marker
/// (letting OpenJPEG reverse it) while other frames in the same series have
/// MCT=0 (requiring manual reversal).  Checking per-frame avoids the mosaic
/// pattern that results from applying the conversion unconditionally.
fn j2k_mct_is_set(data: &[u8]) -> bool {
    // J2K codestream layout: SOC (FF 4F) followed by marker segments.
    // Each segment: FF <code> <Lseg-hi> <Lseg-lo> <payload...>
    // Lseg includes the 2 length bytes but NOT the 2-byte marker itself.
    // COD marker = FF 52; SOT marker = FF 90 (end of main header).
    if data.len() < 2 { return false; }
    // Skip SOC (FF 4F) if present.
    let mut i: usize = if data[0] == 0xFF && data[1] == 0x4F { 2 } else { 0 };
    while i + 1 < data.len() {
        if data[i] != 0xFF {
            i += 1;
            continue;
        }
        let code = data[i + 1];
        // SOT marks the end of the main header; COD will not appear after it.
        if code == 0x90 { break; }
        // COD: FF 52 | Lcod(2) | Scod(1) | SGcod: prog(1) layers(2) MCT(1)
        // MCT flag is at byte offset 8 from the start of the FF marker byte.
        if code == 0x52 {
            return i + 8 < data.len() && data[i + 8] != 0;
        }
        // Skip over this marker segment using its length field.
        if i + 3 < data.len() {
            let seg_len = u16::from_be_bytes([data[i + 2], data[i + 3]]) as usize;
            i += 2 + seg_len;
        } else {
            break;
        }
    }
    false
}

// ─── JPEG subsampling detection ──────────────────────────────────────────────

/// Parse a JPEG byte stream and return the YCbCr chroma subsampling factors
/// as (horiz, vert) for TIFF's YCbCrSubSampling tag.
/// E.g. 4:2:2 → (2, 1), 4:2:0 → (2, 2), 4:4:4 → (1, 1).
fn detect_jpeg_subsampling(data: &[u8]) -> Option<(u16, u16)> {
    if data.len() < 2 || data[0] != 0xFF || data[1] != 0xD8 {
        return None;
    }
    let mut i = 2usize;
    while i + 1 < data.len() {
        if data[i] != 0xFF {
            return None;
        }
        let marker = data[i + 1];
        i += 2;
        // SOF0 (baseline), SOF1 (extended sequential), SOF2 (progressive)
        if matches!(marker, 0xC0 | 0xC1 | 0xC2) {
            // Segment layout at i:
            //   [0..2]  length (includes these 2 bytes)
            //   [2]     sample precision
            //   [3..5]  height
            //   [5..7]  width
            //   [7]     number of components
            //   [8..]   3 bytes per component: id, sampling_factors (H<<4|V), qt_selector
            if i + 13 > data.len() {
                return None;
            }
            let ncomp = data[i + 7] as usize;
            if ncomp < 2 || i + 8 + ncomp * 3 > data.len() {
                return None;
            }
            let y_h  = (data[i + 9] >> 4) as u16;
            let y_v  = (data[i + 9] & 0x0F) as u16;
            let cb_h = (data[i + 12] >> 4) as u16;
            let cb_v = (data[i + 12] & 0x0F) as u16;
            if cb_h == 0 || cb_v == 0 {
                return None;
            }
            return Some((y_h / cb_h, y_v / cb_v));
        } else if marker == 0xD9 {
            break; // EOI
        } else if matches!(marker, 0xD0..=0xD8 | 0x01) {
            continue; // standalone markers (no length field)
        } else {
            if i + 2 > data.len() {
                return None;
            }
            let len = u16::from_be_bytes([data[i], data[i + 1]]) as usize;
            if len < 2 {
                return None;
            }
            i += len;
        }
    }
    None
}

// ─── TIFF tile index from DICOM frame position ────────────────────────────────

/// Compute the TIFF tile number for each DICOM frame by reading
/// PerFrameFunctionalGroupsSequence → PlanePositionSlideSequence.
/// Falls back to sequential numbering when position info is absent.
fn frame_to_tile_indices(
    dcm: &dicom::object::DefaultDicomObject,
    tile_w: u32,
    tile_h: u32,
    image_w: u32,
) -> Vec<u32> {
    let tiles_across = (image_w + tile_w - 1) / tile_w;

    let from_seq = (|| -> Option<Vec<u32>> {
        let pfg    = dcm.element_by_name("PerFrameFunctionalGroupsSequence").ok()?;
        let frames = pfg.items()?;

        frames.iter().map(|frame| {
            let pps       = frame.element_by_name("PlanePositionSlideSequence").ok()?;
            let pps_items = pps.items()?;
            let pos       = pps_items.first()?;

            let col: i32 = pos.element_by_name("ColumnPositionInTotalImagePixelMatrix").ok()
                .and_then(|e| e.to_str().ok())
                .and_then(|s| s.trim().parse().ok())
                .unwrap_or(1);
            let row: i32 = pos.element_by_name("RowPositionInTotalImagePixelMatrix").ok()
                .and_then(|e| e.to_str().ok())
                .and_then(|s| s.trim().parse().ok())
                .unwrap_or(1);

            let col_tile = (col.max(1) as u32 - 1) / tile_w;
            let row_tile = (row.max(1) as u32 - 1) / tile_h;
            Some(row_tile * tiles_across + col_tile)
        }).collect()
    })();

    from_seq.unwrap_or_else(|| {
        let n = dcm.element_by_name("NumberOfFrames").ok()
            .and_then(|e| e.to_str().ok())
            .and_then(|s| s.trim().parse::<u32>().ok())
            .unwrap_or(0);
        (0..n).collect()
    })
}

// ─── Generic pyramidal TIFF writer (JPEG-compressed DICOM) ───────────────────

fn write_flat_multipage_tiff(
    slide_level_metadata_list: &[DcmMetadata],
    output_path: &str,
    pb: Option<&ProgressBar>,
) {
    // A single resolution level may be split across multiple DICOM SOP instances
    // (common for large slides). Group consecutive entries that share the same
    // total pixel matrix dimensions so they map to a single TIFF IFD.
    let mut groups: Vec<Vec<&DcmMetadata>> = Vec::new();
    for meta in slide_level_metadata_list {
        if let Some(last) = groups.last_mut() {
            if last[0].px_columns == meta.px_columns && last[0].px_rows == meta.px_rows {
                last.push(meta);
                continue;
            }
        }
        groups.push(vec![meta]);
    }
    let total_tiles: u64 = groups.iter()
        .filter(|g| g[0].tile_size.is_some())
        .map(|g| g.iter().map(|m| m.n_frames.unwrap_or(0) as u64).sum::<u64>())
        .sum();
    if let Some(p) = pb { p.set_length(total_tiles); }

    unsafe {
        let tiff = TIFFOpen(
            CString::new(output_path).unwrap().as_ptr(),
            CString::new("w8").unwrap().as_ptr(),
        );

        for (group_idx, group) in groups.iter().enumerate() {
            let metadata  = group[0];
            // Skip resolution levels where the entire image fits in a single tile
            // (tile_size == None means tile dimensions equal image dimensions).
            // Such levels provide no pyramid benefit and can have non-16-aligned
            // dimensions that libtiff rejects for JPEG tiles.
            if metadata.tile_size.is_none() {
                eprintln!("  [skip] IFD {} ({}x{}): no tile size — single-tile level omitted",
                    group_idx,
                    metadata.px_columns.unwrap_or(0),
                    metadata.px_rows.unwrap_or(0));
                continue;
            }
            let first_dcm = dicom::object::open_file(&metadata.file_path).unwrap();

            let ifd_width  = metadata.px_columns.unwrap_or(0);
            let ifd_height = metadata.px_rows.unwrap_or(0);
            let (tile_w, tile_h) = metadata.tile_size.unwrap_or((ifd_width, ifd_height));

            let color_space = infer_color_space(&first_dcm);
            let photometric = match color_space {
                ColorSpace::RGB       => PHOTOMETRIC_RGB,
                ColorSpace::YCbCr     => PHOTOMETRIC_YCBCR,
                ColorSpace::Grayscale => PHOTOMETRIC_MINISBLACK,
            };

            let ts_uid = first_dcm.meta().transfer_syntax();
            let compression = match map_transfer_syntax_to_compression(ts_uid) {
                CompressionType::JpegBaseline                        => 7u32,
                CompressionType::JpegExtended                        => 7,
                CompressionType::JpegLossless                        => 7,
                CompressionType::JpegLosslessNonHierarchical         => 7,
                CompressionType::JpegLSLossless                      => 34892,
                CompressionType::JpegLSNearLossless                  => 34892,
                CompressionType::Jpeg2000Lossless                    => 34712,
                CompressionType::Jpeg2000                            => 34712,
                CompressionType::Jpeg2000Part2MulticomponentLossless => 34712,
                CompressionType::Jpeg2000Part2Multicomponent         => 34712,
                CompressionType::Unknown                             => 34712,
            };

            // For JPEG passthrough, libtiff checks that the JPEG SOF header dimensions match the TIFF tile declaration.
            // If tiles are not multiples of 16, this mismatch causes an error, so we skip these levels.
            if compression == 7 && !is_jpeg_tile_aligned(tile_w, tile_h) {
                eprintln!("  [skip] IFD {} ({}x{}): tile {}x{} not 16-aligned for JPEG — omitted",
                    group_idx, ifd_width, ifd_height, tile_w, tile_h);
                continue;
            }

            let mpp = metadata.mpp_x.unwrap_or(0.25);
            let res = 1e4 / mpp;

            let subfile_type: u32 = if group_idx == 0 { 0 } else { FILETYPE_REDUCEDIMAGE };
            TIFFSetField(tiff, TIFFTAG_SUBFILETYPE as u32,     subfile_type);
            TIFFSetField(tiff, TIFFTAG_IMAGEWIDTH as u32,      ifd_width);
            TIFFSetField(tiff, TIFFTAG_IMAGELENGTH as u32,     ifd_height);
            TIFFSetField(tiff, TIFFTAG_TILEWIDTH as u32,       tile_align(tile_w, 16));
            TIFFSetField(tiff, TIFFTAG_TILELENGTH as u32,      tile_align(tile_h, 16));
            TIFFSetField(tiff, TIFFTAG_COMPRESSION as u32,     compression);
            TIFFSetField(tiff, TIFFTAG_PHOTOMETRIC as u32,     photometric);
            TIFFSetField(tiff, TIFFTAG_SAMPLESPERPIXEL as u32, 3u32);
            TIFFSetField(tiff, TIFFTAG_BITSPERSAMPLE as u32,   8u32);
            TIFFSetField(tiff, TIFFTAG_SAMPLEFORMAT as u32,    SAMPLEFORMAT_UINT as u32);
            TIFFSetField(tiff, TIFFTAG_PLANARCONFIG as u32,    PLANARCONFIG_CONTIG as u32);
            TIFFSetField(tiff, TIFFTAG_ORIENTATION as u32,     ORIENTATION_TOPLEFT as u32);
            TIFFSetField(tiff, TIFFTAG_RESOLUTIONUNIT as u32,  RESUNIT_CENTIMETER as u32);
            TIFFSetField(tiff, TIFFTAG_XRESOLUTION as u32,     res);
            TIFFSetField(tiff, TIFFTAG_YRESOLUTION as u32,     res);

            // For YCbCr JPEG, detect actual subsampling from the first tile so
            // the TIFF tag matches the JPEG payload (otherwise libtiff defaults
            // to [2,2] regardless of the stream content, confusing QuPath).
            if matches!(color_space, ColorSpace::YCbCr) {
                let px_tmp   = first_dcm.element_by_name("PixelData").expect("No PixelData");
                let frags_tmp = px_tmp.fragments().expect("Not encapsulated pixel data");
                if let Some(first) = frags_tmp.iter().find(|f| !f.is_empty()) {
                    if let Some((h, v)) = detect_jpeg_subsampling(first) {
                        TIFFSetField(tiff, TIFFTAG_YCBCRSUBSAMPLING as u32, h as u32, v as u32);
                    }
                }
            }

            // Embed ICC profile from DICOM Tag (0028,2000) if present (first IFD only).
            if group_idx == 0 {
                let icc_profile = extract_icc_profile(&first_dcm);
                if let Some(ref icc) = icc_profile {
                    TIFFSetField(tiff, TIFFTAG_ICCPROFILE as u32,
                        icc.len() as u32, icc.as_ptr() as *const c_void);
                }
            }

            drop(first_dcm);

            // Write tiles from every DICOM file in this resolution group, placing
            // each tile at its correct position within the shared pixel matrix.
            for dcm_meta in group.iter() {
                let dicom_obj   = dicom::object::open_file(&dcm_meta.file_path).unwrap();
                let tile_indices = frame_to_tile_indices(&dicom_obj, tile_w, tile_h, ifd_width);
                let px_elem     = dicom_obj.element_by_name("PixelData").expect("No PixelData");
                let fragments   = px_elem.fragments().expect("Not encapsulated pixel data");

                for (frag_idx, fragment) in fragments.iter().enumerate() {
                    if !fragment.is_empty() {
                        let tile_num = tile_indices.get(frag_idx).copied()
                            .unwrap_or(frag_idx as u32);
                        TIFFWriteRawTile(
                            tiff, tile_num,
                            fragment.as_ptr() as *mut c_void,
                            fragment.len() as i64,
                        );
                    }
                }
            }

            TIFFWriteDirectory(tiff);
            let group_tiles: u64 = group.iter().map(|m| m.n_frames.unwrap_or(0) as u64).sum();
            if let Some(p) = pb { p.inc(group_tiles); }
        }
        TIFFClose(tiff);
    }
}

/// Generate a deterministic UUID (version 4 format) from a DICOM UID string.
fn uid_to_uuid(uid: &str) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let mut h = DefaultHasher::new();
    uid.hash(&mut h);
    let a = h.finish();

    let mut h2 = DefaultHasher::new();
    a.hash(&mut h2);
    uid.len().hash(&mut h2);
    let b = h2.finish();

    format!(
        "{:08x}-{:04x}-4{:03x}-{:04x}-{:012x}",
        (a >> 32) as u32,
        (a >> 16) as u16,
        a as u16 & 0x0FFF,
        ((b >> 48) as u16 & 0x3FFF) | 0x8000,
        b & 0x0000_FFFF_FFFF_FFFF_u64,
    )
}

/// Build a conforming OME-XML string (schema 2016-06) for the full-resolution image.
///
/// The returned XML is placed in the ImageDescription tag of IFD 0.  BioFormats
/// identifies a file as OME-TIFF by the presence of this block and uses it to
/// determine pixel dimensions, physical pixel size, and channel layout.
///
/// Structure decisions
/// -------------------
/// * A single `<Image>` / `<Pixels>` block describes the full-resolution plane
///   (IFD 0, `TiffData IFD="0"`).
/// * Reduced-resolution pyramid IFDs carry `SUBFILETYPE=REDUCEDIMAGE`; BioFormats
///   detects those automatically without additional `<Image>` entries.
/// * For RGB/YCbCr images one `<Channel SamplesPerPixel="3">` is emitted with
///   `Interleaved="true"`, matching the JPEG interleaved storage layout.
/// * Physical pixel size is expressed in µm (OME default unit).
/// Round `v` up to the nearest multiple of `align`.
/// libtiff requires JPEG tile dimensions to be multiples of 16 (YCbCr MCU boundary).
/// Applying this universally is safe: for other compressions it has no side-effects.
fn tile_align(v: u32, align: u32) -> u32 {
    (v + align - 1) / align * align
}

/// Check if the JPEG tile dimensions are multiples of 16, as required by libtiff for pass-through tiles.
/// When using `TIFFWriteRawTile` for pass-through, libtiff checks that the JPEG SOF header dimensions match 
/// the TIFF tile declaration (after tile_align). If the JPEG dimensions are not multiples of 16, this mismatch 
/// causes a "Bad value N for tileWidth/tileLength tag" error.
/// 
fn is_jpeg_tile_aligned(tile_w: u32, tile_h: u32) -> bool {
    tile_w % 16 == 0 && tile_h % 16 == 0
}

/// Round `v` to the nearest multiple of 16, with a minimum of 16.
/// Used to compute the output tile size for resampled TIFF writing so that
/// JPEG tiles always satisfy libtiff's MCU-boundary requirement.
fn nearest_16(v: f64) -> u32 {
    ((v / 16.0).round() as u32).max(1) * 16
}

/// Escape special XML characters in an attribute value.
fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
     .replace('<', "&lt;")
     .replace('>', "&gt;")
     .replace('"', "&quot;")
}

#[allow(non_snake_case)]
fn generate_OME_XML(metadata_list: &[DcmMetadata]) -> String {
    let base   = &metadata_list[0];
    let width  = base.px_columns.unwrap_or(0);
    let height = base.px_rows.unwrap_or(0);
    let mpp_x  = base.mpp_x.unwrap_or(0.25);
    let mpp_y  = base.mpp_y.unwrap_or(mpp_x);
    let uuid   = uid_to_uuid(&base.series_instance_uid);
    let name   = &base.series_instance_uid;

    // Read SamplesPerPixel, BitsAllocated, and Manufacturer from the DICOM file.
    let dcm = dicom::object::open_file(&base.file_path).ok();
    let spp: u32 = dcm.as_ref()
        .and_then(|d| d.element_by_name("SamplesPerPixel").ok())
        .and_then(|e| e.to_str().ok().and_then(|s| s.trim().parse().ok()))
        .unwrap_or(3);
    let bps: u32 = dcm.as_ref()
        .and_then(|d| d.element_by_name("BitsAllocated").ok())
        .and_then(|e| e.to_str().ok().and_then(|s| s.trim().parse().ok()))
        .unwrap_or(8);

    // DICOM Tag (0008,0070): scanner vendor name.
    let manufacturer: Option<String> = dcm.as_ref()
        .and_then(|d| d.element_by_name("Manufacturer").ok())
        .and_then(|e| e.to_str().ok().map(|s| s.trim().to_string()))
        .filter(|s| !s.is_empty());

    // OME pixel type string
    let pixel_type = match (bps, spp) {
        (8,  _) => "uint8",
        (16, _) => "uint16",
        (32, _) => "uint32",
        _       => "uint8",
    };

    // For interleaved colour (RGB / YCbCr) the OME convention used by BioFormats
    // is: SizeC = SamplesPerPixel, one <Channel> element with SamplesPerPixel,
    // Interleaved="true".  For grayscale: SizeC=1, SamplesPerPixel=1, no interleave.
    let (size_c, channel_spp, interleaved) = if spp >= 3 {
        (spp, spp, "true")
    } else {
        (1u32, 1u32, "false")
    };

    // Optional <Instrument> block and back-reference from <Image>.
    // Per OME 2016-06 schema: <Instrument> must precede <Image> in the root;
    // <InstrumentRef> must precede <Pixels> inside <Image>.
    let (instrument_block, instrument_ref) = match manufacturer {
        Some(ref mfr) => (
            format!(
                "  <Instrument ID=\"Instrument:0\">\n    <Microscope Manufacturer=\"{}\"/>\n  </Instrument>\n",
                xml_escape(mfr)
            ),
            "    <InstrumentRef ID=\"Instrument:0\"/>\n".to_string(),
        ),
        None => (String::new(), String::new()),
    };

    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<OME xmlns="http://www.openmicroscopy.org/Schemas/OME/2016-06"
     xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance"
     xsi:schemaLocation="http://www.openmicroscopy.org/Schemas/OME/2016-06 http://www.openmicroscopy.org/Schemas/OME/2016-06/ome.xsd"
     UUID="urn:uuid:{uuid}">
{instrument_block}  <Image ID="Image:0" Name="{name}">
{instrument_ref}    <Pixels ID="Pixels:0"
            DimensionOrder="XYZCT"
            Type="{pixel_type}"
            SizeX="{width}"
            SizeY="{height}"
            SizeZ="1"
            SizeC="1"
            SizeT="1"
            PhysicalSizeX="{mpp_x:.6}"
            PhysicalSizeXUnit="µm"
            PhysicalSizeY="{mpp_y:.6}"
            PhysicalSizeYUnit="µm"
            Interleaved="{interleaved}">
      <Channel ID="Channel:0:0" SamplesPerPixel="{channel_spp}">
        <LightPath/>
      </Channel>
      <TiffData FirstC="0" FirstT="0" FirstZ="0" IFD="0" PlaneCount="1"/>
    </Pixels>
  </Image>
</OME>"#
    )
}


// ─── compression tag from transfer syntax ─────────────────────────────────────

fn tiff_compression_tag(ts_uid: &str) -> u32 {
    match map_transfer_syntax_to_compression(ts_uid) {
        CompressionType::JpegBaseline                        => 7,
        CompressionType::JpegExtended                        => 7,
        CompressionType::JpegLossless                        => 7,
        CompressionType::JpegLosslessNonHierarchical         => 7,
        CompressionType::JpegLSLossless                      => 34892,
        CompressionType::JpegLSNearLossless                  => 34892,
        CompressionType::Jpeg2000Lossless                    => 34712,
        CompressionType::Jpeg2000                            => 34712,
        CompressionType::Jpeg2000Part2MulticomponentLossless => 34712,
        CompressionType::Jpeg2000Part2Multicomponent         => 34712,
        CompressionType::Unknown                             => 34712,
    }
}

// ─── OME-TIFF writer ─────────────────────────────────────────────────────────
//
// IFD layout
// ----------
//   IFD 0  (main chain)   Full resolution; OME-XML in ImageDescription;
//                          TIFFTAG_SUBIFD lists N sub-resolution offsets;
//                          SubFileType = 0.
//   SubIFDs 0 … N-1       Pyramid sub-resolutions chained from IFD 0 via
//                          TIFFTAG_SUBIFD.  libtiff routes the next N calls
//                          to TIFFWriteDirectory() into this chain automatically.
//                          SubFileType = REDUCEDIMAGE.
//   IFD 1+ (main chain)   Optional thumbnail / label / overview images
//                          (stripped JPEG, SubFileType = REDUCEDIMAGE).
//
// Pixel data is copied verbatim from DICOM fragments — no re-encoding.
// The OME-XML conforms to the OME 2016-06 schema and is parsed by BioFormats.
fn write_ome_tiff(
    slide_level_metadata_list: &[DcmMetadata],
    _thumbnail_meta: Option<&DcmMetadata>,
    _overview_meta: Option<&DcmMetadata>,
    _label_meta: Option<&DcmMetadata>,
    output_path: &str,
    pb: Option<&ProgressBar>,
) {
    // Group consecutive DcmMetadata entries that share the same total pixel
    // matrix dimensions into a single pyramid level (multi-file SOP instances).
    let mut groups: Vec<Vec<&DcmMetadata>> = Vec::new();
    for meta in slide_level_metadata_list {
        if let Some(last) = groups.last_mut() {
            if last[0].px_columns == meta.px_columns && last[0].px_rows == meta.px_rows {
                last.push(meta);
                continue;
            }
        }
        groups.push(vec![meta]);
    }
    let total_tiles: u64 = groups.iter()
        .filter(|g| g[0].tile_size.is_some())
        .map(|g| g.iter().map(|m| m.n_frames.unwrap_or(0) as u64).sum::<u64>())
        .sum();
    if let Some(p) = pb { p.set_length(total_tiles); }

    let ome_xml      = generate_OME_XML(slide_level_metadata_list);
    let image_desc_c = CString::new(ome_xml).unwrap();

    // Number of sub-resolution levels that will be stored as SubIFDs.
    // Exclude single-tile levels (tile_size == None) as they are skipped below.
    let n_subifds = groups[1..].iter().filter(|g| g[0].tile_size.is_some()).count();

    unsafe {
        let tiff = TIFFOpen(
            CString::new(output_path).unwrap().as_ptr(),
            CString::new("w8").unwrap().as_ptr(), // BigTIFF
        );

        // ── IFD 0: Full resolution (main chain) ───────────────────────────
        {
            let group     = &groups[0];
            let metadata  = group[0];
            let first_dcm = dicom::object::open_file(&metadata.file_path).unwrap();

            let ifd_width  = metadata.px_columns.unwrap_or(0);
            let ifd_height = metadata.px_rows.unwrap_or(0);
            let (tile_w, tile_h) = metadata.tile_size.unwrap_or((ifd_width, ifd_height));

            let color_space = infer_color_space(&first_dcm);
            let photometric = match color_space {
                ColorSpace::RGB       => PHOTOMETRIC_RGB,
                ColorSpace::YCbCr     => PHOTOMETRIC_YCBCR,
                ColorSpace::Grayscale => PHOTOMETRIC_MINISBLACK,
            };
            let spp: u32 = if matches!(color_space, ColorSpace::Grayscale) { 1 } else { 3 };

            let ts_uid    = first_dcm.meta().transfer_syntax();
            let compr     = tiff_compression_tag(ts_uid);
            let mpp       = metadata.mpp_x.unwrap_or(0.25);
            let res       = 1e4 / mpp;

            // Declare the SubIFD chain BEFORE calling TIFFWriteDirectory.
            // libtiff copies the offset array; we only need it alive until
            // TIFFSetField returns.  The actual offsets are back-patched by
            // libtiff when it writes each SubIFD.
            if n_subifds > 0 {
                let subifd_offsets: Vec<u64> = vec![0u64; n_subifds];
                TIFFSetField(tiff, TIFFTAG_SUBIFD,
                    n_subifds as u32, subifd_offsets.as_ptr());
            }

            TIFFSetField(tiff, TIFFTAG_SUBFILETYPE as u32,     0u32);
            TIFFSetField(tiff, TIFFTAG_IMAGEWIDTH as u32,      ifd_width);
            TIFFSetField(tiff, TIFFTAG_IMAGELENGTH as u32,     ifd_height);
            TIFFSetField(tiff, TIFFTAG_TILEWIDTH as u32,       tile_align(tile_w, 16));
            TIFFSetField(tiff, TIFFTAG_TILELENGTH as u32,      tile_align(tile_h, 16));
            TIFFSetField(tiff, TIFFTAG_COMPRESSION as u32,     compr);
            TIFFSetField(tiff, TIFFTAG_PHOTOMETRIC as u32,     photometric);
            TIFFSetField(tiff, TIFFTAG_SAMPLESPERPIXEL as u32, spp);
            TIFFSetField(tiff, TIFFTAG_BITSPERSAMPLE as u32,   8u32);
            TIFFSetField(tiff, TIFFTAG_SAMPLEFORMAT as u32,    SAMPLEFORMAT_UINT as u32);
            TIFFSetField(tiff, TIFFTAG_PLANARCONFIG as u32,    PLANARCONFIG_CONTIG as u32);
            TIFFSetField(tiff, TIFFTAG_ORIENTATION as u32,     ORIENTATION_TOPLEFT as u32);
            TIFFSetField(tiff, TIFFTAG_RESOLUTIONUNIT as u32,  RESUNIT_CENTIMETER as u32);
            TIFFSetField(tiff, TIFFTAG_XRESOLUTION as u32,     res);
            TIFFSetField(tiff, TIFFTAG_YRESOLUTION as u32,     res);
            // OME-XML in ImageDescription marks this as an OME-TIFF.
            TIFFSetField(tiff, TIFFTAG_IMAGEDESCRIPTION as u32, image_desc_c.as_ptr());

            // Mirror actual YCbCr chroma subsampling from the JPEG stream.
            if matches!(color_space, ColorSpace::YCbCr) {
                let px_tmp    = first_dcm.element_by_name("PixelData").expect("No PixelData");
                let frags_tmp = px_tmp.fragments().expect("Not encapsulated pixel data");
                if let Some(frag) = frags_tmp.iter().find(|f| !f.is_empty()) {
                    if let Some((h, v)) = detect_jpeg_subsampling(frag) {
                        TIFFSetField(tiff, TIFFTAG_YCBCRSUBSAMPLING as u32, h as u32, v as u32);
                    }
                }
            }

            // Embed ICC profile from DICOM Tag (0028,2000) if present.
            let icc_profile = extract_icc_profile(&first_dcm);
            if let Some(ref icc) = icc_profile {
                TIFFSetField(tiff, TIFFTAG_ICCPROFILE as u32,
                    icc.len() as u32, icc.as_ptr() as *const c_void);
            }

            drop(first_dcm);

            for dcm_meta in group.iter() {
                let dicom_obj    = dicom::object::open_file(&dcm_meta.file_path).unwrap();
                let tile_indices = frame_to_tile_indices(&dicom_obj, tile_w, tile_h, ifd_width);
                let px_elem      = dicom_obj.element_by_name("PixelData").expect("No PixelData");
                let fragments    = px_elem.fragments().expect("Not encapsulated pixel data");
                for (fi, fragment) in fragments.iter().enumerate() {
                    if !fragment.is_empty() {
                        let tile_num = tile_indices.get(fi).copied().unwrap_or(fi as u32);
                        TIFFWriteRawTile(tiff, tile_num,
                            fragment.as_ptr() as *mut c_void, fragment.len() as i64);
                    }
                }
            }

            // Finalise IFD 0.  libtiff now knows to route the next n_subifds
            // TIFFWriteDirectory calls into the SubIFD chain.
            TIFFWriteDirectory(tiff);
            let ifd0_tiles: u64 = group.iter().map(|m| m.n_frames.unwrap_or(0) as u64).sum();
            if let Some(p) = pb { p.inc(ifd0_tiles); }
        }

        // ── SubIFDs: pyramid sub-resolutions (chained from IFD 0) ─────────
        // libtiff automatically routes the next n_subifds WriteDirectory calls
        // to the SubIFD chain declared above.  No special API call needed here.
        for (_sub_idx, group) in groups[1..].iter().enumerate() {
            let metadata  = group[0];
            if metadata.tile_size.is_none() {
                eprintln!("  [skip] SubIFD ({}x{}): no tile size — single-tile level omitted",
                    metadata.px_columns.unwrap_or(0),
                    metadata.px_rows.unwrap_or(0));
                continue;
            }
            let first_dcm = dicom::object::open_file(&metadata.file_path).unwrap();

            let ifd_width  = metadata.px_columns.unwrap_or(0);
            let ifd_height = metadata.px_rows.unwrap_or(0);
            let (tile_w, tile_h) = metadata.tile_size.unwrap_or((ifd_width, ifd_height));

            let color_space = infer_color_space(&first_dcm);
            let photometric = match color_space {
                ColorSpace::RGB       => PHOTOMETRIC_RGB,
                ColorSpace::YCbCr     => PHOTOMETRIC_YCBCR,
                ColorSpace::Grayscale => PHOTOMETRIC_MINISBLACK,
            };
            let spp: u32 = if matches!(color_space, ColorSpace::Grayscale) { 1 } else { 3 };

            let ts_uid = first_dcm.meta().transfer_syntax();
            let compr  = tiff_compression_tag(ts_uid);

            // When using `TIFFWriteRawTile` for pass-through, libtiff checks that the JPEG SOF header dimensions match
            // the TIFF tile declaration (after tile_align). If the JPEG dimensions are not multiples of 16,
            // this mismatch causes a "Bad value N for tileWidth/tileLength tag" error.
            if compr == 7 && !is_jpeg_tile_aligned(tile_w, tile_h) {
                eprintln!("  [skip] SubIFD ({}x{}): tile {}x{} not 16-aligned for JPEG — omitted",
                    ifd_width, ifd_height, tile_w, tile_h);
                continue;
            }

            let mpp    = metadata.mpp_x.unwrap_or(0.25);
            let res    = 1e4 / mpp;

            // SubFileType = REDUCEDIMAGE signals a lower-resolution version.
            TIFFSetField(tiff, TIFFTAG_SUBFILETYPE as u32,     FILETYPE_REDUCEDIMAGE);
            TIFFSetField(tiff, TIFFTAG_IMAGEWIDTH as u32,      ifd_width);
            TIFFSetField(tiff, TIFFTAG_IMAGELENGTH as u32,     ifd_height);
            TIFFSetField(tiff, TIFFTAG_TILEWIDTH as u32,       tile_align(tile_w, 16));
            TIFFSetField(tiff, TIFFTAG_TILELENGTH as u32,      tile_align(tile_h, 16));
            TIFFSetField(tiff, TIFFTAG_COMPRESSION as u32,     compr);
            TIFFSetField(tiff, TIFFTAG_PHOTOMETRIC as u32,     photometric);
            TIFFSetField(tiff, TIFFTAG_SAMPLESPERPIXEL as u32, spp);
            TIFFSetField(tiff, TIFFTAG_BITSPERSAMPLE as u32,   8u32);
            TIFFSetField(tiff, TIFFTAG_SAMPLEFORMAT as u32,    SAMPLEFORMAT_UINT as u32);
            TIFFSetField(tiff, TIFFTAG_PLANARCONFIG as u32,    PLANARCONFIG_CONTIG as u32);
            TIFFSetField(tiff, TIFFTAG_ORIENTATION as u32,     ORIENTATION_TOPLEFT as u32);
            TIFFSetField(tiff, TIFFTAG_RESOLUTIONUNIT as u32,  RESUNIT_CENTIMETER as u32);
            TIFFSetField(tiff, TIFFTAG_XRESOLUTION as u32,     res);
            TIFFSetField(tiff, TIFFTAG_YRESOLUTION as u32,     res);

            if matches!(color_space, ColorSpace::YCbCr) {
                let px_tmp    = first_dcm.element_by_name("PixelData").expect("No PixelData");
                let frags_tmp = px_tmp.fragments().expect("Not encapsulated pixel data");
                if let Some(frag) = frags_tmp.iter().find(|f| !f.is_empty()) {
                    if let Some((h, v)) = detect_jpeg_subsampling(frag) {
                        TIFFSetField(tiff, TIFFTAG_YCBCRSUBSAMPLING as u32, h as u32, v as u32);
                    }
                }
            }
            drop(first_dcm);

            for dcm_meta in group.iter() {
                let dicom_obj    = dicom::object::open_file(&dcm_meta.file_path).unwrap();
                let tile_indices = frame_to_tile_indices(&dicom_obj, tile_w, tile_h, ifd_width);
                let px_elem      = dicom_obj.element_by_name("PixelData").expect("No PixelData");
                let fragments    = px_elem.fragments().expect("Not encapsulated pixel data");
                for (fi, fragment) in fragments.iter().enumerate() {
                    if !fragment.is_empty() {
                        let tile_num = tile_indices.get(fi).copied().unwrap_or(fi as u32);
                        TIFFWriteRawTile(tiff, tile_num,
                            fragment.as_ptr() as *mut c_void, fragment.len() as i64);
                    }
                }
            }

            // This call writes into the SubIFD chain while n_subifds > 0,
            // then returns to the main IFD chain.
            TIFFWriteDirectory(tiff);
            let subifd_tiles: u64 = group.iter().map(|m| m.n_frames.unwrap_or(0) as u64).sum();
            if let Some(p) = pb { p.inc(subifd_tiles); }
        }

        // ── Optional associated images (main chain, after SubIFD chain) ────
        // Thumbnail / label / overview are appended to the main IFD sequence.
        // BioFormats skips these for pixel reading; slide viewers can use them.
        // if let Some(m) = thumbnail_meta {
        //     if let Some((jpeg, w, h)) = decode_frame_as_jpeg(&m.file_path, 0, 90) {
        //         write_svs_stripped_jpeg(tiff, &jpeg, w, h, FILETYPE_REDUCEDIMAGE);
        //         TIFFWriteDirectory(tiff);
        //         println!("  [OME-TIFF] Thumbnail: {}x{}", w, h);
        //     } else {
        //         println!("  [OME-TIFF] Thumbnail: decode failed, skipped");
        //     }
        // }
        // if let Some(m) = label_meta {
        //     if let Some((jpeg, w, h)) = decode_frame_as_jpeg(&m.file_path, 0, 90) {
        //         write_svs_stripped_jpeg(tiff, &jpeg, w, h, FILETYPE_REDUCEDIMAGE);
        //         TIFFWriteDirectory(tiff);
        //         println!("  [OME-TIFF] Label: {}x{}", w, h);
        //     } else {
        //         println!("  [OME-TIFF] Label: decode failed, skipped");
        //     }
        // }
        // if let Some(m) = overview_meta {
        //     if let Some((jpeg, w, h)) = decode_frame_as_jpeg(&m.file_path, 0, 90) {
        //         write_svs_stripped_jpeg(tiff, &jpeg, w, h, FILETYPE_REDUCEDIMAGE);
        //         TIFFWriteDirectory(tiff);
        //         println!("  [OME-TIFF] Overview: {}x{}", w, h);
        //     } else {
        //         println!("  [OME-TIFF] Overview: decode failed, skipped");
        //     }
        // }

        TIFFClose(tiff);
    }
}

// ─── SVS writer (JPEG 2000-compressed DICOM → Aperio SVS) ───────────────────
//
// SVS IFD order (required by OpenSlide):
//   IFD 0:         Full resolution pyramid level (largest), tiled, SubFileType=0
//   IFD 1:         Thumbnail (stripped JPEG, small), SubFileType=1
//   IFDs 2..N:     Remaining pyramid levels (descending), tiled, SubFileType=1
//   IFD N+1:       Label image (stripped JPEG), SubFileType=1   [optional]
//   IFD N+2:       Macro/Overview image (stripped JPEG), SubFileType=9 [optional]

/// Write one J2K-tiled pyramid level IFD.
/// `image_desc` is only set for IFD 0 (full resolution).
unsafe fn write_svs_tiled_level(
    tiff: *mut TIFF,
    metadata: &DcmMetadata,
    svs_compression: u32,
    photometric: u32,
    res_x: f64,
    res_y: f64,
    subfile_type: u32,
    image_desc: Option<&CString>,
) { unsafe {
    let dicom_obj  = dicom::object::open_file(&metadata.file_path).unwrap();
    let px_elem    = dicom_obj.element_by_name("PixelData").expect("No PixelData");
    let fragments  = px_elem.fragments().expect("Not encapsulated pixel data");

    let ifd_width  = metadata.px_columns.unwrap_or(0);
    let ifd_height = metadata.px_rows.unwrap_or(0);
    let (tile_w, tile_h) = metadata.tile_size.unwrap_or((ifd_width, ifd_height));

    TIFFSetField(tiff, TIFFTAG_SUBFILETYPE as u32,     subfile_type);
    TIFFSetField(tiff, TIFFTAG_IMAGEWIDTH as u32,      ifd_width);
    TIFFSetField(tiff, TIFFTAG_IMAGELENGTH as u32,     ifd_height);
    TIFFSetField(tiff, TIFFTAG_TILEWIDTH as u32,       tile_align(tile_w, 16));
    TIFFSetField(tiff, TIFFTAG_TILELENGTH as u32,      tile_align(tile_h, 16));
    TIFFSetField(tiff, TIFFTAG_COMPRESSION as u32,     svs_compression);
    TIFFSetField(tiff, TIFFTAG_PHOTOMETRIC as u32,     photometric);
    TIFFSetField(tiff, TIFFTAG_SAMPLESPERPIXEL as u32, 3u32);
    TIFFSetField(tiff, TIFFTAG_BITSPERSAMPLE as u32,   8u32);
    TIFFSetField(tiff, TIFFTAG_SAMPLEFORMAT as u32,    SAMPLEFORMAT_UINT as u32);
    TIFFSetField(tiff, TIFFTAG_PLANARCONFIG as u32,    PLANARCONFIG_CONTIG as u32);
    TIFFSetField(tiff, TIFFTAG_ORIENTATION as u32,     ORIENTATION_TOPLEFT as u32);
    TIFFSetField(tiff, TIFFTAG_RESOLUTIONUNIT as u32,  RESUNIT_CENTIMETER as u32);
    TIFFSetField(tiff, TIFFTAG_XRESOLUTION as u32,     res_x);
    TIFFSetField(tiff, TIFFTAG_YRESOLUTION as u32,     res_y);

    if let Some(desc) = image_desc {
        TIFFSetField(tiff, TIFFTAG_IMAGEDESCRIPTION as u32, desc.as_ptr());
    }

    // For YCbCr JPEG, detect actual subsampling from the first tile.
    if photometric == PHOTOMETRIC_YCBCR as u32 && svs_compression == 7 {
        if let Some(first) = fragments.iter().find(|f| !f.is_empty()) {
            if let Some((h, v)) = detect_jpeg_subsampling(first) {
                TIFFSetField(tiff, TIFFTAG_YCBCRSUBSAMPLING as u32, h as u32, v as u32);
            }
        }
    }

    let tile_indices = frame_to_tile_indices(&dicom_obj, tile_w, tile_h, ifd_width);
    for (i, fragment) in fragments.iter().enumerate() {
        if !fragment.is_empty() {
            let tile_num = tile_indices.get(i).copied().unwrap_or(i as u32);
            TIFFWriteRawTile(
                tiff, tile_num,
                fragment.as_ptr() as *mut c_void,
                fragment.len() as i64,
            );
        }
    }
}}

/// Write a stripped JPEG IFD (thumbnail / label / macro).
/// `jpeg_bytes` must be a complete, self-contained JPEG byte stream.
unsafe fn write_svs_stripped_jpeg(
    tiff: *mut TIFF,
    jpeg_bytes: &[u8],
    width: u32,
    height: u32,
    subfile_type: u32,
) { unsafe {
    TIFFSetField(tiff, TIFFTAG_SUBFILETYPE as u32,     subfile_type);
    TIFFSetField(tiff, TIFFTAG_IMAGEWIDTH as u32,      width);
    TIFFSetField(tiff, TIFFTAG_IMAGELENGTH as u32,     height);
    TIFFSetField(tiff, TIFFTAG_COMPRESSION as u32,     7u32); // JPEG
    TIFFSetField(tiff, TIFFTAG_PHOTOMETRIC as u32,     PHOTOMETRIC_YCBCR as u32);
    TIFFSetField(tiff, TIFFTAG_SAMPLESPERPIXEL as u32, 3u32);
    TIFFSetField(tiff, TIFFTAG_BITSPERSAMPLE as u32,   8u32);
    TIFFSetField(tiff, TIFFTAG_PLANARCONFIG as u32,    PLANARCONFIG_CONTIG as u32);
    TIFFSetField(tiff, TIFFTAG_ROWSPERSTRIP as u32,    height);
    TIFFWriteRawStrip(
        tiff, 0,
        jpeg_bytes.as_ptr() as *mut c_void,
        jpeg_bytes.len() as i64,
    );
}}

fn write_svs(
    slide_levels: &[DcmMetadata],      // sorted largest-first (VOLUME)
    thumbnail_meta: Option<&DcmMetadata>,
    label_meta: Option<&DcmMetadata>,
    overview_meta: Option<&DcmMetadata>,
    output_path: &str,
    pb: Option<&ProgressBar>,
) {
    // Determine SVS compression code and photometric from the full-res level.
    // For JPEG2000 source use Aperio proprietary codes; for JPEG use standard 7.
    //
    // Aperio codes for JPEG2000:
    //   33003 (YCBCR): OpenSlide decodes J2K then applies YCbCr→RGB conversion.
    //   33005 (RGB):   OpenSlide decodes J2K and treats output as-is (no conversion).
    //
    // DICOM YBR_ICT/YBR_RCT data has ICT applied in the J2K stream.
    // OpenSlide does not reverse ICT when using 33005, so the decoded values
    // remain in YCbCr space and colors appear wrong.
    // Using 33003 tells OpenSlide to apply YCbCr→RGB, which correctly compensates.
    let base = &slide_levels[0];
    let dcm0 = dicom::object::open_file(&base.file_path).unwrap();
    let color_space = infer_color_space(&dcm0);
    let ts_uid = dcm0.meta().transfer_syntax();
    let is_jp2 = is_jpeg2000(&map_transfer_syntax_to_compression(ts_uid));
    // Read raw PhotometricInterpretation to distinguish YBR_ICT/YBR_RCT from RGB.
    let photometric_interp = dcm0.element_by_name("PhotometricInterpretation")
        .ok()
        .and_then(|e| e.to_str().ok().map(|s| s.trim().to_string()))
        .unwrap_or_default();
    let (svs_compression, photometric) = match (is_jp2, photometric_interp.as_str()) {
        (true,  "YBR_ICT") | (true, "YBR_RCT")
                              => (COMPRESSION_APERIO_JP2_YCBCR, PHOTOMETRIC_YCBCR as u32),
        (true,  "YBR_FULL") | (true, "YBR_FULL_422")
                              => (COMPRESSION_APERIO_JP2_YCBCR, PHOTOMETRIC_YCBCR as u32),
        (true,  _) if matches!(color_space, ColorSpace::Grayscale)
                              => (COMPRESSION_APERIO_JP2_RGB,   PHOTOMETRIC_MINISBLACK as u32),
        (true,  _)            => (COMPRESSION_APERIO_JP2_RGB,   PHOTOMETRIC_RGB as u32),
        (false, _) if matches!(color_space, ColorSpace::YCbCr)
                              => (7u32,                         PHOTOMETRIC_YCBCR as u32),
        (false, _) if matches!(color_space, ColorSpace::Grayscale)
                              => (7u32,                         PHOTOMETRIC_MINISBLACK as u32),
        (false, _)            => (7u32,                         PHOTOMETRIC_RGB as u32),
    };

    // Base resolution
    let base_mpp_x = base.mpp_x.unwrap_or(0.25);
    let base_mpp_y = base.mpp_y.unwrap_or(base_mpp_x);
    let base_res_x = 1e4 / base_mpp_x; // pixels/cm
    let base_res_y = 1e4 / base_mpp_y;

    // Build Aperio ImageDescription for IFD 0
    let img_w      = base.px_columns.unwrap_or(0);
    let img_h      = base.px_rows.unwrap_or(0);
    let (tile_w, tile_h) = base.tile_size.unwrap_or((256, 256));
    let mag        = base.objective_power.unwrap_or_else(|| {
        // Estimate from MPP: 40x ≈ 0.25 µm/px
        (0.25 / base_mpp_x * 40.0).round()
    });
    let image_desc = format!(
        "Aperio Image Library (DICOM converted)\n\
         {}x{} ({}x{}) JPEG2000|AppMag={:.0}|MPP={:.6}",
        img_w, img_h, tile_w, tile_h, mag, base_mpp_x
    );
    let image_desc_c = CString::new(image_desc).unwrap();

    let total_tiles: u64 = {
        let base_tiles = slide_levels[0].n_frames.unwrap_or(0) as u64;
        let other_tiles: u64 = slide_levels[1..].iter().map(|level| {
            if level.tile_size.is_none() { return 0; }
            let (tile_w, tile_h) = level.tile_size.unwrap();
            if svs_compression == 7 && !is_jpeg_tile_aligned(tile_w, tile_h) { return 0; }
            level.n_frames.unwrap_or(0) as u64
        }).sum();
        base_tiles + other_tiles
            + thumbnail_meta.is_some() as u64
            + label_meta.is_some() as u64
            + overview_meta.is_some() as u64
    };
    if let Some(p) = pb { p.set_length(total_tiles); }

    unsafe {
        let tiff = TIFFOpen(
            CString::new(output_path).unwrap().as_ptr(),
            CString::new("w8").unwrap().as_ptr(),
        );

        // ── IFD 0: Full resolution ────────────────────────────────────────
        write_svs_tiled_level(
            tiff, base,
            svs_compression, photometric,
            base_res_x, base_res_y,
            0,  // SubFileType: full image
            Some(&image_desc_c),
        );
        TIFFWriteDirectory(tiff);
        if let Some(p) = pb { p.inc(base.n_frames.unwrap_or(0) as u64); }

        // ── IFD 1: Thumbnail (decoded + re-encoded as JPEG) ──────────────
        let thumb_written = thumbnail_meta
            .and_then(|m| decode_frame_as_jpeg(&m.file_path, 0, 90))
            .map(|(jpeg, w, h)| {
                write_svs_stripped_jpeg(tiff, &jpeg, w, h, FILETYPE_REDUCEDIMAGE);
                TIFFWriteDirectory(tiff);
                if let Some(p) = pb { p.inc(1); }
            });
        let _ = thumb_written;

        // ── IFDs 2..N: Remaining pyramid levels ───────────────────────────
        let base_cols = base.px_columns.unwrap_or(1) as f64;
        for (_i, level) in slide_levels[1..].iter().enumerate() {
            if level.tile_size.is_none() {
                eprintln!("  [skip] SVS level ({}x{}): no tile size — single-tile level omitted",
                    level.px_columns.unwrap_or(0),
                    level.px_rows.unwrap_or(0));
                continue;
            }

            // For JPEG passthrough, libtiff checks that the JPEG SOF header dimensions match the TIFF tile declaration.
            // If tiles are not multiples of 16, this mismatch causes an error, so we skip these levels.
            let (lvl_tile_w, lvl_tile_h) = level.tile_size.unwrap();
            if svs_compression == 7 && !is_jpeg_tile_aligned(lvl_tile_w, lvl_tile_h) {
                eprintln!("  [skip] SVS level ({}x{}): tile {}x{} not 16-aligned for JPEG — omitted",
                    level.px_columns.unwrap_or(0), level.px_rows.unwrap_or(0),
                    lvl_tile_w, lvl_tile_h);
                continue;
            }
            let ds = base_cols / level.px_columns.unwrap_or(1) as f64;
            write_svs_tiled_level(
                tiff, level,
                svs_compression, photometric,
                base_res_x / ds, base_res_y / ds,
                FILETYPE_REDUCEDIMAGE,
                None,
            );
            TIFFWriteDirectory(tiff);
            if let Some(p) = pb { p.inc(level.n_frames.unwrap_or(0) as u64); }
        }

        // ── Label image ───────────────────────────────────────────────────
        if let Some(m) = label_meta {
            if let Some((jpeg, w, h)) = decode_frame_as_jpeg(&m.file_path, 0, 90) {
                write_svs_stripped_jpeg(tiff, &jpeg, w, h, FILETYPE_REDUCEDIMAGE);
                TIFFWriteDirectory(tiff);
                if let Some(p) = pb { p.inc(1); }
            }
        }

        // ── Macro / Overview image ────────────────────────────────────────
        // SubFileType 9 = macro image (Aperio convention, SubFileType bit 3 = 0x8)
        if let Some(m) = overview_meta {
            if let Some((jpeg, w, h)) = decode_frame_as_jpeg(&m.file_path, 0, 90) {
                write_svs_stripped_jpeg(tiff, &jpeg, w, h, 9);
                TIFFWriteDirectory(tiff);
                if let Some(p) = pb { p.inc(1); }
            }
        }

        TIFFClose(tiff);
    }
}

// ─── Resampled TIFF writer ────────────────────────────────────────────────────
//
// Decodes each DICOM resolution level, resizes every tile by the same scale
// factor (derived from the base level), and writes a pyramidal OME-TIFF using
// libtiff's built-in JPEG encoder (TIFFWriteTile + JPEGCOLORMODE_RGB).
//
// Output tile size is fixed across all levels (computed once from the base
// level and rounded to the nearest multiple of 16 for JPEG MCU compliance).
// Pyramid levels whose longer side falls below MIN_PYRAMID_SIDE are skipped.
// If the source has only one DICOM level the output is a single-IFD TIFF.
fn write_resampled_tiff(
    slide_level_metadata_list: &[DcmMetadata],
    output_path: &str,
    target_mpp: f64,
    quality: u8,
    filter: FilterType,
    // When true: OME-TIFF (SubIFD pyramid + OME-XML ImageDescription).
    // When false: flat pyramidal BigTIFF (sequential IFDs, no OME-XML).
    ome: bool,
    pb: Option<&ProgressBar>,
) {

    // ── Group DICOM files by resolution level ─────────────────────────────
    let mut groups: Vec<Vec<&DcmMetadata>> = Vec::new();
    for meta in slide_level_metadata_list {
        if let Some(last) = groups.last_mut() {
            if last[0].px_columns == meta.px_columns && last[0].px_rows == meta.px_rows {
                last.push(meta);
                continue;
            }
        }
        groups.push(vec![meta]);
    }

    // ── Scale parameters derived from base level ───────────────────────────
    let base      = groups[0][0];
    let base_w    = base.px_columns.unwrap_or(0);
    let base_h    = base.px_rows.unwrap_or(0);
    let (in_tile_w, in_tile_h) = base.tile_size.unwrap_or((base_w, base_h));
    let src_mpp_x = base.mpp_x.unwrap_or(0.25);
    let src_mpp_y = base.mpp_y.unwrap_or(src_mpp_x);

    // Output tile size: nearest multiple of 16 to the scaled base tile.
    let out_tile_w = nearest_16(in_tile_w as f64 * src_mpp_x / target_mpp);
    let out_tile_h = nearest_16(in_tile_h as f64 * src_mpp_y / target_mpp);

    // Actual scale after rounding (used uniformly for every level).
    let actual_scale_x = out_tile_w as f64 / in_tile_w as f64;
    let actual_scale_y = out_tile_h as f64 / in_tile_h as f64;

    // ── Determine which groups produce an active pyramid level ─────────────
    // A level is active if its longer output side meets the minimum threshold.
    struct LevelInfo<'a> {
        group:     &'a Vec<&'a DcmMetadata>,
        out_img_w: u32,
        out_img_h: u32,
        // input tile size for this level (may differ from base)
        in_tw:     u32,
        in_th:     u32,
        actual_mpp_x: f64,
        actual_mpp_y: f64,
    }

    let active_levels: Vec<LevelInfo> = groups.iter().filter_map(|group| {
        let meta      = group[0];
        let src_w     = meta.px_columns.unwrap_or(0);
        let src_h     = meta.px_rows.unwrap_or(0);
        let out_img_w = (src_w as f64 * actual_scale_x).round() as u32;
        let out_img_h = (src_h as f64 * actual_scale_y).round() as u32;
        if out_img_w.max(out_img_h) < MIN_PYRAMID_SIDE {
            eprintln!("  [skip] resampled level {}x{} → {}x{}: shorter than MIN_PYRAMID_SIDE ({})",
                src_w, src_h, out_img_w, out_img_h, MIN_PYRAMID_SIDE);
            return None;
        }
        let (in_tw, in_th) = meta.tile_size.unwrap_or((src_w, src_h));
        let lv_mpp_x = meta.mpp_x.unwrap_or(src_mpp_x);
        let lv_mpp_y = meta.mpp_y.unwrap_or(src_mpp_y);
        // Actual stored mpp reflects the rounded tile size.
        let actual_mpp_x = lv_mpp_x * in_tw as f64 / out_tile_w as f64;
        let actual_mpp_y = lv_mpp_y * in_th as f64 / out_tile_h as f64;
        Some(LevelInfo { group, out_img_w, out_img_h, in_tw, in_th, actual_mpp_x, actual_mpp_y })
    }).collect();

    if active_levels.is_empty() { return; }

    // n_subifds: pyramid levels stored as SubIFDs (all active levels except base).
    let n_subifds = active_levels.len() - 1;

    // Total tile count across all active levels for the progress bar.
    let total_tiles: u64 = active_levels.iter().map(|lv| {
        lv.group.iter().map(|m| m.n_frames.unwrap_or(0) as u64).sum::<u64>()
    }).sum();
    if let Some(p) = pb { p.set_length(total_tiles); }

    // ── Color space from the base level ───────────────────────────────────
    let first_dcm   = dicom::object::open_file(&base.file_path).unwrap();
    let color_space = infer_color_space(&first_dcm);
    let icc_profile = extract_icc_profile(&first_dcm);

    // The jp2k crate (OpenJPEG) does NOT automatically reverse the JPEG 2000
    // Irreversible/Reversible Color Transform for DICOM tiles.  When DICOM
    // PhotometricInterpretation is YBR_ICT or YBR_RCT the decoded component
    // values are still in the transformed YCbCr-like space; we must apply the
    // inverse transform manually before feeding pixels to turbojpeg.
    let src_photometric_interp = first_dcm.element_by_name("PhotometricInterpretation")
        .ok()
        .and_then(|e| e.to_str().ok().map(|s| s.trim().to_string()))
        .unwrap_or_default();
    let jp2k_has_ict_rct = matches!(src_photometric_interp.as_str(), "YBR_ICT" | "YBR_RCT");

    // turbojpeg always produces JFIF JPEG (YCbCr encoded internally), regardless
    // of the source color space.  PHOTOMETRIC_YCBCR is the standard convention
    // for JFIF JPEG tiles in TIFF (used by libvips and other WSI tools) and is
    // correctly handled by OpenSlide / Bio-Formats without double-conversion.
    let (photometric, spp): (u32, u32) = match color_space {
        ColorSpace::Grayscale => (PHOTOMETRIC_MINISBLACK as u32, 1),
        _                     => (PHOTOMETRIC_YCBCR      as u32, 3),
    };
    drop(first_dcm);

    // ── OME-XML (only for OME-TIFF output) ───────────────────────────────
    let base_lv = &active_levels[0];
    let image_desc_c: Option<CString> = if ome {
        let mut resampled_meta = base.clone();
        resampled_meta.px_columns = Some(base_lv.out_img_w);
        resampled_meta.px_rows    = Some(base_lv.out_img_h);
        resampled_meta.mpp_x      = Some(base_lv.actual_mpp_x);
        resampled_meta.mpp_y      = Some(base_lv.actual_mpp_y);
        resampled_meta.tile_size  = Some((out_tile_w, out_tile_h));
        Some(CString::new(generate_OME_XML(&[resampled_meta])).unwrap())
    } else {
        None
    };

    // ── Helper: write all tiles for one level ─────────────────────────────
    // Pipeline per tile (fully parallel within each chunk):
    //   1. decode  — turbojpeg::decompress (SIMD) for JPEG sources;
    //                decode_pixel_data_frame for JPEG2000/other
    //   2. resize  — fast_image_resize (SIMD via NEON/AVX2)
    //   3. encode  — turbojpeg::compress (SIMD)
    //   4. write   — TIFFWriteRawTile (~memcpy, sequential)
    //
    // For JPEG sources, raw fragments are pre-extracted as Vec<Vec<u8>> so each
    // worker decodes its own independent bytes (no shared dicom_obj in the hot path).

    // Map image::FilterType → fast_image_resize algorithm once.
    // fast_image_resize has no Gaussian; Lanczos3 is the closest high-quality substitute.
    use fast_image_resize as fir;
    let fir_alg = match filter {
        image::imageops::FilterType::Nearest    => fir::ResizeAlg::Nearest,
        image::imageops::FilterType::Triangle   => fir::ResizeAlg::Convolution(fir::FilterType::Bilinear),
        image::imageops::FilterType::CatmullRom => fir::ResizeAlg::Convolution(fir::FilterType::CatmullRom),
        image::imageops::FilterType::Gaussian   => fir::ResizeAlg::Convolution(fir::FilterType::Lanczos3),
        image::imageops::FilterType::Lanczos3   => fir::ResizeAlg::Convolution(fir::FilterType::Lanczos3),
        _                                       => fir::ResizeAlg::Convolution(fir::FilterType::Lanczos3),
    };
    let fir_pixel_type = if spp == 1 { fir::PixelType::U8 } else { fir::PixelType::U8x3 };
    let resize_opts = fir::ResizeOptions::new().resize_alg(fir_alg);

    let write_level_tiles = |tiff: *mut TIFF, lv: &LevelInfo| {
        let chunk_size = (rayon::current_num_threads() * 4).max(1);

        // JP2K reduce level: largest n such that 2^n <= min(in_tw/out_tile_w, in_th/out_tile_h).
        // Decoding at this level yields ceil(in_tile / 2^n) pixels — far less work than full decode.
        // Example: in_tw=256, out_tile_w=64 → scale_down=4 → n_reduce=2 (decode at 1/4 res).
        let scale_down = (lv.in_tw as f64 / out_tile_w as f64)
            .min(lv.in_th as f64 / out_tile_h as f64);
        let n_reduce: u32 = if scale_down > 1.0 { scale_down.log2().floor() as u32 } else { 0 };

        for dcm_meta in lv.group.iter() {
            let dicom_obj    = dicom::object::open_file(&dcm_meta.file_path).unwrap();
            let src_ifd_w    = dcm_meta.px_columns.unwrap_or(0);
            let tile_indices = frame_to_tile_indices(
                &dicom_obj, lv.in_tw, lv.in_th, src_ifd_w,
            );
            let n_frames  = dcm_meta.n_frames.unwrap_or(0);
            let frame_ids: Vec<u32> = (0..n_frames).collect();

            // Pre-extract raw fragments for JPEG and JP2K sources so each parallel worker
            // decodes its own independent byte slice (no shared dicom_obj in the hot path).
            let is_jpeg_src = matches!(
                map_transfer_syntax_to_compression(&dcm_meta.transfer_syntax_uid),
                CompressionType::JpegBaseline | CompressionType::JpegExtended
            );
            let is_jp2k_src = matches!(
                map_transfer_syntax_to_compression(&dcm_meta.transfer_syntax_uid),
                CompressionType::Jpeg2000Lossless
                | CompressionType::Jpeg2000
                | CompressionType::Jpeg2000Part2MulticomponentLossless
                | CompressionType::Jpeg2000Part2Multicomponent
            );
            let raw_fragments: Vec<Vec<u8>> = if is_jpeg_src || is_jp2k_src {
                let px = dicom_obj.element_by_name("PixelData").expect("No PixelData");
                px.fragments().expect("Not encapsulated pixel data")
                    .iter()
                    .map(|f| f.to_vec())
                    .collect()
            } else {
                Vec::new()
            };

            for chunk in frame_ids.chunks(chunk_size) {
                // Parallel: decode → resize → JPEG encode.
                // Returns Some((tile_num, jpeg_bytes)) on success, None on error.
                let encoded: Vec<Option<(u32, Vec<u8>)>> = chunk
                    .par_iter()
                    .map(|&frame_idx| {
                        // ── 1. Decode → raw pixel bytes ───────────────────────
                        let (raw_pixels, src_w, src_h): (Vec<u8>, u32, u32) = if is_jpeg_src {
                            let frag = raw_fragments.get(frame_idx as usize)?;
                            let (dec_pixels, dec_w, dec_h) = if spp == 1 {
                                let dec = turbojpeg::decompress(frag, turbojpeg::PixelFormat::GRAY).ok()?;
                                let (w, h, ch) = (dec.width, dec.height, 1usize);
                                // Remove row padding if present (turbojpeg default is tight, but be safe).
                                let pixels = if dec.pitch == w * ch {
                                    dec.pixels
                                } else {
                                    (0..h).flat_map(|r| dec.pixels[r*dec.pitch..r*dec.pitch+w*ch].iter().copied()).collect()
                                };
                                (pixels, w as u32, h as u32)
                            } else {
                                let dec = turbojpeg::decompress(frag, turbojpeg::PixelFormat::RGB).ok()?;
                                let (w, h, ch) = (dec.width, dec.height, 3usize);
                                let pixels = if dec.pitch == w * ch {
                                    dec.pixels
                                } else {
                                    (0..h).flat_map(|r| dec.pixels[r*dec.pitch..r*dec.pitch+w*ch].iter().copied()).collect()
                                };
                                (pixels, w as u32, h as u32)
                            };
                            (dec_pixels, dec_w, dec_h)
                        } else if is_jp2k_src {
                            // JP2K: decode only the DWT resolution levels needed (n_reduce).
                            // Pixels decoded = ceil(W/2^n) × ceil(H/2^n) instead of W×H.
                            let frag = raw_fragments.get(frame_idx as usize)?;
                            // DICOM JP2K frames are raw J2K codestreams (magic: FF 4F FF 51)
                            let stream = jp2k::Stream::from_bytes(frag)
                                .map_err(|e| eprintln!("  [warn] frame {}: jp2k stream error: {}", frame_idx, e))
                                .ok()?;
                            let codec = jp2k::Codec::create(jp2k::CODEC_FORMAT::OPJ_CODEC_J2K)
                                .map_err(|e| eprintln!("  [warn] frame {}: jp2k codec error: {}", frame_idx, e))
                                .ok()?;
                            let decode_params = jp2k::DecodeParams::default().with_reduce_factor(n_reduce);
                            let buf = jp2k::ImageBuffer::build(codec, stream, decode_params)
                                .map_err(|e| eprintln!("  [warn] frame {}: jp2k decode failed: {}", frame_idx, e))
                                .ok()?;
                            let (w, h) = (buf.width, buf.height);
                            // Normalize to the expected number of channels (spp)
                            let mut pixels: Vec<u8> = if spp == 1 && buf.num_bands == 1 {
                                buf.buffer
                            } else if spp == 3 && buf.num_bands == 3 {
                                buf.buffer
                            } else if spp == 3 && buf.num_bands == 4 {
                                // RGBA -> RGB: drop alpha channel
                                buf.buffer.chunks(4).flat_map(|c| c[..3].iter().copied()).collect()
                            } else if spp == 1 && buf.num_bands > 1 {
                                // Color -> grayscale: take first channel
                                buf.buffer.into_iter().step_by(buf.num_bands).collect()
                            } else {
                                buf.buffer
                            };
                            // Convert YCbCr → RGB when the decoded components are not yet in
                            // RGB space:
                            //   YBR_FULL   — jp2k returns raw YCbCr (no transform applied)
                            //   YBR_ICT/RCT — OpenJPEG does NOT reverse the JPEG 2000 color
                            //                 transform for DICOM tiles; the values are still in
                            //                 the ICT/RCT-transformed (YCbCr-like) space.
                            // Without this step the YCbCr bytes would be fed to turbojpeg as
                            // RGB, yielding a double color-space conversion and completely wrong
                            // colors in the output.
                            if (color_space == ColorSpace::YCbCr || jp2k_has_ict_rct) && spp == 3 {
                                for chunk in pixels.chunks_mut(3) {
                                    let y  = chunk[0] as f32;
                                    let cb = chunk[1] as f32 - 128.0;
                                    let cr = chunk[2] as f32 - 128.0;
                                    chunk[0] = (y + 1.40200 * cr).clamp(0.0, 255.0) as u8;
                                    chunk[1] = (y - 0.34414 * cb - 0.71414 * cr).clamp(0.0, 255.0) as u8;
                                    chunk[2] = (y + 1.77200 * cb).clamp(0.0, 255.0) as u8;
                                }
                            }
                            (pixels, w, h)
                        } else {
                            // Other transfer syntaxes: fall back to dicom-pixeldata decoder.
                            let decoded = match dicom_obj.decode_pixel_data_frame(frame_idx) {
                                Ok(d)  => d,
                                Err(e) => { eprintln!("  [warn] frame {}: decode failed: {}", frame_idx, e); return None; }
                            };
                            let img = match decoded.to_dynamic_image(0) {
                                Ok(i)  => i,
                                Err(e) => { eprintln!("  [warn] frame {}: to_dynamic_image failed: {}", frame_idx, e); return None; }
                            };
                            let (w, h) = (img.width(), img.height());
                            let pixels = if spp == 1 { img.to_luma8().into_raw() } else { img.to_rgb8().into_raw() };
                            (pixels, w, h)
                        };

                        // ── 2. Resize with fast_image_resize (SIMD) ──────────
                        let src_fir = fir::images::Image::from_vec_u8(
                            src_w, src_h, raw_pixels, fir_pixel_type,
                        ).ok()?;
                        let mut dst_fir = fir::images::Image::new(out_tile_w, out_tile_h, fir_pixel_type);
                        fir::Resizer::new().resize(&src_fir, &mut dst_fir, &resize_opts).ok()?;
                        let resized_raw = dst_fir.into_vec();

                        let tile_num = tile_indices.get(frame_idx as usize).copied()
                            .unwrap_or(frame_idx);

                        // ── 3. JPEG encode with turbojpeg (SIMD) ─────────────
                        let jpeg_bytes = if spp == 1 {
                            let tj_img = turbojpeg::Image::<&[u8]> {
                                pixels: &resized_raw,
                                width:  out_tile_w as usize,
                                pitch:  out_tile_w as usize,
                                height: out_tile_h as usize,
                                format: turbojpeg::PixelFormat::GRAY,
                            };
                            turbojpeg::compress(tj_img, quality as i32, turbojpeg::Subsamp::Gray)
                                .map(|b| b.to_vec()).ok()?
                        } else {
                            let tj_img = turbojpeg::Image::<&[u8]> {
                                pixels: &resized_raw,
                                width:  out_tile_w as usize,
                                pitch:  out_tile_w as usize * 3,
                                height: out_tile_h as usize,
                                format: turbojpeg::PixelFormat::RGB,
                            };
                            turbojpeg::compress(tj_img, quality as i32, turbojpeg::Subsamp::Sub2x2)
                                .map(|b| b.to_vec()).ok()?
                        };

                        Some((tile_num, jpeg_bytes))
                    })
                    .collect();

                // ── 4. Sequential write: TIFFWriteRawTile (~memcpy) ──────
                for item in encoded {
                    if let Some((tile_num, jpeg_bytes)) = item {
                        unsafe {
                            TIFFWriteRawTile(
                                tiff, tile_num,
                                jpeg_bytes.as_ptr() as *mut c_void,
                                jpeg_bytes.len() as i64,
                            );
                        }
                    }
                    if let Some(p) = pb { p.inc(1); }
                }
            }
        }
    };

    unsafe {
        let tiff = TIFFOpen(
            CString::new(output_path).unwrap().as_ptr(),
            CString::new("w8").unwrap().as_ptr(), // BigTIFF
        );

        // ── IFD 0: base (full resampled resolution) ───────────────────────
        let lv0 = &active_levels[0];

        eprintln!("  [resample] {}x{} @ {:.4} µm/px  →  {}x{} @ {:.4} µm/px  (tile {}x{} → {}x{})",
            base_w, base_h, src_mpp_x,
            lv0.out_img_w, lv0.out_img_h, lv0.actual_mpp_x,
            in_tile_w, in_tile_h, out_tile_w, out_tile_h);

        // For OME-TIFF: register the SubIFD chain before writing IFD 0.
        // For flat TIFF: no SubIFD tag; pyramid levels are sequential IFDs.
        if ome && n_subifds > 0 {
            let zeros: Vec<u64> = vec![0u64; n_subifds];
            TIFFSetField(tiff, TIFFTAG_SUBIFD, n_subifds as u32, zeros.as_ptr());
        }

        TIFFSetField(tiff, TIFFTAG_SUBFILETYPE as u32,     0u32);
        TIFFSetField(tiff, TIFFTAG_IMAGEWIDTH as u32,      lv0.out_img_w);
        TIFFSetField(tiff, TIFFTAG_IMAGELENGTH as u32,     lv0.out_img_h);
        TIFFSetField(tiff, TIFFTAG_TILEWIDTH as u32,       out_tile_w);
        TIFFSetField(tiff, TIFFTAG_TILELENGTH as u32,      out_tile_h);
        TIFFSetField(tiff, TIFFTAG_COMPRESSION as u32,     7u32); // JPEG
        TIFFSetField(tiff, TIFFTAG_PHOTOMETRIC as u32,     photometric);
        TIFFSetField(tiff, TIFFTAG_SAMPLESPERPIXEL as u32, spp);
        TIFFSetField(tiff, TIFFTAG_BITSPERSAMPLE as u32,   8u32);
        TIFFSetField(tiff, TIFFTAG_SAMPLEFORMAT as u32,    SAMPLEFORMAT_UINT as u32);
        TIFFSetField(tiff, TIFFTAG_PLANARCONFIG as u32,    PLANARCONFIG_CONTIG as u32);
        TIFFSetField(tiff, TIFFTAG_ORIENTATION as u32,     ORIENTATION_TOPLEFT as u32);
        TIFFSetField(tiff, TIFFTAG_RESOLUTIONUNIT as u32,  RESUNIT_CENTIMETER as u32);
        TIFFSetField(tiff, TIFFTAG_XRESOLUTION as u32,     1e4 / lv0.actual_mpp_x);
        TIFFSetField(tiff, TIFFTAG_YRESOLUTION as u32,     1e4 / lv0.actual_mpp_y);
        // Sub2x2 = 4:2:0 chroma subsampling → YCbCrSubSampling [2, 2]
        if photometric == PHOTOMETRIC_YCBCR as u32 {
            TIFFSetField(tiff, TIFFTAG_YCBCRSUBSAMPLING as u32, 2u32, 2u32);
        }
        if let Some(ref desc) = image_desc_c {
            TIFFSetField(tiff, TIFFTAG_IMAGEDESCRIPTION as u32, desc.as_ptr());
        }
        if let Some(ref icc) = icc_profile {
            TIFFSetField(tiff, TIFFTAG_ICCPROFILE as u32,
                icc.len() as u32, icc.as_ptr() as *const c_void);
        }

        write_level_tiles(tiff, lv0);
        TIFFWriteDirectory(tiff);

        // ── SubIFDs: remaining active levels ──────────────────────────────
        for lv in &active_levels[1..] {
            eprintln!("  [resample]   SubIFD {}x{} @ {:.4} µm/px",
                lv.out_img_w, lv.out_img_h, lv.actual_mpp_x);

            TIFFSetField(tiff, TIFFTAG_SUBFILETYPE as u32,     FILETYPE_REDUCEDIMAGE);
            TIFFSetField(tiff, TIFFTAG_IMAGEWIDTH as u32,      lv.out_img_w);
            TIFFSetField(tiff, TIFFTAG_IMAGELENGTH as u32,     lv.out_img_h);
            TIFFSetField(tiff, TIFFTAG_TILEWIDTH as u32,       out_tile_w);
            TIFFSetField(tiff, TIFFTAG_TILELENGTH as u32,      out_tile_h);
            TIFFSetField(tiff, TIFFTAG_COMPRESSION as u32,     7u32);
            TIFFSetField(tiff, TIFFTAG_PHOTOMETRIC as u32,     photometric);
            TIFFSetField(tiff, TIFFTAG_SAMPLESPERPIXEL as u32, spp);
            TIFFSetField(tiff, TIFFTAG_BITSPERSAMPLE as u32,   8u32);
            TIFFSetField(tiff, TIFFTAG_SAMPLEFORMAT as u32,    SAMPLEFORMAT_UINT as u32);
            TIFFSetField(tiff, TIFFTAG_PLANARCONFIG as u32,    PLANARCONFIG_CONTIG as u32);
            TIFFSetField(tiff, TIFFTAG_ORIENTATION as u32,     ORIENTATION_TOPLEFT as u32);
            TIFFSetField(tiff, TIFFTAG_RESOLUTIONUNIT as u32,  RESUNIT_CENTIMETER as u32);
            TIFFSetField(tiff, TIFFTAG_XRESOLUTION as u32,     1e4 / lv.actual_mpp_x);
            TIFFSetField(tiff, TIFFTAG_YRESOLUTION as u32,     1e4 / lv.actual_mpp_y);
            if photometric == PHOTOMETRIC_YCBCR as u32 {
                TIFFSetField(tiff, TIFFTAG_YCBCRSUBSAMPLING as u32, 2u32, 2u32);
            }

            write_level_tiles(tiff, lv);
            TIFFWriteDirectory(tiff);
        }

        TIFFClose(tiff);
    }
}

// ─── Main entry point ─────────────────────────────────────────────────────────

// ─── Per-series conversion ────────────────────────────────────────────────────
//
// Converts one WSI series (all resolution levels) to the appropriate output
// format and writes the result atomically via a .tmp file.
// Called either inline (resampling mode, serial) or from a rayon::scope task
// (passthrough mode, parallel).
fn convert_one_series(
    series_meta: Vec<DcmMetadata>,
    series_idx: usize,
    args: &Args,
    mp: &MultiProgress,
    skipped: &AtomicUsize,
) {
    let convert_start = std::time::Instant::now();

    let thumbnail_meta = get_thumbnail_obj(&series_meta).cloned();
    let label_meta     = get_label_obj(&series_meta).cloned();
    let overview_meta  = get_overview_obj(&series_meta).cloned();

    let slide_levels_owned = match get_slide_level_obj(&series_meta) {
        Some(v) => v.into_iter().cloned().collect::<Vec<_>>(),
        None    => return,
    };

    let series_id     = &slide_levels_owned[0].series_instance_uid;
    let ts_uid        = &slide_levels_owned[0].transfer_syntax_uid;
    let comp          = map_transfer_syntax_to_compression(ts_uid);
    let src_mpp       = slide_levels_owned[0].mpp_x.unwrap_or(0.25);
    let effective_mpp = args.mpp.filter(|&t| t > src_mpp);

    // Resolve the stem used for the output filename.
    let file_stem: String = if args.use_parent_name {
        Path::new(&slide_levels_owned[0].file_path)
            .parent()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            .unwrap_or(series_id.as_str())
            .to_string()
    } else {
        series_id.clone()
    };

    let output_path = if effective_mpp.is_some() {
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

    // Build progress bar message: "(idx) filename", truncated to fit.
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

    // Skip if the final output already exists (guaranteed complete via tmp rename).
    if Path::new(&output_path).exists() {
        skipped.fetch_add(1, Ordering::Relaxed);
        pb.finish_and_clear();
        return;
    }

    let tmp_path = format!("{}.tmp", output_path);

    if let Some(target_mpp) = effective_mpp {
        write_resampled_tiff(
            &slide_levels_owned, &tmp_path,
            target_mpp, args.quality, args.filter,
            !args.legacy,
            Some(&pb),
        );
    } else if args.legacy {
        if is_jpeg2000(&comp) {
            write_svs(
                &slide_levels_owned,
                thumbnail_meta.as_ref(),
                label_meta.as_ref(),
                overview_meta.as_ref(),
                &tmp_path,
                Some(&pb),
            );
        } else {
            write_flat_multipage_tiff(&slide_levels_owned, &tmp_path, Some(&pb));
        }
    } else {
        write_ome_tiff(
            &slide_levels_owned,
            thumbnail_meta.as_ref(),
            overview_meta.as_ref(),
            label_meta.as_ref(),
            &tmp_path,
            Some(&pb),
        );
    }

    std::fs::rename(&tmp_path, &output_path)
        .expect("Failed to rename tmp file to output");

    let elapsed = convert_start.elapsed();
    pb.finish_with_message(format!("{} {:.2}s",
        pb_msg, elapsed.as_millis() as f64 / 1000.0));
}

pub fn run(args: Args) {
    if let Some(n) = args.jobs {
        rayon::ThreadPoolBuilder::new()
            .num_threads(n)
            .build_global()
            .expect("Failed to build Rayon thread pool");
    }

    if args.verbose {
        println!("Input:  {}", args.input_dir);
        println!("Output: {}", args.output_dir);
    }

    // if output directory doesn't exist, create it
    if !Path::new(&args.output_dir).exists() {
        std::fs::create_dir_all(&args.output_dir).expect("Failed to create output directory");
    }

    // Remove any stale .tmp files left by a previously interrupted run.
    for entry in std::fs::read_dir(&args.output_dir).into_iter().flatten().flatten() {
        let p = entry.path();
        if p.extension().map_or(false, |e| e == "tmp") {
            let _ = std::fs::remove_file(&p);
        }
    }

    let mp = MultiProgress::new();

    // ── Phase 1: discover .dcm paths grouped by directory (fast, no I/O) ─────
    let scan_pb = mp.add(ProgressBar::new_spinner());
    scan_pb.set_style(
        ProgressStyle::with_template("  Scanning...  {msg}").unwrap()
    );
    scan_pb.enable_steady_tick(Duration::from_millis(100));

    let mut dir_map: std::collections::HashMap<std::path::PathBuf, Vec<String>> =
        std::collections::HashMap::new();
    let mut last_dir_count  = 0usize;
    let mut total_file_count = 0usize;
    for entry in WalkDir::new(&args.input_dir).into_iter().filter_map(|e| e.ok()) {
        if entry.file_type().is_file()
            && entry.path().extension().map_or(false, |e| e == "dcm")
        {
            let parent = entry.path().parent()
                .unwrap_or(Path::new(".")).to_path_buf();
            dir_map.entry(parent).or_default()
                .push(entry.path().to_string_lossy().into_owned());
            total_file_count += 1;
            let n_dirs = dir_map.len();
            if n_dirs != last_dir_count {
                scan_pb.set_message(format!("{} files in {} dirs found",
                    total_file_count, n_dirs));
                last_dir_count = n_dirs;
            }
        }
    }
    scan_pb.finish_and_clear();

    let dir_groups: Vec<Vec<String>> = dir_map.into_values().collect();
    let total_files = total_file_count as u64;

    if args.verbose {
        println!("Found {} DICOM files in {} directories",
            total_files, dir_groups.len());
    }

    // ── Phase 2+3: metadata extraction pipelined with conversion ─────────────
    //
    // Scanner thread: iterates over directory groups in parallel, extracts
    // metadata, groups by series_instance_uid, and sends each WSI series
    // through a channel.
    //
    // Main thread: receives complete series via the channel and immediately
    // spawns conversion tasks into a rayon::scope, so scanning and conversion
    // overlap in time.
    let meta_pb = mp.add(ProgressBar::new(total_files));
    meta_pb.set_style(
        ProgressStyle::with_template(
            "  Extracting metadata [{bar:35.cyan/white}] {pos}/{len} ({elapsed})"
        ).unwrap().progress_chars("=>-"),
    );

    let series_counter = AtomicUsize::new(0);
    let skipped_count  = AtomicUsize::new(0);

    let (tx, rx) = mpsc::channel::<Vec<DcmMetadata>>();

    // Scanner thread: process directories in parallel; send each WSI series.
    let meta_pb_clone = meta_pb.clone();
    let scanner = std::thread::spawn(move || {
        dir_groups.into_par_iter().for_each_with(tx, |tx, files| {
            let n = files.len() as u64;
            let metas: Vec<DcmMetadata> = files.par_iter()
                .map(|p| extract_metadata(p))
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
        });
        // All tx clones drop here, closing the channel.
    });

    // References shared across all rayon::scope tasks (safe: scope outlives them).
    let mp_ref      = &mp;
    let args_ref    = &args;
    let skipped_ref = &skipped_count;

    rayon::scope(|s| {
        for series_meta in rx {
            let series_idx = series_counter.fetch_add(1, Ordering::SeqCst) + 1;
            if args_ref.mpp.is_some() {
                // Resampling: CPU-bound (decode + resize + encode).
                // Process one WSI at a time so all n_jobs threads concentrate on
                // tile-level parallelism inside write_resampled_tiff.
                convert_one_series(series_meta, series_idx, args_ref, mp_ref, skipped_ref);
            } else {
                // Passthrough: I/O-bound (raw fragment copy).
                // Spawn multiple WSIs in parallel to overlap I/O across slides.
                s.spawn(move |_| {
                    convert_one_series(series_meta, series_idx, args_ref, mp_ref, skipped_ref);
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
}
