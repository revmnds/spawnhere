//! Snap-to-windows: pulls the cursor and dragged rectangle corners onto the
//! corners and edges of existing windows on the focused monitor (and the
//! safe-area rect). Built once when the overlay opens — the candidate set is
//! frozen for the session.
//!
//! The snapper exposes a single primitive: [`Snapper::snap`] takes a raw point
//! and returns a [`SnapResult`] with the (possibly snapped) point plus optional
//! `x_guide` / `y_guide` coordinates so the renderer can draw Figma-style
//! alignment lines through the locked axis.
//!
//! Algorithm:
//!   1. If a candidate *point* (corner or edge midpoint) lies within the snap
//!      radius (Euclidean), lock both axes to it.
//!   2. Otherwise consider X and Y independently — snap to the nearest x-line
//!      or y-line that's within the radius (1-D distance). Either, both, or
//!      neither axis may snap.

use crate::stroke::Bbox;

/// One axis lock — emitted on whichever axis the user is aligning to.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SnapResult {
    pub point: (f32, f32),
    /// X-coordinate of a vertical alignment guide. `Some` when the cursor is
    /// snapped on the X-axis.
    pub x_guide: Option<f32>,
    /// Y-coordinate of a horizontal alignment guide.
    pub y_guide: Option<f32>,
}

impl SnapResult {
    pub fn passthrough(p: (f32, f32)) -> Self {
        Self { point: p, x_guide: None, y_guide: None }
    }

    /// `true` when at least one axis was actually snapped.
    pub fn snapped(&self) -> bool {
        self.x_guide.is_some() || self.y_guide.is_some()
    }
}

pub struct Snapper {
    /// 4 corners + 4 edge midpoints per rect, plus the safe-area's own.
    points: Vec<(f32, f32)>,
    /// Vertical guide candidates — left/right edges of every rect.
    x_lines: Vec<f32>,
    /// Horizontal guide candidates — top/bottom edges of every rect.
    y_lines: Vec<f32>,
    radius: f32,
}

impl Snapper {
    /// Build from a list of window rects + the monitor's safe-area rect. Both
    /// in the same coord frame as the points the snapper will be queried with
    /// (overlay-local logical pixels for spawnhere's overlay).
    pub fn new(windows: &[Bbox], safe_area: Bbox, radius_px: f32) -> Self {
        let mut points: Vec<(f32, f32)> = Vec::with_capacity(windows.len() * 8 + 8);
        let mut x_lines: Vec<f32> = Vec::with_capacity(windows.len() * 2 + 2);
        let mut y_lines: Vec<f32> = Vec::with_capacity(windows.len() * 2 + 2);

        let push_rect = |pts: &mut Vec<(f32, f32)>,
                         xs: &mut Vec<f32>,
                         ys: &mut Vec<f32>,
                         b: Bbox| {
            let x0 = b.x as f32;
            let y0 = b.y as f32;
            let x1 = (b.x + b.w as i32) as f32;
            let y1 = (b.y + b.h as i32) as f32;
            let cx = (x0 + x1) * 0.5;
            let cy = (y0 + y1) * 0.5;
            pts.extend_from_slice(&[
                (x0, y0), (x1, y0), (x0, y1), (x1, y1), // corners
                (cx, y0), (cx, y1), (x0, cy), (x1, cy), // edge midpoints
            ]);
            xs.push(x0);
            xs.push(x1);
            ys.push(y0);
            ys.push(y1);
        };

        for w in windows {
            push_rect(&mut points, &mut x_lines, &mut y_lines, *w);
        }
        push_rect(&mut points, &mut x_lines, &mut y_lines, safe_area);

        // Dedupe lines so identical edges from adjacent windows don't get
        // weighted twice in nearest-line search.
        dedupe_close(&mut x_lines, 0.5);
        dedupe_close(&mut y_lines, 0.5);

        Self { points, x_lines, y_lines, radius: radius_px }
    }

    /// Snap a raw cursor point. The input is returned unchanged when nothing
    /// is in range; otherwise one or both axes are pulled onto the locked
    /// candidate.
    pub fn snap(&self, p: (f32, f32)) -> SnapResult {
        if self.points.is_empty() && self.x_lines.is_empty() && self.y_lines.is_empty() {
            return SnapResult::passthrough(p);
        }

        // 1) Closest point — the strongest signal. If a corner / midpoint is
        //    within the radius, it wins both axes.
        let mut best_pt: Option<((f32, f32), f32)> = None;
        let r2 = self.radius * self.radius;
        for &c in &self.points {
            let dx = c.0 - p.0;
            let dy = c.1 - p.1;
            let d2 = dx * dx + dy * dy;
            if d2 <= r2 && best_pt.is_none_or(|(_, b)| d2 < b) {
                best_pt = Some((c, d2));
            }
        }
        if let Some((q, _)) = best_pt {
            return SnapResult { point: q, x_guide: Some(q.0), y_guide: Some(q.1) };
        }

        // 2) Per-axis line snap. Each axis decides independently.
        let x_snap = nearest_within(&self.x_lines, p.0, self.radius);
        let y_snap = nearest_within(&self.y_lines, p.1, self.radius);
        let snapped = (x_snap.unwrap_or(p.0), y_snap.unwrap_or(p.1));
        SnapResult {
            point: snapped,
            x_guide: x_snap,
            y_guide: y_snap,
        }
    }
}

