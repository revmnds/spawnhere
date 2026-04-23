use std::collections::HashMap;
use std::path::PathBuf;
use tiny_skia::Pixmap;

/// On-demand cache: icon name → rasterized pixmap at a fixed size.
pub struct IconCache {
    size: u32,
    map: HashMap<String, Option<Pixmap>>,
}

impl IconCache {
    pub fn new(size: u32) -> Self {
        Self { size, map: HashMap::new() }
    }

    pub fn get(&mut self, name: &str) -> Option<&Pixmap> {
        if !self.map.contains_key(name) {
            let pix = lookup_and_render(name, self.size);
            self.map.insert(name.to_string(), pix);
        }
        self.map.get(name).and_then(|o| o.as_ref())
    }
}

fn lookup_and_render(name: &str, size: u32) -> Option<Pixmap> {
    let path = freedesktop_icons::lookup(name)
        .with_size(size as u16)
        .with_scale(1)
        .find()?;
    render_icon(&path, size)
}

fn render_icon(path: &PathBuf, size: u32) -> Option<Pixmap> {
    let ext = path.extension().and_then(|s| s.to_str()).unwrap_or("");
    match ext.to_lowercase().as_str() {
        "svg" | "svgz" => render_svg(path, size),
        "png" | "xpm" => render_png(path, size),
        _ => None,
    }
}

fn render_svg(path: &PathBuf, size: u32) -> Option<Pixmap> {
    let bytes = std::fs::read(path).ok()?;
    let opt = usvg::Options::default();
    let tree = usvg::Tree::from_data(&bytes, &opt).ok()?;

    let svg_size = tree.size();
    let scale = (size as f32) / svg_size.width().max(svg_size.height());
    let mut pix = Pixmap::new(size, size)?;
    let transform = tiny_skia::Transform::from_scale(scale, scale);
    resvg::render(&tree, transform, &mut pix.as_mut());
    Some(pix)
}

fn render_png(path: &PathBuf, size: u32) -> Option<Pixmap> {
    let raw = Pixmap::load_png(path).ok()?;
    if raw.width() == size && raw.height() == size {
        return Some(raw);
    }
    // Crude nearest-neighbour resize via tiny-skia draw at scale.
    let mut out = Pixmap::new(size, size)?;
    let scale_x = size as f32 / raw.width() as f32;
    let scale_y = size as f32 / raw.height() as f32;
    let pattern = tiny_skia::Pattern::new(
        raw.as_ref(),
        tiny_skia::SpreadMode::Pad,
        tiny_skia::FilterQuality::Bilinear,
        1.0,
        tiny_skia::Transform::from_scale(scale_x, scale_y),
    );
    let paint = tiny_skia::Paint { shader: pattern, ..Default::default() };
    out.fill_rect(
        tiny_skia::Rect::from_xywh(0.0, 0.0, size as f32, size as f32)?,
        &paint,
        tiny_skia::Transform::identity(),
        None,
    );
    Some(out)
}
