use cosmic_text::{
    Attrs, Buffer, Color as CtColor, Family, FontSystem, Metrics, Shaping, SwashCache,
};
use tiny_skia::Pixmap;

pub struct TextRenderer {
    font_system: FontSystem,
    swash_cache: SwashCache,
}

impl TextRenderer {
    pub fn new() -> Self {
        Self {
            font_system: FontSystem::new(),
            swash_cache: SwashCache::new(),
        }
    }

    /// Returns the laid-out width of `text` at `size`, in pixels.
    pub fn measure_width(&mut self, text: &str, size: f32) -> f32 {
        if text.is_empty() {
            return 0.0;
        }
        let metrics = Metrics::new(size, size * 1.4);
        let mut buffer = Buffer::new(&mut self.font_system, metrics);
        buffer.set_size(&mut self.font_system, None, None);
        let attrs = Attrs::new().family(Family::SansSerif);
        buffer.set_text(&mut self.font_system, text, attrs, Shaping::Advanced);
        buffer.shape_until_scroll(&mut self.font_system, false);
        buffer
            .layout_runs()
            .map(|r| r.line_w)
            .fold(0.0_f32, f32::max)
    }

    /// Draws `text` into `pixmap` with the top-left of the text bounding box
    /// at `(x, y)`. `max_width` clips the layout horizontally.
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
        let metrics = Metrics::new(size, size * 1.4);
        let mut buffer = Buffer::new(&mut self.font_system, metrics);
        buffer.set_size(&mut self.font_system, Some(max_width), Some(size * 1.6));
        let attrs = Attrs::new().family(Family::SansSerif);
        buffer.set_text(&mut self.font_system, text, attrs, Shaping::Advanced);
        buffer.shape_until_scroll(&mut self.font_system, false);

        let ct_color = CtColor::rgba(color.0, color.1, color.2, color.3);

        let pixmap_w = pixmap.width() as i32;
        let pixmap_h = pixmap.height() as i32;
        let stride = pixmap.width() as usize;
        let pixels = pixmap.pixels_mut();

        buffer.draw(
            &mut self.font_system,
            &mut self.swash_cache,
            ct_color,
            |gx, gy, gw, gh, gcolor| {
                for dy in 0..gh as i32 {
                    for dx in 0..gw as i32 {
                        let px = x + gx + dx;
                        let py = y + gy + dy;
                        if px < 0 || py < 0 || px >= pixmap_w || py >= pixmap_h {
                            continue;
                        }
                        let idx = (py as usize) * stride + (px as usize);
                        let dst = &mut pixels[idx];
                        // Source is premultiplied RGBA from cosmic-text/swash.
                        // Destination is non-premultiplied RGBA in tiny-skia Pixmap.
                        // For simplicity: alpha-blend assuming src is straight alpha.
                        let src_a = gcolor.a() as u32;
                        if src_a == 0 {
                            continue;
                        }
                        if src_a == 255 {
                            *dst = tiny_skia::PremultipliedColorU8::from_rgba(
                                gcolor.r(),
                                gcolor.g(),
                                gcolor.b(),
                                255,
                            )
                            .unwrap();
                            continue;
                        }
                        // src over dst (straight alpha)
                        let inv = 255 - src_a;
                        let dst_rgba = dst.demultiply();
                        let r = ((gcolor.r() as u32 * src_a + dst_rgba.red() as u32 * inv) / 255) as u8;
                        let g = ((gcolor.g() as u32 * src_a + dst_rgba.green() as u32 * inv) / 255) as u8;
                        let b = ((gcolor.b() as u32 * src_a + dst_rgba.blue() as u32 * inv) / 255) as u8;
                        let a = (src_a + (dst_rgba.alpha() as u32 * inv) / 255) as u8;
                        *dst = tiny_skia::PremultipliedColorU8::from_rgba(r, g, b, a)
                            .unwrap_or(*dst);
                    }
                }
            },
        );
    }
}
