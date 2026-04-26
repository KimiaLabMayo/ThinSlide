// DICOM series metadata extraction, classification, and SlideSource implementation.

use dicom_core::Tag;
use super::{LevelInfo, SlideMetadata, SlideSource};

const PWSI_CLASS_UID: &str = "1.2.840.10008.5.1.4.1.1.77.1.6";

// ─── Compression type ─────────────────────────────────────────────────────────

pub(crate) enum CompressionType {
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

impl std::fmt::Display for CompressionType {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        let s = match self {
            CompressionType::JpegBaseline                        => "JPEG Baseline",
            CompressionType::JpegExtended                        => "JPEG Extended Sequential",
            CompressionType::JpegLossless                        => "JPEG Lossless",
            CompressionType::JpegLosslessNonHierarchical         => "JPEG Lossless Non-Hierarchical",
            CompressionType::JpegLSLossless                      => "JPEG-LS Lossless",
            CompressionType::JpegLSNearLossless                  => "JPEG-LS Near-Lossless",
            CompressionType::Jpeg2000Lossless                    => "JPEG 2000 Lossless",
            CompressionType::Jpeg2000                            => "JPEG 2000",
            CompressionType::Jpeg2000Part2MulticomponentLossless => "JPEG 2000 Part 2 Multicomponent Lossless",
            CompressionType::Jpeg2000Part2Multicomponent         => "JPEG 2000 Part 2 Multicomponent",
            CompressionType::Unknown                             => "Unknown/Uncompressed",
        };
        write!(f, "{}", s)
    }
}

pub(crate) fn map_transfer_syntax_to_compression(uid: &str) -> CompressionType {
    match uid {
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
        _                         => CompressionType::Unknown,
    }
}

pub(crate) fn is_jpeg2000(comp: &CompressionType) -> bool {
    matches!(comp,
        CompressionType::Jpeg2000
        | CompressionType::Jpeg2000Lossless
        | CompressionType::Jpeg2000Part2Multicomponent
        | CompressionType::Jpeg2000Part2MulticomponentLossless
    )
}

