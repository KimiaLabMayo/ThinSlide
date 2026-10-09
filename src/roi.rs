// Region-of-interest support for --roi.
// Loads QuPath-style GeoJSON annotations (level-0 pixel coordinates) and tests
// whether a rectangle overlaps any annotated area. Accepted top-level shapes:
// FeatureCollection, a bare array of Features, a single Feature, or a geometry.

use serde_json::Value;

#[derive(Clone, Debug)]
struct Polygon {
    // Exterior ring followed by any hole rings.
    rings: Vec<Vec<(f64, f64)>>,
    // (min_x, min_y, max_x, max_y)
    bbox: (f64, f64, f64, f64),
}

#[derive(Clone, Debug)]
pub struct Roi {
    polygons: Vec<Polygon>,
}

impl Roi {
    /// `ids` selects Features by their top-level "id"; empty means all geometries.
    pub fn load(path: &str, ids: &[String]) -> Result<Roi, String> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| format!("cannot read '{}': {}", path, e))?;
        Self::parse(&text, ids)
    }

    pub fn parse(text: &str, ids: &[String]) -> Result<Roi, String> {
        let v: Value = serde_json::from_str(text).map_err(|e| format!("invalid JSON: {}", e))?;
        let mut polygons = Vec::new();
        let mut found = Vec::new();
        collect(&v, (!ids.is_empty()).then_some(ids), &mut found, &mut polygons)?;
        let missing: Vec<&str> = ids.iter().filter(|id| !found.contains(id)).map(String::as_str).collect();
        if !missing.is_empty() {
            return Err(format!("feature id(s) not found: {}", missing.join(", ")));
        }
        if polygons.is_empty() {
            return Err("no Polygon/MultiPolygon geometry found".to_string());
        }
        Ok(Roi { polygons })
    }

    /// Resolves the ROI for one slide from the --roi argument: a .geojson file is used
    /// as-is; a directory is searched for `<stem>.geojson`. Ok(None) means no match.
    pub fn resolve(roi_arg: &str, stem: &str, ids: &[String]) -> Result<Option<Roi>, String> {
        let p = std::path::Path::new(roi_arg);
        if p.is_file() { return Self::load(roi_arg, ids).map(Some); }
        let candidate = p.join(format!("{}.geojson", stem));
        if !candidate.is_file() { return Ok(None); }
        Self::load(&candidate.to_string_lossy(), ids).map(Some)
    }

    /// True if the closed rectangle [x0, x1] x [y0, y1] overlaps any annotation.
    pub fn intersects_rect(&self, x0: f64, y0: f64, x1: f64, y1: f64) -> bool {
        self.polygons.iter().any(|p| p.intersects_rect(x0, y0, x1, y1))
    }

    /// Per-tile "touches an annotation" flags (row-major) for a `grid` of `tile`-sized
    /// cells laid over a pyramid level of size `level`; `base` is the level-0 size the
    /// annotation coordinates refer to.
    pub fn tile_mask(&self, grid: (u32, u32), tile: (u32, u32), level: (u32, u32), base: (u32, u32)) -> Vec<bool> {
        let sx = base.0 as f64 / level.0 as f64;
        let sy = base.1 as f64 / level.1 as f64;
        (0..grid.0 * grid.1).map(|id| {
            let (c, r) = (id % grid.0, id / grid.0);
            let x0 = (c * tile.0) as f64 * sx;
            let y0 = (r * tile.1) as f64 * sy;
            let x1 = ((c + 1) * tile.0).min(level.0) as f64 * sx;
            let y1 = ((r + 1) * tile.1).min(level.1) as f64 * sy;
            self.intersects_rect(x0, y0, x1, y1)
        }).collect()
    }
}

