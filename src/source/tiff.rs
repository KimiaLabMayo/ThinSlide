// TIFF/SVS pyramid metadata collection and level navigation.

use std::ffi::CString;
use crate::bindings::{
    TIFF, TIFFOpen, TIFFClose, TIFFGetField,
    TIFFSetDirectory, TIFFSetSubDirectory,
    TIFFNumberOfDirectories, TIFFNumberOfTiles,
    TIFFTAG_IMAGEWIDTH, TIFFTAG_IMAGELENGTH,
    TIFFTAG_TILEWIDTH, TIFFTAG_TILELENGTH,
    TIFFTAG_COMPRESSION, TIFFTAG_PHOTOMETRIC, TIFFTAG_SAMPLESPERPIXEL,
    TIFFTAG_IMAGEDESCRIPTION, TIFFTAG_ICCPROFILE,
    TIFFTAG_SUBIFD,
    TIFFTAG_XRESOLUTION, TIFFTAG_YRESOLUTION, TIFFTAG_RESOLUTIONUNIT,
    RESUNIT_CENTIMETER, RESUNIT_INCH,
};
use super::{LevelInfo, SlideMetadata, SlideSource};

pub(crate) const COMPRESSION_APERIO_JP2_YCBCR: u32 = 33003;
pub(crate) const COMPRESSION_APERIO_JP2_RGB: u32   = 33005;
pub(crate) const COMPRESSION_JP2000: u32           = 34712;

pub(crate) fn is_jp2k(c: u32) -> bool {
    matches!(c, COMPRESSION_APERIO_JP2_YCBCR | COMPRESSION_APERIO_JP2_RGB | COMPRESSION_JP2000)
}

#[derive(Clone)]
enum LevelNav {
    Dir(u32),
    SubDir(u64),
}

/// One IFD of the main chain, in file order.
#[derive(Clone)]
pub(crate) enum MainIfd {
    /// Full-resolution plane (one Z/C/T plane for OME-TIFF) with its SubIFD levels,
    /// ordered like `TiffSource` levels (finest first).
    Plane(Vec<TiffLevel>),
    /// OME-TIFF only: IFD of another OME Image (label, macro, thumbnail), copied as-is.
    Aux(u32),
}

#[derive(Clone)]
pub(crate) struct TiffLevel {
    pub img_w:       u32,
    pub img_h:       u32,
    pub tile_w:      u32,
    pub tile_h:      u32,
    pub mpp_x:       f64,
    pub mpp_y:       f64,
    pub compression: u16,
    pub photometric: u16,
    pub spp:         u16,
    pub n_tiles:     u32,
    nav:             LevelNav,
}

pub struct TiffSource {
    pub path:   String,
    levels:     Vec<TiffLevel>,
    level_info: Vec<LevelInfo>,
    icc:        Option<Vec<u8>>,
    pub ome_xml: Option<String>,
    layout:     Vec<MainIfd>,
    metadata:   SlideMetadata,
}

impl TiffSource {
    /// Opens a pyramidal TIFF/SVS, or an OME-TIFF when IFD 0 carries OME-XML.
    /// Errors name the reason the file cannot be processed.
    pub fn open(path: &str) -> Result<TiffSource, String> {
        let path_c  = CString::new(path).map_err(|e| e.to_string())?;
        let mode_c  = CString::new("r").map_err(|e| e.to_string())?;
        let tiff = unsafe { TIFFOpen(path_c.as_ptr(), mode_c.as_ptr()) };
        if tiff.is_null() { return Err("cannot open".to_string()); }
        let ome_xml = read_ome_xml_str(tiff);
        if ome_xml.is_some() {
            let spp = ifd0_samples_per_pixel(tiff);
            if spp != 3 {
                unsafe { TIFFClose(tiff); }
                return Err(format!("non-RGB OME-TIFF (SamplesPerPixel={spp}) is not supported"));
            }
        }
        // OME-TIFF planes are separate main IFDs, so only SubIFDs form the pyramid.
        let mut levels = collect_pyramid_levels(tiff, 0, ome_xml.is_none());
        let icc     = read_icc_profile(tiff, &levels);

        // OME-XML mpp fallback when TIFF resolution tags are absent
        if !levels.is_empty() && levels[0].mpp_x <= 0.0 {
            if let Some(ref xml) = ome_xml {
                if let Some((mx, my)) = parse_mpp_from_ome_xml(xml) {
                    let base_w = levels[0].img_w as f64;
                    let base_h = levels[0].img_h as f64;
                    levels[0].mpp_x = mx;
                    levels[0].mpp_y = my;
                    for lv in levels.iter_mut().skip(1) {
                        if lv.img_w > 0 { lv.mpp_x = mx * base_w / lv.img_w as f64; }
                        if lv.img_h > 0 { lv.mpp_y = my * base_h / lv.img_h as f64; }
                    }
                }
            }
        }

        let layout = match &ome_xml {
            _ if levels.is_empty() => Err("no tiled pyramid found".to_string()),
            Some(xml) => ome_layout(tiff, xml, path, &levels),
            None => Ok(vec![MainIfd::Plane(levels.clone())]),
        };
        unsafe { TIFFClose(tiff); }
        let layout = layout?;
        let name = std::path::Path::new(path)
            .file_stem().unwrap_or_default()
            .to_string_lossy().to_string();
        let level_info = levels.iter().map(level_to_info).collect();
        Ok(TiffSource {
            path: path.to_string(),
            levels,
            level_info,
            icc,
            ome_xml,
            layout,
            metadata: SlideMetadata { name },
        })
    }

