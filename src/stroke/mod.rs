use std::time::Instant;

#[derive(Clone, Copy, Debug)]
pub struct Point {
    pub x: f32,
    pub y: f32,
    /// Seconds since stroke start. Unused in M1; used for velocity-based stroke width in M2.
    #[allow(dead_code)]
    pub t: f32,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Bbox {
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
}

impl Bbox {
    pub fn enforce_min(mut self, min_w: u32, min_h: u32) -> Self {
        if self.w < min_w {
            let diff = (min_w - self.w) as i32;
            self.x -= diff / 2;
            self.w = min_w;
        }
        if self.h < min_h {
            let diff = (min_h - self.h) as i32;
            self.y -= diff / 2;
            self.h = min_h;
        }
        self
    }

    /// Keep the bbox within an arbitrary output rect in the same coord frame.
    /// Handles both single-monitor setups (rect at origin) and multi-monitor
    /// (rect at a non-zero global position). If the bbox is larger than the
    /// rect, shrink + origin-clamp so the spawned window is at least fully
    /// visible on that output.
    pub fn clamp_to_rect(mut self, rect: Bbox) -> Self {
        let rx = rect.x;
        let ry = rect.y;
        let rw = rect.w as i32;
        let rh = rect.h as i32;
        if self.w as i32 > rw {
            self.w = rect.w;
            self.x = rx;
        } else {
            if self.x < rx {
                self.x = rx;
            }
            if self.x + self.w as i32 > rx + rw {
                self.x = rx + rw - self.w as i32;
            }
        }
        if self.h as i32 > rh {
            self.h = rect.h;
            self.y = ry;
        } else {
            if self.y < ry {
                self.y = ry;
            }
            if self.y + self.h as i32 > ry + rh {
                self.y = ry + rh - self.h as i32;
            }
        }
        self
    }

}

pub struct Stroke {
    points: Vec<Point>,
    started: Option<Instant>,
}

impl Stroke {
    pub fn new() -> Self {
        Self {
            points: Vec::with_capacity(512),
            started: None,
        }
    }

    pub fn clear(&mut self) {
        self.points.clear();
        self.started = None;
    }

    pub fn push(&mut self, x: f32, y: f32) {
        let now = Instant::now();
        let t = match self.started {
            Some(s) => now.duration_since(s).as_secs_f32(),
            None => {
                self.started = Some(now);
                0.0
            }
        };
        self.points.push(Point { x, y, t });
    }

    pub fn points(&self) -> &[Point] {
        &self.points
    }

    pub fn bbox(&self, padding: u32) -> Bbox {
        if self.points.is_empty() {
            return Bbox::default();
        }
        let (mut x_min, mut y_min) = (f32::INFINITY, f32::INFINITY);
        let (mut x_max, mut y_max) = (f32::NEG_INFINITY, f32::NEG_INFINITY);
        for p in &self.points {
            if p.x < x_min {
                x_min = p.x;
            }
            if p.y < y_min {
                y_min = p.y;
            }
            if p.x > x_max {
                x_max = p.x;
            }
            if p.y > y_max {
                y_max = p.y;
            }
        }
        let pad = padding as f32;
        Bbox {
            x: (x_min - pad).round() as i32,
            y: (y_min - pad).round() as i32,
            w: ((x_max - x_min) + 2.0 * pad).round().max(0.0) as u32,
            h: ((y_max - y_min) + 2.0 * pad).round().max(0.0) as u32,
        }
    }
}

impl Default for Stroke {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bbox_of_empty_stroke_is_zero() {
        let s = Stroke::new();
        let b = s.bbox(0);
        assert_eq!(b.w, 0);
        assert_eq!(b.h, 0);
    }

    #[test]
    fn bbox_encloses_all_points() {
        let mut s = Stroke::new();
        s.push(100.0, 200.0);
        s.push(300.0, 150.0);
        s.push(250.0, 400.0);
        let b = s.bbox(0);
        assert_eq!(b.x, 100);
        assert_eq!(b.y, 150);
        assert_eq!(b.w, 200);
        assert_eq!(b.h, 250);
    }

    #[test]
    fn bbox_applies_padding_both_sides() {
        let mut s = Stroke::new();
        s.push(100.0, 100.0);
        s.push(200.0, 200.0);
        let b = s.bbox(10);
        assert_eq!(b.x, 90);
        assert_eq!(b.y, 90);
        assert_eq!(b.w, 120);
        assert_eq!(b.h, 120);
    }

    #[test]
    fn enforce_min_grows_and_recenters() {
        let b = Bbox { x: 100, y: 100, w: 50, h: 50 };
        let m = b.enforce_min(200, 200);
        assert_eq!(m.w, 200);
        assert_eq!(m.h, 200);
        assert_eq!(m.x, 25);
        assert_eq!(m.y, 25);
    }

    #[test]
    fn enforce_min_keeps_larger() {
        let b = Bbox { x: 0, y: 0, w: 500, h: 500 };
        let m = b.enforce_min(200, 200);
        assert_eq!(m.w, 500);
        assert_eq!(m.h, 500);
        assert_eq!(m.x, 0);
        assert_eq!(m.y, 0);
    }

    #[test]
    fn clamp_slides_right_edge_overflow_back_in() {
        // Single-monitor bug regression: stroke near right edge + enforce_min
        // grew it past screen width. Expect the box to slide left.
        let screen = Bbox { x: 0, y: 0, w: 1920, h: 1080 };
        let b = Bbox { x: 1550, y: 500, w: 400, h: 300 };
        let c = b.clamp_to_rect(screen);
        assert_eq!(c.x + c.w as i32, 1920);
        assert_eq!(c.w, 400);
    }

    #[test]
    fn clamp_handles_negative_origin() {
        let screen = Bbox { x: 0, y: 0, w: 1920, h: 1080 };
        let b = Bbox { x: -50, y: -30, w: 400, h: 300 };
        let c = b.clamp_to_rect(screen);
        assert_eq!(c.x, 0);
        assert_eq!(c.y, 0);
    }

    #[test]
    fn clamp_shrinks_oversized_bbox() {
        let screen = Bbox { x: 0, y: 0, w: 1920, h: 1080 };
        let b = Bbox { x: 0, y: 0, w: 3000, h: 2000 };
        let c = b.clamp_to_rect(screen);
        assert_eq!(c.w, 1920);
        assert_eq!(c.h, 1080);
        assert_eq!(c.x, 0);
        assert_eq!(c.y, 0);
    }

    #[test]
    fn clamp_to_rect_respects_non_zero_origin() {
        // Second monitor sits at x=1920. A stroke centered there with min
        // enforcement pushing past its right edge should slide back into that
        // monitor, not into monitor 1.
        let monitor = Bbox { x: 1920, y: 0, w: 1920, h: 1080 };
        let b = Bbox { x: 3700, y: 500, w: 400, h: 300 };
        let c = b.clamp_to_rect(monitor);
        assert_eq!(c.x + c.w as i32, monitor.x + monitor.w as i32);
        assert_eq!(c.w, 400);
    }

    #[test]
    fn clamp_to_rect_handles_left_overshoot_on_second_monitor() {
        // bbox origin is to the LEFT of the monitor — slide in.
        let monitor = Bbox { x: 1920, y: 0, w: 1920, h: 1080 };
        let b = Bbox { x: 1800, y: 500, w: 400, h: 300 };
        let c = b.clamp_to_rect(monitor);
        assert_eq!(c.x, 1920);
        assert_eq!(c.w, 400);
    }

}
