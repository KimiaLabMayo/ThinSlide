// libtiff's own JPEG codec (tif_jpeg.c) is never compiled into our release
// binaries (built with -Djpeg=OFF — JPEG en/decode goes through the
// `turbojpeg` crate, tiles are passed through raw via TIFFWriteRawTile/
// TIFFReadRawTile). TIFFTAG_JPEGTABLES (347) is normally registered as a
// pseudo-tag only when TIFFInitJPEG() runs, so with the codec absent
// TIFFSetField/TIFFGetField for it always fail with "Unknown tag 347".
// We register it ourselves via libtiff's documented custom-tag mechanism,
// replicating tif_jpeg.c's own field definition for this tag.
//
// This must only happen when the real JPEG codec is absent. Our extender
// runs at TIFFDefaultDirectory() time — before TIFFTAG_COMPRESSION is ever
// set on the handle — which is *earlier* than TIFFInitJPEG() registers its
// own (fuller) field entry for this tag when the codec is configured.
// _TIFFMergeFields() silently skips re-adding a tag that's already
// registered, so if we always merged ours first, a JPEG-enabled libtiff
// (e.g. local Homebrew dev builds) would end up stuck with our generic
// byte-blob field instead of tif_jpeg.c's own — which owns a *different*
// internal storage slot (`sp->otherSettings.jpegtables`) that
// TIFFWriteDirectory actually serializes from. SetField/GetField would
// keep working (reading/writing our slot), but the bytes would never reach
// the file: JPEGTables silently vanishes from the output, corrupting every
// tile that relies on it. So only merge our fallback field when
// TIFFIsCODECConfigured(COMPRESSION_JPEG) is false — the exact case where
// there's no real field entry to collide with.
use crate::bindings::{
    COMPRESSION_JPEG, FIELD_CUSTOM, TIFFDataType_TIFF_UNDEFINED, TIFFExtendProc, TIFFFieldInfo,
    TIFFIsCODECConfigured, TIFFMergeFieldInfo, TIFFSetTagExtender, TIFFTAG_JPEGTABLES, TIFF,
    TIFF_VARIABLE2,
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
        if TIFFIsCODECConfigured(COMPRESSION_JPEG as u16) == 0 {
            TIFFMergeFieldInfo(tif, &JPEGTABLES_FIELD, 1);
        }
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

// Registering the tag above is only half the story for *reading*. libtiff's
// TIFFReadDirectory() (tif_dirread.c) additionally gates every JPEG-codec
// tag — JPEGTABLES included — behind `_TIFFCheckFieldIsValidForCodec()`,
// which checks `TIFFIsCODECConfigured(COMPRESSION_JPEG)`. On a -Djpeg=OFF
// libtiff that's always false, so the tag is marked `tdir_ignore` while the
// directory is parsed and its value is never fetched — TIFFGetField(JPEGTABLES)
// then legitimately reports "not found" on *every* JPEG-compressed source
// directory, regardless of the tag registration above. This is a read-time
// gate inside libtiff we cannot lift from outside it (the check runs
// unconditionally, whether or not the tag is otherwise recognized).
//
// Work around it by re-parsing the current IFD ourselves, directly from the
// file, to pull out tag 347's raw bytes. Used only as a fallback when
// TIFFGetField fails, so behavior on a JPEG-enabled libtiff (dev builds)
// is unchanged.
pub(crate) fn read_jpegtables_fallback(tiff: *mut TIFF, path: &str) -> Option<Vec<u8>> {
    use crate::bindings::{TIFFCurrentDirOffset, TIFFIsBigTIFF, TIFFIsByteSwapped};
    use std::io::{Read, Seek, SeekFrom};

    let dir_offset = unsafe { TIFFCurrentDirOffset(tiff) };
    if dir_offset == 0 {
        return None;
    }
    let big_tiff = unsafe { TIFFIsBigTIFF(tiff) } != 0;
    // Our host targets (x86_64 / aarch64) are all little-endian, so
    // "byte swapped" means the file itself is big-endian (MM).
    let file_be = unsafe { TIFFIsByteSwapped(tiff) } != 0;

    let mut f = std::fs::File::open(path).ok()?;
    f.seek(SeekFrom::Start(dir_offset)).ok()?;

    fn r_u16(f: &mut std::fs::File, be: bool) -> Option<u16> {
        let mut b = [0u8; 2];
        f.read_exact(&mut b).ok()?;
        Some(if be { u16::from_be_bytes(b) } else { u16::from_le_bytes(b) })
    }
    fn r_u32(f: &mut std::fs::File, be: bool) -> Option<u32> {
        let mut b = [0u8; 4];
        f.read_exact(&mut b).ok()?;
        Some(if be { u32::from_be_bytes(b) } else { u32::from_le_bytes(b) })
    }
    fn r_u64(f: &mut std::fs::File, be: bool) -> Option<u64> {
        let mut b = [0u8; 8];
        f.read_exact(&mut b).ok()?;
        Some(if be { u64::from_be_bytes(b) } else { u64::from_le_bytes(b) })
    }
    // TIFF field type -> byte size (BYTE/ASCII/SBYTE/UNDEFINED=1,
    // SHORT/SSHORT=2, LONG/SLONG/FLOAT/IFD=4, RATIONAL/SRATIONAL/DOUBLE/
    // LONG8/SLONG8/IFD8=8).
    fn type_size(t: u16) -> u64 {
        match t {
            1 | 2 | 6 | 7 => 1,
            3 | 8 => 2,
            4 | 9 | 11 | 13 => 4,
            5 | 10 | 12 | 16 | 17 | 18 => 8,
            _ => 1,
        }
    }

    const TAG_JPEGTABLES: u16 = 347;

    if big_tiff {
        let n_entries = r_u64(&mut f, file_be)?;
        for _ in 0..n_entries {
            let tag = r_u16(&mut f, file_be)?;
            let typ = r_u16(&mut f, file_be)?;
            let count = r_u64(&mut f, file_be)?;
            let mut value = [0u8; 8];
            f.read_exact(&mut value).ok()?;
            if tag != TAG_JPEGTABLES {
                continue;
            }
            let total = type_size(typ) * count;
            if total <= 8 {
                return Some(value[..total as usize].to_vec());
            }
            let data_offset = if file_be { u64::from_be_bytes(value) } else { u64::from_le_bytes(value) };
            f.seek(SeekFrom::Start(data_offset)).ok()?;
            let mut buf = vec![0u8; total as usize];
            f.read_exact(&mut buf).ok()?;
            return Some(buf);
        }
    } else {
        let n_entries = r_u16(&mut f, file_be)?;
        for _ in 0..n_entries {
            let tag = r_u16(&mut f, file_be)?;
            let typ = r_u16(&mut f, file_be)?;
            let count = r_u32(&mut f, file_be)?;
            let mut value = [0u8; 4];
            f.read_exact(&mut value).ok()?;
            if tag != TAG_JPEGTABLES {
                continue;
            }
            let total = type_size(typ) * count as u64;
            if total <= 4 {
                return Some(value[..total as usize].to_vec());
            }
            let data_offset = (if file_be { u32::from_be_bytes(value) } else { u32::from_le_bytes(value) }) as u64;
            f.seek(SeekFrom::Start(data_offset)).ok()?;
            let mut buf = vec![0u8; total as usize];
            f.read_exact(&mut buf).ok()?;
            return Some(buf);
        }
    }
    None
}

/// Fetch TIFFTAG_JPEGTABLES for the TIFF handle's *current* directory,
/// falling back to a raw re-parse of that directory (see
/// `read_jpegtables_fallback`) when TIFFGetField can't retrieve it — the
/// case on a -Djpeg=OFF libtiff reading a JPEG-compressed source.
pub(crate) fn get_jpeg_tables(tiff: *mut TIFF, path: &str) -> Option<Vec<u8>> {
    use crate::bindings::TIFFGetField;

    let mut tlen: u32 = 0;
    let mut tptr: *const u8 = std::ptr::null();
    let ok = unsafe {
        TIFFGetField(tiff, TIFFTAG_JPEGTABLES, &mut tlen as *mut u32, &mut tptr as *mut *const u8)
    };
    if ok != 0 && !tptr.is_null() && tlen > 2 {
        return Some(unsafe { std::slice::from_raw_parts(tptr, tlen as usize) }.to_vec());
    }
    read_jpegtables_fallback(tiff, path).filter(|v| v.len() > 2)
}