/// Tile-aligned crop of a level: the bounding box of the tiles touching an annotation.
#[derive(Clone, Debug)]
pub struct RoiCrop {
    pub c0:   u32,
    pub r0:   u32,
    pub cols: u32,
    pub rows: u32,
    full_cols: u32,
    /// "Touches an annotation" flags of the cropped tiles (row-major).
    pub mask: Vec<bool>,
}

impl RoiCrop {
    /// Builds the crop from a full-grid tile mask; None if no tile is flagged.
    pub fn from_mask(mask: &[bool], grid: (u32, u32)) -> Option<RoiCrop> {
        let (mut c0, mut r0, mut c1, mut r1) = (u32::MAX, u32::MAX, 0, 0);
        for id in (0..mask.len()).filter(|&i| mask[i]) {
            let (c, r) = (id as u32 % grid.0, id as u32 / grid.0);
            (c0, r0, c1, r1) = (c0.min(c), r0.min(r), c1.max(c + 1), r1.max(r + 1));
        }
        if c1 == 0 { return None; }
        let (cols, rows) = (c1 - c0, r1 - r0);
        let crop_mask = (0..cols * rows)
            .map(|id| mask[((r0 + id / cols) * grid.0 + c0 + id % cols) as usize])
            .collect();
        Some(RoiCrop { c0, r0, cols, rows, full_cols: grid.0, mask: crop_mask })
    }

    /// Tile index in the full grid for cropped tile `id`.
    pub fn full_id(&self, id: u32) -> u32 {
        (self.r0 + id / self.cols) * self.full_cols + self.c0 + id % self.cols
    }

    /// Pixel size of the cropped level; the last tile column/row is clipped to `full`.
    pub fn dim(&self, full: (u32, u32), tile: (u32, u32)) -> (u32, u32) {
        (((self.c0 + self.cols) * tile.0).min(full.0) - self.c0 * tile.0,
         ((self.r0 + self.rows) * tile.1).min(full.1) - self.r0 * tile.1)
    }
}

// `ids`: Some = keep only geometries of Features whose "id" is listed (matched ids are
// pushed to `found`); None = keep every geometry.
fn collect(v: &Value, ids: Option<&[String]>, found: &mut Vec<String>, out: &mut Vec<Polygon>) -> Result<(), String> {
    match v {
        Value::Array(items) => {
            for it in items { collect(it, ids, found, out)?; }
        }
        Value::Object(obj) => match obj.get("type").and_then(Value::as_str) {
            Some("FeatureCollection") => {
                if let Some(fs) = obj.get("features") { collect(fs, ids, found, out)?; }
            }
            Some("Feature") => {
                if let Some(ids) = ids {
                    let fid = match obj.get("id") {
                        Some(Value::String(s)) => s.clone(),
                        Some(Value::Number(n)) => n.to_string(),
                        _ => return Ok(()),
                    };
                    if !ids.contains(&fid) { return Ok(()); }
                    found.push(fid);
                }
                if let Some(g) = obj.get("geometry") { collect(g, None, found, out)?; }
            }
            // Bare geometries carry no Feature id.
            Some(_) if ids.is_some() => {}
            Some("GeometryCollection") => {
                if let Some(gs) = obj.get("geometries") { collect(gs, None, found, out)?; }
            }
            Some("Polygon") => {
                out.push(parse_polygon(obj.get("coordinates").unwrap_or(&Value::Null))?);
            }
            Some("MultiPolygon") => {
                let polys = obj.get("coordinates").and_then(Value::as_array)
                    .ok_or("MultiPolygon without coordinates array")?;
                for p in polys { out.push(parse_polygon(p)?); }
            }
            // Points and LineStrings enclose no area.
            _ => {}
        },
        // e.g. a Feature with "geometry": null
        _ => {}
    }
    Ok(())
}