pub(crate) fn tiff_compression_tag(ts_uid: &str) -> u32 {
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

// ─── Color space ─────────────────────────────────────────────────────────────

#[derive(PartialEq, Clone, Copy)]
pub(crate) enum ColorSpace {
    RGB,
    YCbCr,
    Grayscale,
}

impl std::fmt::Display for ColorSpace {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        let s = match self {
            ColorSpace::RGB       => "RGB",
            ColorSpace::YCbCr     => "YCbCr",
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

pub(crate) fn infer_color_space(dcm: &dicom::object::DefaultDicomObject) -> ColorSpace {
    // 1. DICOM PhotometricInterpretation tag (most reliable)
    if let Ok(elem) = dcm.element_by_name("PhotometricInterpretation") {
        if let Ok(s) = elem.to_str() {
            match s.trim() {
                "RGB" => return ColorSpace::RGB,
                // YBR_ICT / YBR_RCT are JPEG 2000 internal transforms;
                // the decoded output is RGB, so SVS photometric = RGB.
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

// ─── Metadata ────────────────────────────────────────────────────────────────

#[derive(Clone)]
pub(crate) struct DcmMetadata {
    pub(crate) sop_class_uid:        String,
    pub(crate) series_instance_uid:  String,
    pub(crate) modality:             String,
    pub(crate) transfer_syntax_uid:  String,
    pub(crate) n_frames:             Option<u32>,
    pub(crate) px_columns:           Option<u32>,
    pub(crate) px_rows:              Option<u32>,
    pub(crate) file_path:            String,
    pub(crate) image_type:           Option<String>,
    pub(crate) tile_size:            Option<(u32, u32)>,
    pub(crate) mpp_x:                Option<f64>,
    pub(crate) mpp_y:                Option<f64>,
    pub(crate) objective_power:      Option<f64>,
    pub(crate) spp:                  u16,
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

pub(crate) fn extract_metadata(dcm_path: &str) -> Result<DcmMetadata, dicom_object::ReadError> {
    let dcm = dicom::object::open_file(dcm_path)?;

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

    let sop_class_uid       = get_str("SOPClassUID");
    let series_instance_uid = get_str("SeriesInstanceUID");
    let modality            = get_str("Modality");
    let transfer_syntax_uid = dcm.meta().transfer_syntax().to_string();

    let n_frames   = get_u32("NumberOfFrames");
    let px_columns = get_u32("TotalPixelMatrixColumns");
    let px_rows    = get_u32("TotalPixelMatrixRows");

    let image_type = dcm.element_by_name("ImageType").ok()
        .and_then(|e| e.to_str().ok())
        .map(|s| s.to_string());

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
    let spp = get_u32("SamplesPerPixel").unwrap_or(3) as u16;

    Ok(DcmMetadata {
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
        spp,
    })
}

/// Extract the ICC profile bytes from DICOM Tag (0028,2000), if present.
pub(crate) fn extract_icc_profile(dcm: &dicom::object::DefaultDicomObject) -> Option<Vec<u8>> {
    // Standard DICOM WSI: OpticalPathSequence[0].ICCProfile
    let from_optical = (|| -> Option<Vec<u8>> {
        let seq  = dcm.element_by_name("OpticalPathSequence").ok()?;
        let item = seq.items()?.first()?;
        let bytes = item.element_by_name("ICCProfile").ok()
            .and_then(|e| e.to_bytes().ok())
            .map(|b| b.into_owned())
            .filter(|b| !b.is_empty())?;
        Some(bytes)
    })();
    if from_optical.is_some() { return from_optical; }

    // Fallback: top-level tag (0028,2000)
    let bytes = dcm.element(Tag(0x0028, 0x2000)).ok()
        .and_then(|e| e.to_bytes().ok())
        .map(|b| b.into_owned())?;
    if bytes.is_empty() { None } else { Some(bytes) }
}

// ─── Series helpers ──────────────────────────────────────────────────────────

pub(crate) fn is_wsi_dicom(m: &DcmMetadata) -> bool {
    m.sop_class_uid == PWSI_CLASS_UID && m.modality == "SM"
}

pub(crate) fn group_by_resolution<'a>(metas: &'a [DcmMetadata]) -> Vec<Vec<&'a DcmMetadata>> {
    let mut groups: Vec<Vec<&DcmMetadata>> = Vec::new();
    for meta in metas {
        if let Some(last) = groups.last_mut() {
            if last[0].px_columns == meta.px_columns && last[0].px_rows == meta.px_rows {
                last.push(meta);
                continue;
            }
        }
        groups.push(vec![meta]);
    }
    groups
}

pub(crate) fn get_thumbnail_obj(list: &[DcmMetadata]) -> Option<&DcmMetadata> {
    list.iter().find(|m| m.image_type.as_deref().unwrap_or("").contains("THUMBNAIL"))
}

pub(crate) fn get_overview_obj(list: &[DcmMetadata]) -> Option<&DcmMetadata> {
    list.iter().find(|m| m.image_type.as_deref().unwrap_or("").contains("OVERVIEW"))
}

pub(crate) fn get_label_obj(list: &[DcmMetadata]) -> Option<&DcmMetadata> {
    list.iter().find(|m| m.image_type.as_deref().unwrap_or("").contains("LABEL"))
}

pub(crate) fn get_slide_level_obj(list: &[DcmMetadata]) -> Option<Vec<&DcmMetadata>> {
    let mut v: Vec<&DcmMetadata> = list.iter()
        .filter(|m| m.image_type.as_deref().unwrap_or("").contains("VOLUME"))
        .filter(|m| {
            let ifd_w = m.px_columns.unwrap_or(0);
            let ifd_h = m.px_rows.unwrap_or(0);
            let (tile_w, tile_h) = m.tile_size.unwrap_or((ifd_w, ifd_h));
            ifd_w >= tile_w && ifd_h >= tile_h
        })
        .collect();
    v.sort_by(|a, b| b.px_columns.cmp(&a.px_columns));
    if v.is_empty() { None } else { Some(v) }
}

/// Compute the TIFF tile number for each DICOM frame by reading
/// PerFrameFunctionalGroupsSequence → PlanePositionSlideSequence.
/// Falls back to sequential numbering when position info is absent.
pub(crate) fn frame_to_tile_indices(
    dcm:     &dicom::object::DefaultDicomObject,
    tile_w:  u32,
    tile_h:  u32,
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

// ─── DicomSource ─────────────────────────────────────────────────────────────

pub struct DicomSource {
    pub(crate) slide_levels: Vec<DcmMetadata>,    // sorted flat list (largest first)
    pub(crate) thumbnail:    Option<DcmMetadata>,
    pub(crate) label:        Option<DcmMetadata>,
    pub(crate) overview:     Option<DcmMetadata>,
    icc:                     Option<Vec<u8>>,
    level_info:              Vec<LevelInfo>,
    metadata:                SlideMetadata,
}

impl DicomSource {
    pub(crate) fn from_series(metas: Vec<DcmMetadata>) -> Option<Self> {
        let slide_level_refs = get_slide_level_obj(&metas)?;
        let slide_levels: Vec<DcmMetadata> = slide_level_refs.into_iter().cloned().collect();

        let thumbnail = get_thumbnail_obj(&metas).cloned();
        let label     = get_label_obj(&metas).cloned();
        let overview  = get_overview_obj(&metas).cloned();

        let level_info: Vec<LevelInfo> = group_by_resolution(&slide_levels)
            .iter()
            .map(|g| {
                let m = g[0];
                let (tw, th) = m.tile_size.unwrap_or((
                    m.px_columns.unwrap_or(0),
                    m.px_rows.unwrap_or(0),
                ));
                LevelInfo {
                    img_w:   m.px_columns.unwrap_or(0),
                    img_h:   m.px_rows.unwrap_or(0),
                    tile_w:  tw,
                    tile_h:  th,
                    mpp_x:   m.mpp_x.unwrap_or(0.0),
                    mpp_y:   m.mpp_y.unwrap_or(0.0),
                    n_tiles: g.iter().map(|x| x.n_frames.unwrap_or(0)).sum(),
                    spp:     m.spp,
                }
            })
            .collect();

        let icc = dicom::object::open_file(&slide_levels[0].file_path)
            .ok()
            .and_then(|d| extract_icc_profile(&d));

        let name = slide_levels[0].series_instance_uid.clone();

        Some(DicomSource {
            slide_levels, thumbnail, label, overview,
            icc, level_info,
            metadata: SlideMetadata { name },
        })
    }
}

impl SlideSource for DicomSource {
    fn levels(&self)      -> &[LevelInfo]   { &self.level_info }
    fn icc_profile(&self) -> Option<&[u8]>  { self.icc.as_deref() }
    fn metadata(&self)    -> &SlideMetadata { &self.metadata }
}
