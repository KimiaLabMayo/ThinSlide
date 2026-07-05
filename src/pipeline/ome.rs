// OME-XML generation for both DICOM and TIFF pyramid outputs.

use crate::source::dicom::DcmMetadata;

pub fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
     .replace('<', "&lt;")
     .replace('>', "&gt;")
     .replace('"', "&quot;")
}

fn uid_to_uuid(uid: &str) -> String {
    const OFFSET: u64 = 14695981039346656037;
    const PRIME:  u64 = 1099511628211;
    let bytes = uid.as_bytes();
    let mut a = OFFSET;
    for &b in bytes { a ^= b as u64; a = a.wrapping_mul(PRIME); }
    let mut bv = OFFSET ^ 0xdeadbeef_cafebabe_u64;
    for &byte in bytes { bv ^= byte as u64; bv = bv.wrapping_mul(PRIME); }
    format!(
        "{:08x}-{:04x}-4{:03x}-{:04x}-{:012x}",
        (a >> 32) as u32,
        (a >> 16) as u16,
        a as u16 & 0x0FFF,
        ((bv >> 48) as u16 & 0x3FFF) | 0x8000,
        bv & 0x0000_FFFF_FFFF_FFFF_u64,
    )
}

/// Build a conforming OME-XML string (schema 2016-06) for a DICOM-derived pyramid.
/// Placed in ImageDescription tag of IFD 0; identifies the file as OME-TIFF for BioFormats.
pub(crate) fn generate_dicom_ome_xml(metadata_list: &[DcmMetadata]) -> String {
    let base   = &metadata_list[0];
    let width  = base.px_columns.unwrap_or(0);
    let height = base.px_rows.unwrap_or(0);
    let mpp_x  = base.mpp_x.unwrap_or(0.25);
    let mpp_y  = base.mpp_y.unwrap_or(mpp_x);
    let uuid   = uid_to_uuid(&base.series_instance_uid);
    let name   = &base.series_instance_uid;

    let spp: u32 = base.spp as u32;
    let dcm = dicom::object::open_file(&base.file_path).ok();
    let bps: u32 = dcm.as_ref()
        .and_then(|d| d.element_by_name("BitsAllocated").ok())
        .and_then(|e| e.to_str().ok().and_then(|s| s.trim().parse().ok()))
        .unwrap_or(8);
    let manufacturer: Option<String> = dcm.as_ref()
        .and_then(|d| d.element_by_name("Manufacturer").ok())
        .and_then(|e| e.to_str().ok().map(|s| s.trim().to_string()))
        .filter(|s| !s.is_empty());

    let pixel_type = match (bps, spp) {
        (8,  _) => "uint8",
        (16, _) => "uint16",
        (32, _) => "uint32",
        _       => "uint8",
    };

    let (size_c, channel_spp, interleaved) = if spp >= 3 {
        (spp, spp, "true")
    } else {
        (1u32, 1u32, "false")
    };

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
            SizeC="{size_c}"
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

/// Replace the first occurrence of `attr="..."` (word-boundary aware) in `xml`.
fn replace_xml_attr(xml: &str, attr: &str, new_val: &str) -> String {
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
                    let val_end = val_start + end;
                    let mut result = xml.to_string();
                    result.replace_range(val_start..val_end, new_val);
                    return result;
                }
            }
        }
        pos += 1;
    }
    xml.to_string()
}

/// Update an existing OME-XML string with new output dimensions and physical size.
/// Preserves all other metadata (Image name, Channel info, Instrument, etc.).
pub(crate) fn update_ome_xml_for_output(
    original: &str,
    new_width: u32, new_height: u32,
    new_mpp_x: f64, new_mpp_y: f64,
) -> String {
    let mut xml = original.to_string();
    xml = replace_xml_attr(&xml, "SizeX",           &new_width.to_string());
    xml = replace_xml_attr(&xml, "SizeY",           &new_height.to_string());
    xml = replace_xml_attr(&xml, "PhysicalSizeX",   &format!("{new_mpp_x:.6}"));
    xml = replace_xml_attr(&xml, "PhysicalSizeY",   &format!("{new_mpp_y:.6}"));
    xml = replace_xml_attr(&xml, "PhysicalSizeXUnit", "µm");
    xml = replace_xml_attr(&xml, "PhysicalSizeYUnit", "µm");
    // Reset TiffData IFD to 0 (main image is always at IFD 0 in our output)
    xml = replace_xml_attr(&xml, "IFD", "0");
    xml
}

/// Build an OME-XML string for a TIFF/SVS-derived pyramid (simpler form without DICOM UUID).
pub(crate) fn generate_tiff_ome_xml(
    name: &str,
    width: u32, height: u32,
    mpp_x: f64, mpp_y: f64,
    spp: u32,
) -> String {
    let safe_name = xml_escape(name);
    let type_str  = "uint8";
    // OME requires SizeC == sum of each Channel's SamplesPerPixel.
    // A single RGB channel (spp=3) is one Channel with SamplesPerPixel=3, so SizeC=3.
    let size_c      = spp;
    let interleaved = if spp >= 3 { "true" } else { "false" };

    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<OME xmlns="http://www.openmicroscopy.org/Schemas/OME/2016-06" xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance" xsi:schemaLocation="http://www.openmicroscopy.org/Schemas/OME/2016-06 http://www.openmicroscopy.org/Schemas/OME/2016-06/ome.xsd">
  <Image ID="Image:0" Name="{safe_name}">
    <Pixels ID="Pixels:0" DimensionOrder="XYZCT" Type="{type_str}" SizeX="{width}" SizeY="{height}" SizeZ="1" SizeC="{size_c}" SizeT="1" PhysicalSizeX="{mpp_x:.6}" PhysicalSizeXUnit="µm" PhysicalSizeY="{mpp_y:.6}" PhysicalSizeYUnit="µm" Interleaved="{interleaved}">
      <Channel ID="Channel:0:0" SamplesPerPixel="{spp}"/>
      <TiffData/>
    </Pixels>
  </Image>
</OME>"#
    )
}