fn parse_polygon(v: &Value) -> Result<Polygon, String> {
    let rings_v = v.as_array().ok_or("Polygon coordinates must be an array of rings")?;
    let mut rings = Vec::with_capacity(rings_v.len());
    for ring_v in rings_v {
        let pts_v = ring_v.as_array().ok_or("Polygon ring must be an array of positions")?;
        let mut ring = Vec::with_capacity(pts_v.len());
        for p in pts_v {
            let xy = p.as_array()
                .and_then(|a| Some((a.first()?.as_f64()?, a.get(1)?.as_f64()?)))
                .ok_or("Polygon position must be [x, y]")?;
            ring.push(xy);
        }
        rings.push(ring);
    }
    let ext = rings.first().filter(|r| !r.is_empty()).ok_or("Polygon has no exterior ring")?;
    let bbox = ext.iter().fold(
        (f64::MAX, f64::MAX, f64::MIN, f64::MIN),
        |(a, b, c, d), &(x, y)| (a.min(x), b.min(y), c.max(x), d.max(y)),
    );
    Ok(Polygon { rings, bbox })
}

impl Polygon {
    fn intersects_rect(&self, x0: f64, y0: f64, x1: f64, y1: f64) -> bool {
        let (bx0, by0, bx1, by1) = self.bbox;
        if bx1 < x0 || bx0 > x1 || by1 < y0 || by0 > y1 { return false; }
        // Any boundary edge crossing (or lying inside) the rectangle.
        for ring in &self.rings {
            let n = ring.len();
            for i in 0..n {
                if segment_hits_rect(ring[i], ring[(i + 1) % n], x0, y0, x1, y1) { return true; }
            }
        }
        // No edge touches the rectangle: it is either entirely inside or entirely outside.
        self.contains((x0 + x1) * 0.5, (y0 + y1) * 0.5)
    }

    // Even-odd rule over all rings, so holes are excluded.
    fn contains(&self, x: f64, y: f64) -> bool {
        let mut inside = false;
        for ring in &self.rings {
            let n = ring.len();
            for i in 0..n {
                let (xi, yi) = ring[i];
                let (xj, yj) = ring[(i + 1) % n];
                if (yi > y) != (yj > y) && x < (xj - xi) * (y - yi) / (yj - yi) + xi {
                    inside = !inside;
                }
            }
        }
        inside
    }
}