    /// Returns (levels of the first plane, ICC, OME-XML, main IFD layout, metadata).
    pub(crate) fn into_parts(self) -> (Vec<TiffLevel>, Option<Vec<u8>>, Option<String>, Vec<MainIfd>, SlideMetadata) {
        (self.levels, self.icc, self.ome_xml, self.layout, self.metadata)
    }
}

impl SlideSource for TiffSource {
    fn levels(&self)      -> &[LevelInfo]   { &self.level_info }
    fn icc_profile(&self) -> Option<&[u8]>  { self.icc.as_deref() }
    fn metadata(&self)    -> &SlideMetadata { &self.metadata }
}

fn level_to_info(l: &TiffLevel) -> LevelInfo {
    LevelInfo {
        img_w: l.img_w, img_h: l.img_h,
        tile_w: l.tile_w, tile_h: l.tile_h,
        mpp_x: l.mpp_x, mpp_y: l.mpp_y,
        n_tiles: l.n_tiles, spp: l.spp,
    }
}

pub(crate) unsafe fn navigate(tiff: *mut TIFF, idx: usize, levels: &[TiffLevel]) {
    match &levels[idx].nav {
        LevelNav::Dir(d)    => unsafe { TIFFSetDirectory(tiff, *d); },
        LevelNav::SubDir(o) => unsafe { TIFFSetSubDirectory(tiff, *o); },
    }
}

/// Pyramid rooted at main IFD `dir`: the IFD plus its SubIFDs, or (with `scan_chain`
/// and no SubIFDs) every tiled IFD of the main chain.
fn collect_pyramid_levels(tiff: *mut TIFF, dir: u32, scan_chain: bool) -> Vec<TiffLevel> {
    let mut levels = Vec::new();

    unsafe { TIFFSetDirectory(tiff, dir); }
    let Some(mut lv0) = read_level_meta(tiff) else { return levels; };

    let mut n_sub: u16 = 0;
    let mut sub_ptr: *const u64 = std::ptr::null();
    let has_subifds = unsafe {
        TIFFGetField(tiff, TIFFTAG_SUBIFD,
            &mut n_sub as *mut u16,
            &mut sub_ptr as *mut *const u64) != 0
    } && n_sub > 0 && !sub_ptr.is_null();

    if lv0.mpp_x <= 0.0 {
        if let Some(mpp) = parse_mpp_from_image_description(tiff) {
            lv0.mpp_x = mpp;
            lv0.mpp_y = mpp;
        }
    }

    lv0.nav = LevelNav::Dir(dir);
    levels.push(lv0);

    if has_subifds {
        // Copy: the array belongs to libtiff's current directory, which the loop replaces.
        let offsets = unsafe { std::slice::from_raw_parts(sub_ptr, n_sub as usize) }.to_vec();
        for off in offsets {
            if off == 0 { continue; }
            if unsafe { TIFFSetSubDirectory(tiff, off) } != 0 {
                if let Some(mut lv) = read_level_meta(tiff) {
                    lv.nav = LevelNav::SubDir(off);
                    levels.push(lv);
                }
                unsafe { TIFFSetDirectory(tiff, dir); }
            }
        }
    } else if scan_chain {
        let n_dirs = unsafe { TIFFNumberOfDirectories(tiff) };
        for dir_idx in 1..n_dirs {
            unsafe { TIFFSetDirectory(tiff, dir_idx); }
            if let Some(mut lv) = read_level_meta(tiff) {
                lv.nav = LevelNav::Dir(dir_idx);
                levels.push(lv);
            }
        }
        unsafe { TIFFSetDirectory(tiff, 0); }
    }

    levels.sort_by(|a, b| b.img_w.cmp(&a.img_w));

    if levels.len() > 1 && levels[0].img_w > 0 {
        let bw  = levels[0].img_w as f64;
        let bh  = levels[0].img_h as f64;
        let bmx = levels[0].mpp_x;
        let bmy = levels[0].mpp_y;
        for lv in levels.iter_mut().skip(1) {
            if lv.img_w > 0 { lv.mpp_x = bmx * bw / lv.img_w as f64; }
            if lv.img_h > 0 { lv.mpp_y = bmy * bh / lv.img_h as f64; }
        }
    }

    levels.sort_by(|a, b| a.mpp_x.partial_cmp(&b.mpp_x).unwrap());
    levels
}

