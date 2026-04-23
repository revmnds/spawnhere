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
}