// Liang-Barsky clipping test of segment a-b against the closed rectangle.
fn segment_hits_rect(a: (f64, f64), b: (f64, f64), x0: f64, y0: f64, x1: f64, y1: f64) -> bool {
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    let (mut t0, mut t1) = (0.0f64, 1.0f64);
    for (p, q) in [(-dx, a.0 - x0), (dx, x1 - a.0), (-dy, a.1 - y0), (dy, y1 - a.1)] {
        if p == 0.0 {
            if q < 0.0 { return false; }
        } else {
            let r = q / p;
            if p < 0.0 {
                if r > t1 { return false; }
                t0 = t0.max(r);
            } else {
                if r < t0 { return false; }
                t1 = t1.min(r);
            }
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    const SQUARE: &str = r#"{"type":"Polygon","coordinates":[[[100,100],[100,200],[200,200],[200,100],[100,100]]]}"#;

    #[test]
    fn accepts_all_container_shapes() {
        let feat = format!(r#"{{"type":"Feature","geometry":{},"properties":{{}}}}"#, SQUARE);
        for text in [
            SQUARE.to_string(),
            feat.clone(),
            format!("[{}]", feat),
            format!(r#"{{"type":"FeatureCollection","features":[{}]}}"#, feat),
        ] {
            let roi = Roi::parse(&text, &[]).unwrap();
            assert!(roi.intersects_rect(150.0, 150.0, 160.0, 160.0), "{}", text);
        }
    }

    #[test]
    fn rect_inside_outside_and_crossing() {
        let roi = Roi::parse(SQUARE, &[]).unwrap();
        assert!(roi.intersects_rect(120.0, 120.0, 130.0, 130.0));   // fully inside
        assert!(roi.intersects_rect(0.0, 0.0, 1000.0, 1000.0));     // contains polygon
        assert!(roi.intersects_rect(190.0, 50.0, 300.0, 120.0));    // crosses corner
        assert!(!roi.intersects_rect(300.0, 300.0, 400.0, 400.0));  // outside bbox
    }

    #[test]
    fn hole_and_concave_outside() {
        // Square with a hole, plus an L-shaped polygon whose bbox covers the probe.
        let text = r#"{"type":"MultiPolygon","coordinates":[
            [[[0,0],[0,100],[100,100],[100,0],[0,0]],[[20,20],[20,80],[80,80],[80,20],[20,20]]],
            [[[200,0],[200,100],[300,100],[300,90],[210,90],[210,0],[200,0]]]
        ]}"#;
        let roi = Roi::parse(text, &[]).unwrap();
        assert!(!roi.intersects_rect(40.0, 40.0, 60.0, 60.0));   // inside the hole
        assert!(roi.intersects_rect(5.0, 5.0, 10.0, 10.0));      // on the ring
        assert!(!roi.intersects_rect(250.0, 20.0, 260.0, 30.0)); // in L-shape's empty corner
    }

    #[test]
    fn crop_from_mask() {
        // 4x3 grid with tiles (1,1) and (2,2) flagged.
        let mut mask = vec![false; 12];
        mask[5] = true;
        mask[10] = true;
        let c = RoiCrop::from_mask(&mask, (4, 3)).unwrap();
        assert_eq!((c.c0, c.r0, c.cols, c.rows), (1, 1, 2, 2));
        assert_eq!(c.mask, vec![true, false, false, true]);
        assert_eq!(c.full_id(3), 10);
        // Image 1000x700 with 256px tiles: the last row is clipped.
        assert_eq!(c.dim((1000, 700), (256, 256)), (512, 444));
        assert!(RoiCrop::from_mask(&[false; 12], (4, 3)).is_none());
    }

    #[test]
    fn rejects_empty_and_invalid() {
        assert!(Roi::parse(r#"{"type":"FeatureCollection","features":[]}"#, &[]).is_err());
        assert!(Roi::parse(r#"{"type":"Point","coordinates":[1,2]}"#, &[]).is_err());
        assert!(Roi::parse("not json", &[]).is_err());
    }

    #[test]
    fn selects_features_by_id() {
        let text = r#"{"type":"FeatureCollection","features":[
            {"type":"Feature","id":"section-0","geometry":{"type":"Polygon","coordinates":[[[0,0],[0,10],[10,10],[10,0],[0,0]]]}},
            {"type":"Feature","id":"section-1","geometry":{"type":"Polygon","coordinates":[[[100,100],[100,110],[110,110],[110,100],[100,100]]]}},
            {"type":"Feature","id":7,"geometry":{"type":"Polygon","coordinates":[[[200,200],[200,210],[210,210],[210,200],[200,200]]]}}
        ]}"#;
        let ids = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let roi = Roi::parse(text, &ids(&["section-1"])).unwrap();
        assert!(!roi.intersects_rect(2.0, 2.0, 4.0, 4.0));
        assert!(roi.intersects_rect(102.0, 102.0, 104.0, 104.0));
        assert!(!roi.intersects_rect(202.0, 202.0, 204.0, 204.0));
        let roi = Roi::parse(text, &ids(&["section-0", "7"])).unwrap();
        assert!(roi.intersects_rect(2.0, 2.0, 4.0, 4.0));
        assert!(roi.intersects_rect(202.0, 202.0, 204.0, 204.0));
        let err = Roi::parse(text, &ids(&["section-0", "section-5"])).unwrap_err();
        assert!(err.contains("section-5") && !err.contains("section-0"), "{}", err);
        // A bare geometry has no id to match.
        assert!(Roi::parse(SQUARE, &ids(&["section-0"])).is_err());
    }
}
