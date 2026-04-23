use cosmic_text::{
    Attrs, Buffer, Color as CtColor, Family, FontSystem, Metrics, Shaping, SwashCache, Weight,
};
use tiny_skia::Pixmap;

/// Inter Variable (OFL 1.1) embedded into the binary so the UI looks identical
/// on any machine, independent of what `sans-serif` resolves to on that
/// system. One variable file covers all weights the picker uses.
const INTER_VARIABLE_TTF: &[u8] =
    include_bytes!("../../assets/fonts/Inter-Variable.ttf");

pub struct TextRenderer {
    font_system: FontSystem,
    swash_cache: SwashCache,
}

impl TextRenderer {
    pub fn new() -> Self {
        let mut font_system = FontSystem::new();
        font_system.db_mut().load_font_data(INTER_VARIABLE_TTF.to_vec());
        Self {
            font_system,
            swash_cache: SwashCache::new(),
        }
    }

    fn attrs(weight: Weight) -> Attrs<'static> {
        Attrs::new().family(Family::Name("Inter")).weight(weight)
    }

    /// Returns the laid-out width of `text` at `size`, in pixels.
    pub fn measure_width(&mut self, text: &str, size: f32) -> f32 {
        self.measure_width_weighted(text, size, Weight::NORMAL)
    }

    pub fn measure_width_weighted(&mut self, text: &str, size: f32, weight: Weight) -> f32 {
        if text.is_empty() {
            return 0.0;
        }
        let metrics = Metrics::new(size, size * 1.4);
        let mut buffer = Buffer::new(&mut self.font_system, metrics);
        buffer.set_size(&mut self.font_system, None, None);
        buffer.set_text(&mut self.font_system, text, Self::attrs(weight), Shaping::Advanced);
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
        let metrics = Metrics::new(size, size * 1.4);
        let mut buffer = Buffer::new(&mut self.font_system, metrics);
        buffer.set_size(&mut self.font_system, Some(max_width), Some(size * 1.6));
        buffer.set_text(&mut self.font_system, text, Self::attrs(weight), Shaping::Advanced);
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
