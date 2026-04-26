// Shared TIFF IFD tag-setting helper used by both DICOM and TIFF writers.

use crate::bindings::{
    TIFF, TIFFSetField,
    TIFFTAG_SUBFILETYPE, TIFFTAG_IMAGEWIDTH, TIFFTAG_IMAGELENGTH,
    TIFFTAG_TILEWIDTH, TIFFTAG_TILELENGTH, TIFFTAG_COMPRESSION,
    TIFFTAG_PHOTOMETRIC, TIFFTAG_SAMPLESPERPIXEL, TIFFTAG_BITSPERSAMPLE,
    TIFFTAG_SAMPLEFORMAT, TIFFTAG_PLANARCONFIG, TIFFTAG_ORIENTATION,
    TIFFTAG_RESOLUTIONUNIT, TIFFTAG_XRESOLUTION, TIFFTAG_YRESOLUTION,
    SAMPLEFORMAT_UINT, PLANARCONFIG_CONTIG, ORIENTATION_TOPLEFT,
    RESUNIT_CENTIMETER,
};

/// Set the standard image/tile/compression/resolution TIFF tags common to every IFD.
/// Pass mpp_x == 0.0 to skip resolution tags (unknown physical size).
pub(crate) unsafe fn set_tiff_ifd_tags(
    tiff: *mut TIFF,
    subfile_type: u32,
    width: u32, height: u32,
    tile_w: u32, tile_h: u32,
    compression: u32,
    photometric: u32,
    spp: u32,
    mpp_x: f64, mpp_y: f64,
) { unsafe {
    TIFFSetField(tiff, TIFFTAG_SUBFILETYPE as u32,     subfile_type);
    TIFFSetField(tiff, TIFFTAG_IMAGEWIDTH as u32,      width);
    TIFFSetField(tiff, TIFFTAG_IMAGELENGTH as u32,     height);
    TIFFSetField(tiff, TIFFTAG_TILEWIDTH as u32,       crate::tile_align(tile_w, 16));
    TIFFSetField(tiff, TIFFTAG_TILELENGTH as u32,      crate::tile_align(tile_h, 16));
    TIFFSetField(tiff, TIFFTAG_COMPRESSION as u32,     compression);
    TIFFSetField(tiff, TIFFTAG_PHOTOMETRIC as u32,     photometric);
    TIFFSetField(tiff, TIFFTAG_SAMPLESPERPIXEL as u32, spp);
    TIFFSetField(tiff, TIFFTAG_BITSPERSAMPLE as u32,   8u32);
    TIFFSetField(tiff, TIFFTAG_SAMPLEFORMAT as u32,    SAMPLEFORMAT_UINT as u32);
    TIFFSetField(tiff, TIFFTAG_PLANARCONFIG as u32,    PLANARCONFIG_CONTIG as u32);
    TIFFSetField(tiff, TIFFTAG_ORIENTATION as u32,     ORIENTATION_TOPLEFT as u32);
    if mpp_x > 0.0 {
        TIFFSetField(tiff, TIFFTAG_RESOLUTIONUNIT as u32, RESUNIT_CENTIMETER as u32);
        TIFFSetField(tiff, TIFFTAG_XRESOLUTION as u32,   1e4 / mpp_x);
        TIFFSetField(tiff, TIFFTAG_YRESOLUTION as u32,   1e4 / mpp_y);
    }
}}