fn read_level_meta(tiff: *mut TIFF) -> Option<TiffLevel> {
    let mut width: u32 = 0;
    let mut height: u32 = 0;
    let mut tile_w: u32 = 0;
    let mut tile_h: u32 = 0;
    let mut compression: u16 = 1;
    let mut photometric: u16 = 2;
    let mut spp: u16 = 3;
    let mut xres: f32 = 0.0;
    let mut yres: f32 = 0.0;
    let mut resunit: u16 = RESUNIT_CENTIMETER as u16;

    unsafe {
        if TIFFGetField(tiff, TIFFTAG_IMAGEWIDTH,  &mut width  as *mut u32) == 0 { return None; }
        if TIFFGetField(tiff, TIFFTAG_IMAGELENGTH, &mut height as *mut u32) == 0 { return None; }
        if TIFFGetField(tiff, TIFFTAG_TILEWIDTH,   &mut tile_w as *mut u32) == 0 { return None; }
        if TIFFGetField(tiff, TIFFTAG_TILELENGTH,  &mut tile_h as *mut u32) == 0 { return None; }
        if tile_w == 0 || tile_h == 0 { return None; }

        TIFFGetField(tiff, TIFFTAG_COMPRESSION,     &mut compression as *mut u16);
        TIFFGetField(tiff, TIFFTAG_PHOTOMETRIC,     &mut photometric as *mut u16);
        TIFFGetField(tiff, TIFFTAG_SAMPLESPERPIXEL, &mut spp        as *mut u16);
        TIFFGetField(tiff, TIFFTAG_XRESOLUTION,     &mut xres       as *mut f32);
        TIFFGetField(tiff, TIFFTAG_YRESOLUTION,     &mut yres       as *mut f32);
        TIFFGetField(tiff, TIFFTAG_RESOLUTIONUNIT,  &mut resunit    as *mut u16);
    }

    let (mpp_x, mpp_y) = mpp_from_resolution(xres as f64, yres as f64, resunit as u32);
    let n_tiles = unsafe { TIFFNumberOfTiles(tiff) };

    Some(TiffLevel {
        img_w: width, img_h: height,
        tile_w, tile_h,
        mpp_x, mpp_y,
        compression, photometric, spp,
        n_tiles,
        nav: LevelNav::Dir(0),
    })
}

fn mpp_from_resolution(xres: f64, yres: f64, resunit: u32) -> (f64, f64) {
    if xres <= 0.0 || yres <= 0.0 { return (0.0, 0.0); }
    if resunit == RESUNIT_CENTIMETER {
        (10000.0 / xres, 10000.0 / yres)
    } else if resunit == RESUNIT_INCH {
        (25400.0 / xres, 25400.0 / yres)
    } else {
        (0.0, 0.0)
    }
}