fn nearest_within(lines: &[f32], v: f32, radius: f32) -> Option<f32> {
    let mut best: Option<(f32, f32)> = None;
    for &l in lines {
        let d = (l - v).abs();
        if d <= radius && best.is_none_or(|(_, bd)| d < bd) {
            best = Some((l, d));
        }
    }
    best.map(|(l, _)| l)
}

fn dedupe_close(values: &mut Vec<f32>, eps: f32) {
    values.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    values.dedup_by(|a, b| (*a - *b).abs() < eps);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: i32, y: i32, w: u32, h: u32) -> Bbox {
        Bbox { x, y, w, h }
    }

    fn safe() -> Bbox {
        rect(0, 0, 1920, 1080)
    }

    #[test]
    fn empty_targets_passes_through() {
        // Even with no windows we still have the safe-area corners. Construct
        // by passing a degenerate safe area to verify the pure pass-through.
        let s = Snapper::new(&[], rect(0, 0, 0, 0), 12.0);
        // The degenerate safe-area injects (0,0) corners — but a point far
        // away from origin should pass through unchanged.
        let r = s.snap((500.0, 500.0));
        assert_eq!(r.point, (500.0, 500.0));
        assert!(!r.snapped());
    }

    #[test]
    fn locks_to_corner_within_radius() {
        let w = rect(100, 100, 400, 300);
        let s = Snapper::new(&[w], safe(), 12.0);
        // Top-left corner is (100, 100). Cursor at (105, 103) → within radius.
        let r = s.snap((105.0, 103.0));
        assert_eq!(r.point, (100.0, 100.0));
        assert_eq!(r.x_guide, Some(100.0));
        assert_eq!(r.y_guide, Some(100.0));
    }

    #[test]
    fn out_of_radius_passes_through() {
        let w = rect(100, 100, 400, 300);
        let s = Snapper::new(&[w], safe(), 12.0);
        // 50 px away from anything interesting.
        let r = s.snap((250.0, 250.0));
        assert_eq!(r.point, (250.0, 250.0));
        assert_eq!(r.x_guide, None);
        assert_eq!(r.y_guide, None);
    }

    #[test]
    fn axis_only_snap_when_only_edge_in_range() {
        let w = rect(100, 100, 400, 300);
        let s = Snapper::new(&[w], safe(), 12.0);
        // Cursor near the LEFT edge (x=100) but vertically far from any
        // corner or midpoint. Vertical edge midpoint is at y=250 (cy of a
        // 300-tall rect starting at y=100). We pick y=600 to dodge it and
        // any safe-area features.
        let r = s.snap((103.0, 600.0));
        assert_eq!(r.point.0, 100.0);
        assert_eq!(r.point.1, 600.0);
        assert_eq!(r.x_guide, Some(100.0));
        assert_eq!(r.y_guide, None);
    }

    #[test]
    fn picks_nearest_corner_when_multiple_in_range() {
        // Two windows whose corners are both within radius of the cursor.
        let a = rect(100, 100, 50, 50);
        let b = rect(160, 100, 50, 50);
        // a's right edge is at x=150, b's left edge is at x=160. Cursor at
        // (155, 100) is within radius of both top edges. Closest corner is
        // a's top-right (150, 100), 5px away.
        let s = Snapper::new(&[a, b], safe(), 12.0);
        let r = s.snap((155.0, 100.0));
        assert_eq!(r.point, (150.0, 100.0));
    }

    #[test]
    fn includes_edge_midpoints() {
        let w = rect(0, 0, 200, 100);
        let s = Snapper::new(&[w], rect(0, 0, 8000, 8000), 12.0);
        // Midpoint of top edge is (100, 0). Cursor at (101, 4) → within radius.
        let r = s.snap((101.0, 4.0));
        assert_eq!(r.point, (100.0, 0.0));
    }

    #[test]
    fn includes_safe_area_corners() {
        let s = Snapper::new(&[], rect(0, 30, 1920, 1050), 12.0);
        // Safe area's top-right corner is (1920, 30). Cursor near it.
        let r = s.snap((1915.0, 33.0));
        assert_eq!(r.point, (1920.0, 30.0));
    }

    #[test]
    fn radius_zero_disables_snap() {
        let w = rect(100, 100, 400, 300);
        let s = Snapper::new(&[w], safe(), 0.0);
        let r = s.snap((100.5, 100.5));
        // 0-radius means only an EXACT hit would lock. (100.5, 100.5) doesn't
        // match any candidate so it should pass through.
        assert_eq!(r.point, (100.5, 100.5));
        assert!(!r.snapped());
    }

    #[test]
    fn dedupes_shared_edges() {
        // Two windows sharing a common vertical edge at x=500. Without dedupe
        // we'd hold two identical candidates; the snap result is the same
        // either way, but the candidate count smaller is better for speed.
        let a = rect(100, 100, 400, 300);
        let b = rect(500, 100, 400, 300);
        let s = Snapper::new(&[a, b], safe(), 12.0);
        // x=500 appears once even though both rects contributed it.
        assert_eq!(s.x_lines.iter().filter(|&&v| (v - 500.0).abs() < 0.1).count(), 1);
    }
}
