// OME-XML generation for both DICOM and TIFF pyramid outputs.

use crate::source::dicom::DcmMetadata;

pub fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
     .replace('<', "&lt;")
     .replace('>', "&gt;")
     .replace('"', "&quot;")
}

/// Build a conforming OME-XML string (schema 2016-06) for a DICOM-derived pyramid.
/// Placed in ImageDescription tag of IFD 0; identifies the file as OME-TIFF for BioFormats.
/// The `Image` element has no `Name` and the `OME` element has no `UUID`: both are optional,
/// and the only DICOM-derived value available for them is SeriesInstanceUID, which is PHI
/// and must never appear in the output (note the default output filename is also derived
/// from SeriesInstanceUID, so it cannot be used as a stand-in either).
pub(crate) fn generate_dicom_ome_xml(metadata_list: &[DcmMetadata]) -> String {
    let base   = &metadata_list[0];
    let width  = base.px_columns.unwrap_or(0);
    let height = base.px_rows.unwrap_or(0);
    let mpp_x  = base.mpp_x.unwrap_or(0.25);
    let mpp_y  = base.mpp_y.unwrap_or(mpp_x);

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
     xsi:schemaLocation="http://www.openmicroscopy.org/Schemas/OME/2016-06 http://www.openmicroscopy.org/Schemas/OME/2016-06/ome.xsd">
{instrument_block}  <Image ID="Image:0">
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
/// Only the first <Pixels> (the pyramid image) is changed; all other metadata, including
/// TiffData IFD numbers, is preserved since the output keeps the source main-IFD order.
/// `pyramid` lists the (width, height) of every output level, base first; an existing
/// Bio-Formats PyramidResolution annotation is rewritten to match its SubIFD levels.
pub(crate) fn update_ome_xml_for_output(
    original: &str,
    pyramid: &[(u32, u32)],
    new_mpp_x: f64, new_mpp_y: f64,
) -> String {
    let Some(start) = original.find("<Pixels") else { return original.to_string(); };
    let end = original[start..].find('>').map_or(original.len(), |e| start + e);
    let (new_width, new_height) = pyramid[0];
    let mut tag = original[start..end].to_string();
    tag = replace_xml_attr(&tag, "SizeX",           &new_width.to_string());
    tag = replace_xml_attr(&tag, "SizeY",           &new_height.to_string());
    tag = replace_xml_attr(&tag, "PhysicalSizeX",   &format!("{new_mpp_x:.6}"));
    tag = replace_xml_attr(&tag, "PhysicalSizeY",   &format!("{new_mpp_y:.6}"));
    tag = replace_xml_attr(&tag, "PhysicalSizeXUnit", "µm");
    tag = replace_xml_attr(&tag, "PhysicalSizeYUnit", "µm");
    let xml = format!("{}{}{}", &original[..start], tag, &original[end..]);
    replace_pyramid_resolution(&xml, &pyramid[1..])
}

/// Rewrite the <Value> of the "openmicroscopy.org/PyramidResolution" MapAnnotation with
/// one `<M K="n">W H</M>` entry per reduced level; unchanged if there is none.
fn replace_pyramid_resolution(xml: &str, reduced: &[(u32, u32)]) -> String {
    let Some(ns) = xml.find("openmicroscopy.org/PyramidResolution") else { return xml.to_string(); };
    let Some(v0) = xml[ns..].find("<Value>").map(|i| ns + i + "<Value>".len()) else { return xml.to_string(); };
    let Some(v1) = xml[v0..].find("</Value>").map(|i| v0 + i) else { return xml.to_string(); };
    let entries: String = reduced.iter().enumerate()
        .map(|(i, (w, h))| format!("<M K=\"{}\">{} {}</M>", i + 1, w, h))
        .collect();
    format!("{}{}{}", &xml[..v0], entries, &xml[v1..])
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