fn parse_mpp_from_image_description(tiff: *mut TIFF) -> Option<f64> {
    let mut desc_ptr: *const std::os::raw::c_char = std::ptr::null();
    let ok = unsafe {
        TIFFGetField(tiff, TIFFTAG_IMAGEDESCRIPTION,
            &mut desc_ptr as *mut *const std::os::raw::c_char)
    };
    if ok == 0 || desc_ptr.is_null() { return None; }
    let desc = unsafe { std::ffi::CStr::from_ptr(desc_ptr) }.to_string_lossy();
    for part in desc.split('|') {
        let part = part.trim();
        if let Some(val_str) = part.strip_prefix("MPP = ") {
            if let Ok(mpp) = val_str.trim().parse::<f64>() {
                if mpp > 0.0 { return Some(mpp); }
            }
        }
    }
    None
}

fn ifd0_samples_per_pixel(tiff: *mut TIFF) -> u16 {
    let mut spp: u16 = 1;
    unsafe {
        TIFFSetDirectory(tiff, 0);
        TIFFGetField(tiff, TIFFTAG_SAMPLESPERPIXEL, &mut spp as *mut u16);
    }
    spp
}

/// OME-XML of IFD 0, detected like tifffile `is_ome`: the description contains an
/// <OME> root element and ends with its closing tag.
fn read_ome_xml_str(tiff: *mut TIFF) -> Option<String> {
    unsafe { TIFFSetDirectory(tiff, 0); }
    let mut desc_ptr: *const std::os::raw::c_char = std::ptr::null();
    let ok = unsafe {
        TIFFGetField(tiff, TIFFTAG_IMAGEDESCRIPTION,
            &mut desc_ptr as *mut *const std::os::raw::c_char)
    };
    if ok == 0 || desc_ptr.is_null() { return None; }
    let desc = unsafe { std::ffi::CStr::from_ptr(desc_ptr) }.to_string_lossy().to_string();
    if desc.contains("<OME") && desc.trim_end().ends_with("OME>") {
        Some(desc)
    } else {
        None
    }
}

/// Classifies every main IFD of an OME-TIFF: the IFDs mapped by the first <Image>'s
/// <TiffData> are its Z/C/T planes (each a SubIFD pyramid shaped like `plane0`);
/// all others belong to other Images (label, macro, ...) and are kept as Aux.
fn ome_layout(tiff: *mut TIFF, xml: &str, path: &str, plane0: &[TiffLevel]) -> Result<Vec<MainIfd>, String> {
    if xml.contains("<BinaryOnly") {
        return Err("BinaryOnly OME-TIFF (metadata in a companion file) is not supported".to_string());
    }
    let img_start = xml.find("<Image ").ok_or("OME-XML has no <Image>")?;
    let img = &xml[img_start..];
    let img = &img[..img.find("</Image>").unwrap_or(img.len())];
    let pix = &img[img.find("<Pixels").ok_or("OME-XML <Image> has no <Pixels>")?..];
    let pix_tag = &pix[..pix.find('>').unwrap_or(pix.len())];
    let size = |name: &str| -> u32 {
        extract_xml_attr(pix_tag, name).and_then(|v| v.parse().ok()).unwrap_or(1).max(1)
    };
    // SizeC counts samples, so an interleaved RGB plane (spp=3) is one plane for SizeC=3.
    let n_planes = size("SizeZ") * size("SizeT") * size("SizeC").div_ceil(plane0[0].spp.max(1) as u32);

    // Bio-Formats writes <UUID FileName="..."> even for single-file data; only reject
    // TiffData that points into another file of a multi-file set.
    let own_name = std::path::Path::new(path).file_name().and_then(|n| n.to_str()).unwrap_or("");
    let tags = |name: &'static str| img.match_indices(name)
        .map(|(i, _)| &img[i..i + img[i..].find('>').unwrap_or(img.len() - i)]);
    if tags("<UUID").any(|t| extract_xml_attr(t, "FileName").is_some_and(|f| f != own_name)) {
        return Err("multi-file OME-TIFF (TiffData in another file) is not supported".to_string());
    }

    let mut plane_ifds = std::collections::BTreeSet::new();
    for tag in tags("<TiffData") {
        let attr = |name: &str| extract_xml_attr(tag, name).and_then(|v| v.parse::<u32>().ok());
        let ifd = attr("IFD");
        let count = attr("PlaneCount").unwrap_or(if ifd.is_some() { 1 } else { n_planes });
        plane_ifds.extend(ifd.unwrap_or(0)..ifd.unwrap_or(0) + count);
    }
    if plane_ifds.is_empty() {
        plane_ifds.extend(0..n_planes);
    }

    let n_dirs = unsafe { TIFFNumberOfDirectories(tiff) };
    if !plane_ifds.contains(&0) {
        return Err("IFD 0 is not a plane of the first OME <Image>".to_string());
    }
    if let Some(&ifd) = plane_ifds.iter().find(|&&i| i >= n_dirs) {
        return Err(format!("OME <TiffData> refers to missing IFD {ifd}"));
    }

    (0..n_dirs).map(|dir| {
        if !plane_ifds.contains(&dir) { return Ok(MainIfd::Aux(dir)); }
        if dir == 0 { return Ok(MainIfd::Plane(plane0.to_vec())); }
        let mut levels = collect_pyramid_levels(tiff, dir, false);
        let same_shape = levels.len() == plane0.len() && levels.iter().zip(plane0).all(|(a, b)|
            (a.img_w, a.img_h, a.tile_w, a.tile_h, a.compression, a.photometric, a.spp)
                == (b.img_w, b.img_h, b.tile_w, b.tile_h, b.compression, b.photometric, b.spp));
        if !same_shape {
            return Err(format!("OME plane at IFD {dir} differs in pyramid shape or encoding from IFD 0"));
        }
        // Geometry is identical, so the resolution resolved for plane 0 applies.
        for (lv, lv0) in levels.iter_mut().zip(plane0) {
            (lv.mpp_x, lv.mpp_y) = (lv0.mpp_x, lv0.mpp_y);
        }
        Ok(MainIfd::Plane(levels))
    }).collect()
}

