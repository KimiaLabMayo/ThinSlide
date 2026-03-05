// WSI dicom to tiff/svs converter
// Convert the whole slide image dicom files to a single pyramidal tiff/svs file (OpenSlide compatible)
// For JPEG-compressed DICOM: outputs generic pyramidal BigTIFF
// For JPEG 2000-compressed DICOM: outputs Aperio SVS format (OpenSlide handles J2K in SVS)
// No re-encoding: compressed pixel data is written directly to the output file

#[allow(non_upper_case_globals)]
#[allow(non_camel_case_types)]
#[allow(non_snake_case)]
mod bindings;
use bindings::*;

use std::ffi::CString;
use std::os::raw::c_void;
use walkdir::WalkDir;
use dicom_pixeldata::PixelDecoder;

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
    Unknown,
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

// ─── Metadata ────────────────────────────────────────────────────────────────

#[derive(Clone)]
struct DcmMetadata {
    sop_class_uid: String,
    study_instance_uid: String,
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
    let study_instance_uid = get_str("StudyInstanceUID");
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
        study_instance_uid,
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
        .collect();
    v.sort_by(|a, b| b.px_columns.cmp(&a.px_columns));
    if v.is_empty() { None } else { Some(v) }
}

fn calc_downsampling_factors(list: &[DcmMetadata]) -> Vec<u32> {
    let max_cols = list.iter().filter_map(|m| m.px_columns).max().unwrap_or(1);
    list.iter()
        .filter_map(|m| m.px_columns.map(|c| max_cols / c))
        .collect()
}

/// Decode a single DICOM frame and encode it as a JPEG byte stream.
/// Returns (jpeg_bytes, width, height).
fn decode_frame_as_jpeg(dcm_path: &str, frame: u32, quality: u8) -> Option<(Vec<u8>, u32, u32)> {
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
    pub input_dir: String,
    pub output_dir: String,
}

impl Args {
    pub fn build(mut args: impl Iterator<Item = String>) -> Result<Args, &'static str> {
        args.next();
        let input_dir  = args.next().ok_or("Didn't get an input directory path")?;
        let output_dir = args.next().ok_or("Didn't get an output directory path")?;
        Ok(Args { input_dir, output_dir })
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

