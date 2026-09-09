// libtiff's own JPEG codec (tif_jpeg.c) is never compiled into our release
// binaries (built with -Djpeg=OFF — JPEG en/decode goes through the
// `turbojpeg` crate, tiles are passed through raw via TIFFWriteRawTile/
// TIFFReadRawTile). TIFFTAG_JPEGTABLES (347) is normally registered as a
// pseudo-tag only when TIFFInitJPEG() runs, so with the codec absent
// TIFFSetField/TIFFGetField for it always fail with "Unknown tag 347".
// We register it ourselves via libtiff's documented custom-tag mechanism,
// replicating tif_jpeg.c's own field definition for this tag. libtiff's
// _TIFFMergeFields() skips re-adding a tag that's already registered, so
// this is also safe to coexist with a JPEG-enabled libtiff (e.g. local
// Homebrew dev builds) regardless of registration order.

use crate::bindings::{
    FIELD_CUSTOM, TIFFDataType_TIFF_UNDEFINED, TIFFExtendProc, TIFFFieldInfo,
    TIFFMergeFieldInfo, TIFFSetTagExtender, TIFFTAG_JPEGTABLES, TIFF, TIFF_VARIABLE2,
};
use std::sync::{Once, OnceLock};

static JPEGTABLES_NAME: &std::ffi::CStr = c"JPEGTables";

// TIFFFieldInfo carries a raw `field_name` pointer, so bindgen doesn't derive
// Sync for it. The pointer here is `'static` (a C string literal) and the
// struct is never mutated after construction, so sharing it across threads
// is sound.
unsafe impl Sync for TIFFFieldInfo {}

static JPEGTABLES_FIELD: TIFFFieldInfo = TIFFFieldInfo {
    field_tag: TIFFTAG_JPEGTABLES,
    field_readcount: TIFF_VARIABLE2 as std::os::raw::c_short,
    field_writecount: TIFF_VARIABLE2 as std::os::raw::c_short,
    field_type: TIFFDataType_TIFF_UNDEFINED,
    field_bit: FIELD_CUSTOM as std::os::raw::c_ushort,
    field_oktochange: 1,
    field_passcount: 1,
    field_name: JPEGTABLES_NAME.as_ptr() as *mut std::os::raw::c_char,
};

static PREV_EXTENDER: OnceLock<TIFFExtendProc> = OnceLock::new();

unsafe extern "C" fn extend_tag_set(tif: *mut TIFF) {
    unsafe {
        if let Some(Some(prev)) = PREV_EXTENDER.get() {
            prev(tif);
        }
        TIFFMergeFieldInfo(tif, &JPEGTABLES_FIELD, 1);
    }
}

static REGISTER_ONCE: Once = Once::new();

/// Registers TIFFTAG_JPEGTABLES as a recognized tag on every TIFF handle
/// opened for the rest of the process. Call once, at the very start of
/// main(), before any TIFFOpen (including on worker threads). Safe to call
/// more than once — only the first call has effect.
pub fn ensure_jpegtables_tag_registered() {
    REGISTER_ONCE.call_once(|| unsafe {
        let prev = TIFFSetTagExtender(Some(extend_tag_set));
        let _ = PREV_EXTENDER.set(prev);
    });
}
