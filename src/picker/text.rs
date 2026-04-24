use fontdue::{Font, FontSettings};
use std::collections::HashMap;
use tiny_skia::{Pixmap, PremultipliedColorU8};

/// Inter Variable (OFL 1.1) embedded into the binary so the UI looks identical
/// on any machine, independent of what `sans-serif` resolves to on that
/// system. fontdue reads only the font's default instance (Regular wght=400);
/// Medium/SemiBold emphasis is synthesized at compositing time — see `Weight`.
const INTER_VARIABLE_TTF: &[u8] =
    include_bytes!("../../assets/fonts/Inter-Variable.ttf");

/// Text weight, applied as a second composite pass shifted one physical pixel
/// to the right. `bold_pass` is the alpha of that pass — 0 skips it entirely.
#[derive(Copy, Clone, Debug)]
pub struct Weight {
    bold_pass: f32,
}

impl Weight {
    pub const NORMAL: Weight = Weight { bold_pass: 0.0 };
    pub const MEDIUM: Weight = Weight { bold_pass: 0.55 };
    pub const SEMIBOLD: Weight = Weight { bold_pass: 1.0 };
}

pub struct TextRenderer {
    font: Font,
    cache: HashMap<(char, u32), CachedGlyph>,
}

struct CachedGlyph {
    width: usize,
    height: usize,
    xmin: i32,
    ymin: i32,
    advance: f32,
    pixels: Vec<u8>,
}

impl TextRenderer {
    pub fn new() -> Self {
        let font = Font::from_bytes(INTER_VARIABLE_TTF, FontSettings::default())
            .expect("bundled Inter-Variable.ttf fails to parse");
        Self { font, cache: HashMap::new() }
    }

    fn ascent(&self, size: f32) -> f32 {
        self.font
            .horizontal_line_metrics(size)
            .map(|m| m.ascent)
            .unwrap_or(size * 0.8)
    }

    fn glyph(&mut self, ch: char, size: f32) -> &CachedGlyph {
        // Quantize size to 1/64 px so the cache stays bounded across the
        // handful of distinct sizes the picker uses at each scale factor.
        let key = (ch, (size * 64.0).round() as u32);
        if !self.cache.contains_key(&key) {
            let (m, pixels) = self.font.rasterize(ch, size);
            self.cache.insert(
                key,
                CachedGlyph {
                    width: m.width,
                    height: m.height,
                    xmin: m.xmin,
                    ymin: m.ymin,
                    advance: m.advance_width,
                    pixels,
                },
            );
        }
        &self.cache[&key]
    }

    pub fn measure_width(&mut self, text: &str, size: f32) -> f32 {
        self.measure_width_weighted(text, size, Weight::NORMAL)
    }

    pub fn measure_width_weighted(&mut self, text: &str, size: f32, weight: Weight) -> f32 {
        if text.is_empty() {
            return 0.0;
        }
        let mut pen = 0.0f32;
        for ch in text.chars() {
            pen += self.glyph(ch, size).advance;
        }
        // Synthetic bold extends each glyph 1 px to the right; account for the
        // trailing overhang so callers laying out adjacent elements don't
        // overlap them.
        if weight.bold_pass > 0.0 {
            pen += 1.0;
        }
        pen
    }

    #[allow(clippy::too_many_arguments)]
    pub fn draw(
        &mut self,
        pixmap: &mut Pixmap,
        x: i32,
        y: i32,
        text: &str,
        size: f32,
        max_width: f32,
        color: (u8, u8, u8, u8),
    ) {
        self.draw_weighted(pixmap, x, y, text, size, max_width, color, Weight::NORMAL);
    }

    #[allow(clippy::too_many_arguments)]
    pub fn draw_weighted(
        &mut self,
        pixmap: &mut Pixmap,
        x: i32,
        y: i32,
        text: &str,
        size: f32,
        max_width: f32,
        color: (u8, u8, u8, u8),
        weight: Weight,
    ) {
        if text.is_empty() {
            return;
        }
        let ascent = self.ascent(size);
        let baseline_y = y as f32 + ascent;
        let stop_at = x as f32 + max_width;

        // Collect glyph plan first so we can drop the &mut borrow on the cache
        // before compositing into `pixmap.pixels_mut()`.
        struct Placed {
            x: i32,
            y: i32,
            w: usize,
            h: usize,
            bitmap: Vec<u8>,
        }
        let mut plan: Vec<Placed> = Vec::with_capacity(text.len());
        let mut pen = x as f32;
        for ch in text.chars() {
            let g = self.glyph(ch, size);
            if pen + g.advance > stop_at {
                break;
            }
            let gx = (pen + g.xmin as f32).round() as i32;
            let gy = (baseline_y - g.height as f32 - g.ymin as f32).round() as i32;
            plan.push(Placed {
                x: gx,
                y: gy,
                w: g.width,
                h: g.height,
                bitmap: g.pixels.clone(),
            });
            pen += g.advance;
        }

        let pixmap_w = pixmap.width() as i32;
        let pixmap_h = pixmap.height() as i32;
        let stride = pixmap.width() as usize;
        let pixels = pixmap.pixels_mut();

        for placed in &plan {
            composite_glyph(
                pixels, pixmap_w, pixmap_h, stride,
                placed.x, placed.y, placed.w, placed.h, &placed.bitmap,
                color, 1.0,
            );
            if weight.bold_pass > 0.0 {
                composite_glyph(
                    pixels, pixmap_w, pixmap_h, stride,
                    placed.x + 1, placed.y, placed.w, placed.h, &placed.bitmap,
                    color, weight.bold_pass,
                );
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn composite_glyph(
    pixels: &mut [PremultipliedColorU8],
    pmw: i32,
    pmh: i32,
    stride: usize,
    x0: i32,
    y0: i32,
    gw: usize,
    gh: usize,
    bmp: &[u8],
    color: (u8, u8, u8, u8),
    alpha_mul: f32,
) {
    let (cr, cg, cb, ca) = color;
    let text_alpha = (ca as f32 * alpha_mul).round().clamp(0.0, 255.0) as u32;
    if text_alpha == 0 {
        return;
    }
    for row in 0..gh as i32 {
        let py = y0 + row;
        if py < 0 || py >= pmh {
            continue;
        }
        for col in 0..gw as i32 {
            let cov = bmp[(row as usize) * gw + col as usize] as u32;
            if cov == 0 {
                continue;
            }
            let px = x0 + col;
            if px < 0 || px >= pmw {
                continue;
            }
            // Effective alpha = coverage × text alpha (both 0..=255).
            let src_a = (cov * text_alpha) / 255;
            if src_a == 0 {
                continue;
            }
            let idx = (py as usize) * stride + (px as usize);
            let dst = &mut pixels[idx];
            if src_a == 255 {
                *dst = PremultipliedColorU8::from_rgba(cr, cg, cb, 255).unwrap();
                continue;
            }
            let inv = 255 - src_a;
            let dst_rgba = dst.demultiply();
            let r = ((cr as u32 * src_a + dst_rgba.red() as u32 * inv) / 255) as u8;
            let g = ((cg as u32 * src_a + dst_rgba.green() as u32 * inv) / 255) as u8;
            let b = ((cb as u32 * src_a + dst_rgba.blue() as u32 * inv) / 255) as u8;
            let a = (src_a + (dst_rgba.alpha() as u32 * inv) / 255) as u8;
            *dst = PremultipliedColorU8::from_rgba(r, g, b, a).unwrap_or(*dst);
        }
    }
}