    unsafe {
        let tiff = TIFFOpen(
            CString::new(output_path).unwrap().as_ptr(),
            CString::new("w8").unwrap().as_ptr(),
        );

        for (group_idx, group) in groups.iter().enumerate() {
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
                ColorSpace::Unknown   => PHOTOMETRIC_RGB,
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

            let mpp = metadata.mpp_x.unwrap_or(0.25);
            let res = 1e4 / mpp;

            let subfile_type: u32 = if group_idx == 0 { 0 } else { FILETYPE_REDUCEDIMAGE };
            TIFFSetField(tiff, TIFFTAG_SUBFILETYPE as u32,     subfile_type);
            TIFFSetField(tiff, TIFFTAG_IMAGEWIDTH as u32,      ifd_width);
            TIFFSetField(tiff, TIFFTAG_IMAGELENGTH as u32,     ifd_height);
            TIFFSetField(tiff, TIFFTAG_TILEWIDTH as u32,       tile_w);
            TIFFSetField(tiff, TIFFTAG_TILELENGTH as u32,      tile_h);
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
            println!("  [TIFF] level {}: {}x{} ({} DICOM file(s))",
                group_idx, ifd_width, ifd_height, group.len());
        }
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
) {
    let dicom_obj  = dicom::object::open_file(&metadata.file_path).unwrap();
    let px_elem    = dicom_obj.element_by_name("PixelData").expect("No PixelData");
    let fragments  = px_elem.fragments().expect("Not encapsulated pixel data");

    let ifd_width  = metadata.px_columns.unwrap_or(0);
    let ifd_height = metadata.px_rows.unwrap_or(0);
    let (tile_w, tile_h) = metadata.tile_size.unwrap_or((ifd_width, ifd_height));

    TIFFSetField(tiff, TIFFTAG_SUBFILETYPE as u32,     subfile_type);
    TIFFSetField(tiff, TIFFTAG_IMAGEWIDTH as u32,      ifd_width);
    TIFFSetField(tiff, TIFFTAG_IMAGELENGTH as u32,     ifd_height);
    TIFFSetField(tiff, TIFFTAG_TILEWIDTH as u32,       tile_w);
    TIFFSetField(tiff, TIFFTAG_TILELENGTH as u32,      tile_h);
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

    for (i, fragment) in fragments.iter().enumerate() {
        if !fragment.is_empty() {
            TIFFWriteRawTile(
                tiff, i as u32,
                fragment.as_ptr() as *mut c_void,
                fragment.len() as i64,
            );
        }
    }
}

/// Write a stripped JPEG IFD (thumbnail / label / macro).
/// `jpeg_bytes` must be a complete, self-contained JPEG byte stream.
unsafe fn write_svs_stripped_jpeg(
    tiff: *mut TIFF,
    jpeg_bytes: &[u8],
    width: u32,
    height: u32,
    subfile_type: u32,
) {
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
}

fn write_svs(
    slide_levels: &[DcmMetadata],      // sorted largest-first (VOLUME)
    thumbnail_meta: Option<&DcmMetadata>,
    label_meta: Option<&DcmMetadata>,
    overview_meta: Option<&DcmMetadata>,
    output_path: &str,
) {
    // Determine SVS compression code and photometric from the full-res level
    let base = &slide_levels[0];
    let dcm0 = dicom::object::open_file(&base.file_path).unwrap();
    let (svs_compression, photometric) = match infer_color_space(&dcm0) {
        ColorSpace::RGB | ColorSpace::Unknown => {
            (COMPRESSION_APERIO_JP2_RGB, PHOTOMETRIC_RGB as u32)
        }
        ColorSpace::YCbCr => {
            (COMPRESSION_APERIO_JP2_YCBCR, PHOTOMETRIC_YCBCR as u32)
        }
        ColorSpace::Grayscale => {
            (COMPRESSION_APERIO_JP2_RGB, PHOTOMETRIC_MINISBLACK as u32)
        }
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
        println!("  [SVS] IFD 0 (full res): {}x{}", img_w, img_h);

        // ── IFD 1: Thumbnail (decoded + re-encoded as JPEG) ──────────────
        let thumb_written = thumbnail_meta
            .and_then(|m| decode_frame_as_jpeg(&m.file_path, 0, 90))
            .map(|(jpeg, w, h)| {
                write_svs_stripped_jpeg(tiff, &jpeg, w, h, FILETYPE_REDUCEDIMAGE);
                TIFFWriteDirectory(tiff);
                println!("  [SVS] IFD 1 (thumbnail): {}x{}", w, h);
            });

        if thumb_written.is_none() {
            // No dedicated thumbnail DICOM — skip (OpenSlide can manage without it)
            println!("  [SVS] IFD 1 (thumbnail): skipped (not available or decode failed)");
        }

        // ── IFDs 2..N: Remaining pyramid levels ───────────────────────────
        let base_cols = base.px_columns.unwrap_or(1) as f64;
        for (i, level) in slide_levels[1..].iter().enumerate() {
            let ds = base_cols / level.px_columns.unwrap_or(1) as f64;
            let lw = base.px_columns.unwrap_or(0);
            let lh = base.px_rows.unwrap_or(0);
            let _ = (lw, lh); // suppress warnings; actual size from level metadata
            write_svs_tiled_level(
                tiff, level,
                svs_compression, photometric,
                base_res_x / ds, base_res_y / ds,
                FILETYPE_REDUCEDIMAGE,
                None,
            );
            TIFFWriteDirectory(tiff);
            println!("  [SVS] IFD {} (level {}): {}x{} (ds={:.1}x)",
                i + 2, i + 1,
                level.px_columns.unwrap_or(0), level.px_rows.unwrap_or(0), ds);
        }

        // ── Label image ───────────────────────────────────────────────────
        if let Some(m) = label_meta {
            if let Some((jpeg, w, h)) = decode_frame_as_jpeg(&m.file_path, 0, 90) {
                write_svs_stripped_jpeg(tiff, &jpeg, w, h, FILETYPE_REDUCEDIMAGE);
                TIFFWriteDirectory(tiff);
                println!("  [SVS] Label: {}x{}", w, h);
            } else {
                println!("  [SVS] Label: decode failed, skipped");
            }
        }

        // ── Macro / Overview image ────────────────────────────────────────
        // SubFileType 9 = macro image (Aperio convention, SubFileType bit 3 = 0x8)
        if let Some(m) = overview_meta {
            if let Some((jpeg, w, h)) = decode_frame_as_jpeg(&m.file_path, 0, 90) {
                write_svs_stripped_jpeg(tiff, &jpeg, w, h, 9);
                TIFFWriteDirectory(tiff);
                println!("  [SVS] Macro/Overview: {}x{}", w, h);
            } else {
                println!("  [SVS] Macro/Overview: decode failed, skipped");
            }
        }

        TIFFClose(tiff);
    }
}

// ─── Main entry point ─────────────────────────────────────────────────────────

pub fn run(args: Args) {
    println!("Input:  {}", args.input_dir);
    println!("Output: {}", args.output_dir);

    let dicom_files = search_dicom_files(&args.input_dir);
    println!("Found {} DICOM files", dicom_files.len());

    let metadata_list: Vec<DcmMetadata> = dicom_files.iter()
        .map(|p| extract_metadata(p))
        .collect();

    // Unique series instance UIDs for WSI
    let unique_series: Vec<String> = metadata_list.iter()
        .filter(|m| is_wsi_dicom(m))
        .map(|m| m.series_instance_uid.clone())
        .collect::<std::collections::HashSet<_>>()
        .into_iter()
        .collect();

    println!("Found {} unique WSI series", unique_series.len());

    for series_id in unique_series {
        println!("──────────────────────────────────────────");
        println!("Series: {}", series_id);

        let series_meta: Vec<&DcmMetadata> = metadata_list.iter()
            .filter(|m| m.series_instance_uid == series_id)
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
            None => {
                println!("  No VOLUME images found, skipping.");
                continue;
            }
        };

        println!("  Pyramid levels: {}", slide_levels_owned.len());
        for (i, lv) in slide_levels_owned.iter().enumerate() {
            println!("    Level {}: {}x{} (MPP={:?})",
                i, lv.px_columns.unwrap_or(0), lv.px_rows.unwrap_or(0), lv.mpp_x);
        }

        // Determine output format from transfer syntax of full-res level
        let ts_uid = &slide_levels_owned[0].transfer_syntax_uid;
        let comp = map_transfer_syntax_to_compression(ts_uid);

        if is_jpeg2000(&comp) {
            let output_path = format!("{}/{}.svs", args.output_dir, series_id);
            println!("  → Writing SVS (JPEG 2000): {}", output_path);
            write_svs(
                &slide_levels_owned,
                thumbnail_meta.as_ref(),
                label_meta.as_ref(),
                overview_meta.as_ref(),
                &output_path,
            );
            println!("  Done: {}", output_path);
        } else {
            let output_path = format!("{}/{}.tiff", args.output_dir, series_id);
            println!("  → Writing pyramidal TIFF (JPEG): {}", output_path);
            write_flat_multipage_tiff(&slide_levels_owned, &output_path);
            println!("  Done: {}", output_path);
        }
    }
}
