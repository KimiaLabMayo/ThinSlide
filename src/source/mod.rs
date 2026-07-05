pub mod tiff;
pub mod dicom;
pub mod vsi;
pub mod mrxs;

#[derive(Clone)]
pub struct LevelInfo {
    pub img_w:   u32,
    pub img_h:   u32,
    pub tile_w:  u32,
    pub tile_h:  u32,
    pub mpp_x:   f64,
    pub mpp_y:   f64,
    pub n_tiles: u32,
    pub spp:     u16,
}

pub struct SlideMetadata {
    pub name: String,
}

pub trait SlideSource {
    fn levels(&self) -> &[LevelInfo];
    fn icc_profile(&self) -> Option<&[u8]>;
    fn metadata(&self) -> &SlideMetadata;
}