/// Parse PhysicalSizeX/Y attributes from OME-XML and return (mpp_x, mpp_y) in µm.
fn parse_mpp_from_ome_xml(xml: &str) -> Option<(f64, f64)> {
    let px: f64 = extract_xml_attr(xml, "PhysicalSizeX")?.parse().ok()?;
    let py: f64 = extract_xml_attr(xml, "PhysicalSizeY")
        .and_then(|s| s.parse().ok())
        .unwrap_or(px);
    if px <= 0.0 || py <= 0.0 { return None; }
    let ux = extract_xml_attr(xml, "PhysicalSizeXUnit").unwrap_or_else(|| "µm".to_string());
    let uy = extract_xml_attr(xml, "PhysicalSizeYUnit").unwrap_or_else(|| "µm".to_string());
    Some((to_um(px, &ux)?, to_um(py, &uy)?))
}

fn to_um(val: f64, unit: &str) -> Option<f64> {
    match unit {
        "µm" | "um" | "μm" | "micron" => Some(val),
        "nm"                           => Some(val / 1000.0),
        "mm"                           => Some(val * 1000.0),
        "cm"                           => Some(val * 10_000.0),
        "m"                            => Some(val * 1_000_000.0),
        _                              => None,
    }
}

/// Extract the value of the first XML attribute matching `attr="..."` with word-boundary check.
fn extract_xml_attr(xml: &str, attr: &str) -> Option<String> {
    let needle = format!("{}=\"", attr);
    let bytes  = xml.as_bytes();
    let nb     = needle.as_bytes();
    let mut pos = 0usize;
    while pos + nb.len() <= bytes.len() {
        if bytes[pos..].starts_with(nb) {
            let before_ok = pos == 0
                || (!bytes[pos - 1].is_ascii_alphanumeric() && bytes[pos - 1] != b'_');
            if before_ok {
                let val_start = pos + nb.len();
                if let Some(end) = xml[val_start..].find('"') {
                    return Some(xml[val_start..val_start + end].to_string());
                }
            }
        }
        pos += 1;
    }
    None
}

fn read_icc_profile(tiff: *mut TIFF, levels: &[TiffLevel]) -> Option<Vec<u8>> {
    if levels.is_empty() { return None; }
    unsafe { navigate(tiff, 0, levels); }
    let mut icc_len: u32 = 0;
    let mut icc_ptr: *const u8 = std::ptr::null();
    let ok = unsafe {
        TIFFGetField(tiff, TIFFTAG_ICCPROFILE,
            &mut icc_len as *mut u32,
            &mut icc_ptr as *mut *const u8) != 0
    };
    if ok && icc_len > 0 && !icc_ptr.is_null() {
        Some(unsafe { std::slice::from_raw_parts(icc_ptr, icc_len as usize) }.to_vec())
    } else {
        None
    }
}
