// WSI dicom to tiff/svs converter
// Convert the whole slide image dicom files to a single pyramidal OME-TIFF (default) or
// legacy format (SVS / generic BigTIFF) when --legacy is passed.
// No re-encoding: compressed pixel data is written directly to the output file

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
use rayon::prelude::*;
use walkdir::WalkDir;
use dicom_pixeldata::PixelDecoder;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use std::path::Path;

const PWSI_CLASS_UID: &str = "1.2.840.10008.5.1.4.1.1.77.1.6";

// SVS (Aperio) JPEG 2000 proprietary compression codes recognized by OpenSlide
const COMPRESSION_APERIO_JP2_YCBCR: u32 = 33003;
const COMPRESSION_APERIO_JP2_RGB: u32 = 33005;

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
}

impl Args {
    pub fn build(args: impl Iterator<Item = String>) -> Result<Args, &'static str> {
        let all: Vec<String> = args.collect();
        let legacy  = all.iter().any(|a| a == "--legacy");
        let verbose = all.iter().any(|a| a == "-v" || a == "--verbose");

        // Parse --jobs N or -j N
        let jobs = all.windows(2).find_map(|w| {
            if w[0] == "--jobs" || w[0] == "-j" {
                w[1].parse::<usize>().ok()
            } else {
                None
            }
        });

        // Collect positional args, skipping flags and their values
        let mut positional: Vec<&str> = Vec::new();
        let mut skip_next = false;
        for token in &all[1..] {
            if skip_next { skip_next = false; continue; }
            if token == "--jobs" || token == "-j" { skip_next = true; continue; }
            if token.starts_with('-') { continue; }
            positional.push(token.as_str());
        }
        let input_dir  = positional.get(0).ok_or("Didn't get an input directory path")?.to_string();
        let output_dir = positional.get(1).ok_or("Didn't get an output directory path")?.to_string();
        Ok(Args { input_dir, output_dir, legacy, verbose, jobs })
    }
}

fn search_dicom_files(input_dir: &str) -> Vec<String> {
    WalkDir::new(input_dir)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().unwrap_or_default() == "dcm")
        .map(|e| e.path().to_string_lossy().into_owned())
        .collect()
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
    if let Some(p) = pb { p.set_length(groups.len() as u64); }

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
            if let Some(p) = pb { p.inc(1); }
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
    if let Some(p) = pb { p.set_length(groups.len() as u64); }

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
            if let Some(p) = pb { p.inc(1); }
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
            if let Some(p) = pb { p.inc(1); }
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

    let total_ifds = 1
        + slide_levels.len().saturating_sub(1) as u64
        + thumbnail_meta.is_some() as u64
        + label_meta.is_some() as u64
        + overview_meta.is_some() as u64;
    if let Some(p) = pb { p.set_length(total_ifds); }

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
        if let Some(p) = pb { p.inc(1); }

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
            if let Some(p) = pb { p.inc(1); }
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

// ─── Main entry point ─────────────────────────────────────────────────────────

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

    let start_time = std::time::Instant::now();
    let dicom_files = search_dicom_files(&args.input_dir);
    let elapsed = start_time.elapsed();
    if args.verbose {
        println!("Found {} DICOM files in {:.2}s", dicom_files.len(), elapsed.as_millis() as f64 / 1000.0);
    }

    let mp = MultiProgress::new();

    let scan_pb = mp.add(ProgressBar::new(dicom_files.len() as u64));
    scan_pb.set_style(
        ProgressStyle::with_template(
            "  Scanning DICOM files [{bar:35.cyan/white}] {pos}/{len} ({elapsed})"
        ).unwrap().progress_chars("=>-"),
    );

    let metadata_list: Vec<DcmMetadata> = dicom_files.par_iter()
        .map(|p| { let m = extract_metadata(p); scan_pb.inc(1); m })
        .collect();

    scan_pb.finish_and_clear();

    // Unique series instance UIDs for WSI
    let unique_series: Vec<String> = metadata_list.iter()
        .filter(|m| is_wsi_dicom(m))
        .map(|m| m.series_instance_uid.clone())
        .collect::<std::collections::HashSet<_>>()
        .into_iter()
        .collect();

    if args.verbose {
        println!("Found {} unique WSI series", unique_series.len());
    }

    let overall = mp.add(ProgressBar::new(unique_series.len() as u64));
    overall.set_style(
        ProgressStyle::with_template(
            "  Converting series    [{bar:35.yellow/white}] {pos}/{len}"
        ).unwrap().progress_chars("=>-"),
    );

    unique_series.par_iter().for_each(|series_id| {
        let convert_start = std::time::Instant::now();

        let series_meta: Vec<&DcmMetadata> = metadata_list.iter()
            .filter(|m| m.series_instance_uid == *series_id)
            .collect();

        let thumbnail_meta = get_thumbnail_obj(
            &series_meta.iter().map(|m| (*m).clone()).collect::<Vec<_>>()
        ).cloned();
        let label_meta = get_label_obj(
            &series_meta.iter().map(|m| (*m).clone()).collect::<Vec<_>>()
        ).cloned();
        let overview_meta = get_overview_obj(
            &series_meta.iter().map(|m| (*m).clone()).collect::<Vec<_>>()
        ).cloned();

        let slide_levels_owned = match get_slide_level_obj(
            &series_meta.iter().map(|m| (*m).clone()).collect::<Vec<_>>()
        ) {
            Some(v) => v.into_iter().cloned().collect::<Vec<_>>(),
            None => return,
        };

        // Create per-series progress bar
        let pb = mp.add(ProgressBar::new(0));
        pb.set_style(
            ProgressStyle::with_template(
                "  {msg:<45} [{bar:35.green/white}] {pos:>2}/{len} IFDs"
            ).unwrap().progress_chars("=>-"),
        );
        let msg = if series_id.len() > 43 {
            format!("…{}", &series_id[series_id.len() - 42..])
        } else {
            series_id.clone()
        };
        pb.set_message(msg);

        // Determine output format and path
        let ts_uid = &slide_levels_owned[0].transfer_syntax_uid;
        let comp = map_transfer_syntax_to_compression(ts_uid);

        let output_path = if args.legacy && is_jpeg2000(&comp) {
            format!("{}/{}.svs", args.output_dir, series_id)
        } else if args.legacy {
            format!("{}/{}.tiff", args.output_dir, series_id)
        } else {
            format!("{}/{}.ome.tiff", args.output_dir, series_id)
        };

        if args.legacy {
            if is_jpeg2000(&comp) {
                write_svs(
                    &slide_levels_owned,
                    thumbnail_meta.as_ref(),
                    label_meta.as_ref(),
                    overview_meta.as_ref(),
                    &output_path,
                    Some(&pb),
                );
            } else {
                write_flat_multipage_tiff(&slide_levels_owned, &output_path, Some(&pb));
            }
        } else {
            write_ome_tiff(
                &slide_levels_owned,
                thumbnail_meta.as_ref(),
                overview_meta.as_ref(),
                label_meta.as_ref(),
                &output_path,
                Some(&pb),
            );
        }

        let elapsed = convert_start.elapsed();
        pb.finish_with_message(format!("{:.2}s  {}",
            elapsed.as_millis() as f64 / 1000.0,
            std::path::Path::new(&output_path).file_name()
                .and_then(|n| n.to_str()).unwrap_or("")));
        overall.inc(1);
    });
    overall.finish_with_message("Done");
}
