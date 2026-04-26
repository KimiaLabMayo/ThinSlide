use std::sync::Arc;

pub(crate) struct IccTransform(lcms2::Transform<u8, u8>);
// SAFETY: lcms2 transforms are thread-safe for concurrent cmsDoTransform calls (LCMS2 >= 2.8).
unsafe impl Send for IccTransform {}
unsafe impl Sync for IccTransform {}

pub(crate) fn build_icc_transform(icc_data: &[u8]) -> Option<Arc<IccTransform>> {
    let src = lcms2::Profile::new_icc(icc_data).ok()?;
    let dst = lcms2::Profile::new_srgb();
    let xform = lcms2::Transform::new(
        &src, lcms2::PixelFormat::RGB_8,
        &dst, lcms2::PixelFormat::RGB_8,
        lcms2::Intent::Perceptual,
    ).ok()?;
    Some(Arc::new(IccTransform(xform)))
}

pub(crate) fn apply_icc(xform: &IccTransform, src: &[u8], dst: &mut [u8]) {
    xform.0.transform_pixels(src, dst);
}
