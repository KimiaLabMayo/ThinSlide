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
    pub fn load(path: &str) -> Result<Roi, String> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| format!("cannot read '{}': {}", path, e))?;
        Self::parse(&text)
    }

    pub fn parse(text: &str) -> Result<Roi, String> {
        let v: Value = serde_json::from_str(text).map_err(|e| format!("invalid JSON: {}", e))?;
        let mut polygons = Vec::new();
        collect(&v, &mut polygons)?;
        if polygons.is_empty() {
            return Err("no Polygon/MultiPolygon geometry found".to_string());
        }
        Ok(Roi { polygons })
    }

    /// Resolves the ROI for one slide from the --roi argument: a .geojson file is used
    /// as-is; a directory is searched for `<stem>.geojson`. Ok(None) means no match.
    pub fn resolve(roi_arg: &str, stem: &str) -> Result<Option<Roi>, String> {
        let p = std::path::Path::new(roi_arg);
        if p.is_file() { return Self::load(roi_arg).map(Some); }
        let candidate = p.join(format!("{}.geojson", stem));
        if !candidate.is_file() { return Ok(None); }
        Self::load(&candidate.to_string_lossy()).map(Some)
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

fn collect(v: &Value, out: &mut Vec<Polygon>) -> Result<(), String> {
    match v {
        Value::Array(items) => {
            for it in items { collect(it, out)?; }
        }
        Value::Object(obj) => match obj.get("type").and_then(Value::as_str) {
            Some("FeatureCollection") => {
                if let Some(fs) = obj.get("features") { collect(fs, out)?; }
            }
            Some("Feature") => {
                if let Some(g) = obj.get("geometry") { collect(g, out)?; }
            }
            Some("GeometryCollection") => {
                if let Some(gs) = obj.get("geometries") { collect(gs, out)?; }
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
            let roi = Roi::parse(&text).unwrap();
            assert!(roi.intersects_rect(150.0, 150.0, 160.0, 160.0), "{}", text);
        }
    }

    #[test]
    fn rect_inside_outside_and_crossing() {
        let roi = Roi::parse(SQUARE).unwrap();
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
        let roi = Roi::parse(text).unwrap();
        assert!(!roi.intersects_rect(40.0, 40.0, 60.0, 60.0));   // inside the hole
        assert!(roi.intersects_rect(5.0, 5.0, 10.0, 10.0));      // on the ring
        assert!(!roi.intersects_rect(250.0, 20.0, 260.0, 30.0)); // in L-shape's empty corner
    }

    #[test]
    fn rejects_empty_and_invalid() {
        assert!(Roi::parse(r#"{"type":"FeatureCollection","features":[]}"#).is_err());
        assert!(Roi::parse(r#"{"type":"Point","coordinates":[1,2]}"#).is_err());
        assert!(Roi::parse("not json").is_err());
    }
}
