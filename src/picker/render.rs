use super::{PickerState, TextRenderer, VISIBLE_ITEMS};
use crate::apps::IconCache;
use tiny_skia::{
    FillRule, Paint, PathBuilder, Pixmap, PixmapPaint, Rect, Transform,
};

pub const CARD_WIDTH: u32 = 600;

const CARD_PADDING: i32 = 20;
const SEARCH_HEIGHT: i32 = 48;
const SEPARATOR_GAP: i32 = 12;
const ITEM_HEIGHT: i32 = 40;
const ICON_SIZE: u32 = 24;
const SCROLL_RAIL_W: i32 = 3;
const SCROLL_RAIL_GAP: i32 = 4;
/// Vertical space for the "Recent" section header. Only added when at least
/// one previously-picked app is visible.
const SECTION_HEADER_H: i32 = 22;
/// Square hit-box for the × "forget" button on history rows (logical px).
const FORGET_BTN_SIZE: i32 = 22;
/// Right-edge inset for the × button within a row.
const FORGET_BTN_INSET: i32 = 6;

// All layout math in this file is in **logical pixels**. Physical-pixel
// scaling happens at the edges:
//   * tiny-skia shape/rect calls receive `Transform::from_scale(scale, scale)`
//     so logical coords land at the right physical pixels.
//   * `TextRenderer::draw` renders glyphs by writing into the pixmap directly,
//     so callers pre-multiply position + font size by `scale`.
//   * `IconCache` is rebuilt at `ICON_SIZE * scale` when scale changes, so
//     icons are already at physical size; `draw_pixmap` gets physical (x, y)
//     with identity transform.

fn list_rows(state: &PickerState) -> i32 {
    if state.loading() || state.match_count() == 0 {
        1
    } else {
        state.visible_count() as i32
    }
}

/// Total extra vertical space needed for the Recent section headers. Returns
/// (recent_hdr, others_hdr) so callers know whether each is shown.
fn section_headers(state: &PickerState) -> (bool, bool) {
    let recent = state.visible_recent_count();
    if recent == 0 {
        return (false, false);
    }
    // Show "Other apps" header only if there's a non-recent tail in view.
    let show_others = state.visible_count() > recent;
    (true, show_others)
}

fn card_height(state: &PickerState) -> i32 {
    let (show_recent, show_others) = section_headers(state);
    let hdr_space = (show_recent as i32 + show_others as i32) * SECTION_HEADER_H;
    CARD_PADDING * 2 + SEARCH_HEIGHT + SEPARATOR_GAP + hdr_space + list_rows(state) * ITEM_HEIGHT
}

#[derive(Clone, Copy, Debug)]
pub struct CardRect {
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
    /// Absolute y where the Recent section header starts (None if no recents).
    pub recent_header_y: Option<i32>,
    /// Absolute y where the "Other apps" section header starts (None if absent).
    pub others_header_y: Option<i32>,
    /// Number of visible recent rows; informs `item_at` skip-over math.
    pub recent_count: u32,
}

impl CardRect {
    pub fn contains(&self, px: i32, py: i32) -> bool {
        px >= self.x
            && px < self.x + self.w as i32
            && py >= self.y
            && py < self.y + self.h as i32
    }

    /// Returns the relative index into the visible slice clicked, if any.
    /// Skips the header strips so a click on "Recent" doesn't spawn anything.
    pub fn item_at(&self, py: i32, visible_count: usize) -> Option<usize> {
        let first_row = self.y
            + CARD_PADDING
            + SEARCH_HEIGHT
            + SEPARATOR_GAP
            + if self.recent_header_y.is_some() { SECTION_HEADER_H } else { 0 };
        if py < first_row {
            return None;
        }
        let recent = self.recent_count as i32;
        let post_recent = first_row + recent * ITEM_HEIGHT;
        if self.others_header_y.is_some() && py >= post_recent && py < post_recent + SECTION_HEADER_H
        {
            // Click landed on the "Other apps" header strip.
            return None;
        }
        let row_y = if py >= post_recent + if self.others_header_y.is_some() { SECTION_HEADER_H } else { 0 } {
            py - if self.others_header_y.is_some() { SECTION_HEADER_H } else { 0 }
        } else {
            py
        };
        let local = row_y - first_row;
        if local < 0 {
            return None;
        }
        let idx = (local / ITEM_HEIGHT) as usize;
        if idx < visible_count {
            Some(idx)
        } else {
            None
        }
    }

    /// True if the click landed inside the × forget button of a row. The
    /// caller still needs to confirm the row is a history row — the × is
    /// hidden for non-history rows but the hit-box is cheap either way.
    pub fn forget_button_hit(&self, px: i32, _py: i32) -> bool {
        let right = self.x + self.w as i32 - CARD_PADDING - FORGET_BTN_INSET;
        let left = right - FORGET_BTN_SIZE;
        px >= left && px < right
    }
}

#[allow(clippy::too_many_arguments)]
pub fn draw(
    pixmap: &mut Pixmap,
    state: &PickerState,
    text: &mut TextRenderer,
    icons: &mut IconCache,
    bbox_center: (i32, i32),
    surface_size: (u32, u32),
    caret_visible: bool,
    hovered_rel: Option<usize>,
    scale: u32,
) -> CardRect {
    let cw = CARD_WIDTH as i32;
    let ch = card_height(state);
    let (cx, cy) = bbox_center;
    let margin = 20;
    let (sw, sh) = (surface_size.0 as i32, surface_size.1 as i32);

    let raw_x = cx - cw / 2;
    let raw_y = cy - ch / 2;
    let x = raw_x.clamp(margin, (sw - cw - margin).max(margin));
    let y = raw_y.clamp(margin, (sh - ch - margin).max(margin));

    draw_card_bg(pixmap, x, y, cw, ch, scale);
    draw_search(
        pixmap,
        text,
        x + CARD_PADDING,
        y + CARD_PADDING,
        cw - 2 * CARD_PADDING,
        state.query(),
        state.loading(),
        caret_visible,
        scale,
    );
    draw_separator(
        pixmap,
        x + CARD_PADDING,
        y + CARD_PADDING + SEARCH_HEIGHT + SEPARATOR_GAP / 2,
        cw - 2 * CARD_PADDING,
        scale,
    );
    let (show_recent, show_others) = section_headers(state);
    let recent_count = state.visible_recent_count() as u32;
    let list_x = x + CARD_PADDING;
    let list_w = cw - 2 * CARD_PADDING;
    let mut cursor_y = y + CARD_PADDING + SEARCH_HEIGHT + SEPARATOR_GAP;
    let recent_header_y = if show_recent {
        let hy = cursor_y;
        draw_section_header(pixmap, text, list_x, hy, list_w, "Recent", scale);
        cursor_y += SECTION_HEADER_H;
        Some(hy)
    } else {
        None
    };
    let recent_rows_end = cursor_y + recent_count as i32 * ITEM_HEIGHT;
    let others_header_y = if show_others {
        let hy = recent_rows_end;
        draw_section_header(pixmap, text, list_x, hy, list_w, "Other apps", scale);
        Some(hy)
    } else {
        None
    };
    draw_list(
        pixmap,
        text,
        icons,
        state,
        list_x,
        cursor_y,
        list_w,
        hovered_rel,
        scale,
        recent_count as usize,
        show_others,
    );

    CardRect {
        x,
        y,
        w: cw as u32,
        h: ch as u32,
        recent_header_y,
        others_header_y,
        recent_count,
    }
}

fn scale_tf(scale: u32) -> Transform {
    let s = scale as f32;
    Transform::from_scale(s, s)
}

fn draw_card_bg(pixmap: &mut Pixmap, x: i32, y: i32, w: i32, h: i32, scale: u32) {
    let t = scale_tf(scale);
    let mut bg = Paint::default();
    bg.set_color_rgba8(26, 27, 34, 238);
    bg.anti_alias = true;

    if let Some(path) = rounded_rect_path(x as f32, y as f32, w as f32, h as f32, 12.0) {
        pixmap.fill_path(&path, &bg, FillRule::Winding, t, None);

        let mut border = Paint::default();
        border.set_color_rgba8(176, 128, 255, 80);
        border.anti_alias = true;
        let stroke = tiny_skia::Stroke { width: 1.0, ..Default::default() };
        pixmap.stroke_path(&path, &border, &stroke, t, None);
    }
}

fn rounded_rect_path(x: f32, y: f32, w: f32, h: f32, r: f32) -> Option<tiny_skia::Path> {
    let mut pb = PathBuilder::new();
    pb.move_to(x + r, y);
    pb.line_to(x + w - r, y);
    pb.quad_to(x + w, y, x + w, y + r);
    pb.line_to(x + w, y + h - r);
    pb.quad_to(x + w, y + h, x + w - r, y + h);
    pb.line_to(x + r, y + h);
    pb.quad_to(x, y + h, x, y + h - r);
    pb.line_to(x, y + r);
    pb.quad_to(x, y, x + r, y);
    pb.close();
    pb.finish()
}

#[allow(clippy::too_many_arguments)]
fn draw_search(
    pixmap: &mut Pixmap,
    text: &mut TextRenderer,
    x: i32,
    y: i32,
    w: i32,
    query: &str,
    loading: bool,
    caret_visible: bool,
    scale: u32,
) {
    let t = scale_tf(scale);
    let s_i = scale as i32;
    let s_f = scale as f32;

    let mut bg = Paint::default();
    bg.set_color_rgba8(37, 38, 47, 255);
    bg.anti_alias = true;
    if let Some(path) =
        rounded_rect_path(x as f32, y as f32, w as f32, SEARCH_HEIGHT as f32, 8.0)
    {
        pixmap.fill_path(&path, &bg, FillRule::Winding, t, None);
    }

    let prompt_x = x + 14;
    let prompt_y = y + 12;
    text.draw(
        pixmap,
        prompt_x * s_i,
        prompt_y * s_i,
        "🔍",
        18.0 * s_f,
        24.0 * s_f,
        (176, 128, 255, 255),
    );

    let text_x = x + 50;
    let text_y = y + 12;
    let query_width_phys = if query.is_empty() {
        let placeholder = if loading {
            "Loading apps…"
        } else {
            "Type to search"
        };
        text.draw(
            pixmap,
            text_x * s_i,
            text_y * s_i,
            placeholder,
            18.0 * s_f,
            (w - 60) as f32 * s_f,
            (130, 130, 140, 255),
        );
        0.0
    } else {
        text.draw(
            pixmap,
            text_x * s_i,
            text_y * s_i,
            query,
            18.0 * s_f,
            (w - 60) as f32 * s_f,
            (235, 235, 240, 255),
        );
        text.measure_width(query, 18.0 * s_f)
    };

    if caret_visible {
        // Caret position is in PHYSICAL pixels because query_width_phys is
        // physical. Draw with identity transform against the physical pixmap.
        let caret_x_phys = text_x as f32 * s_f + query_width_phys + 1.0 * s_f;
        let caret_y_phys = (text_y as f32 + 4.0) * s_f;
        let caret_w_phys = 2.0 * s_f;
        let caret_h_phys = 22.0 * s_f;
        let mut caret = Paint::default();
        caret.set_color_rgba8(176, 128, 255, 255);
        if let Some(rect) =
            Rect::from_xywh(caret_x_phys, caret_y_phys, caret_w_phys, caret_h_phys)
        {
            pixmap.fill_rect(rect, &caret, Transform::identity(), None);
        }
    }
}

fn draw_separator(pixmap: &mut Pixmap, x: i32, y: i32, w: i32, scale: u32) {
    let t = scale_tf(scale);
    let mut paint = Paint::default();
    paint.set_color_rgba8(60, 62, 75, 255);
    if let Some(rect) = Rect::from_xywh(x as f32, y as f32, w as f32, 1.0) {
        pixmap.fill_rect(rect, &paint, t, None);
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_list(
    pixmap: &mut Pixmap,
    text: &mut TextRenderer,
    icons: &mut IconCache,
    state: &PickerState,
    x: i32,
    y_start: i32,
    w: i32,
    hovered_rel: Option<usize>,
    scale: u32,
    recent_count: usize,
    show_others_header: bool,
) {
    let t = scale_tf(scale);
    let s_i = scale as i32;
    let s_f = scale as f32;

    if state.loading() {
        return;
    }

    if state.match_count() == 0 {
        text.draw(
            pixmap,
            (x + 8) * s_i,
            (y_start + 10) * s_i,
            "No results",
            14.0 * s_f,
            (w - 16) as f32 * s_f,
            (130, 130, 140, 255),
        );
        return;
    }

    let total = state.match_count();
    let needs_rail = total > VISIBLE_ITEMS;
    let list_w = if needs_rail {
        w - SCROLL_RAIL_W - SCROLL_RAIL_GAP
    } else {
        w
    };

    for (i, (abs, app, selected)) in state.visible().enumerate() {
        // The "Other apps" header sits between recent rows and the rest; shift
        // subsequent rows down by its height.
        let header_offset = if show_others_header && i >= recent_count {
            SECTION_HEADER_H
        } else {
            0
        };
        let row_y = y_start + i as i32 * ITEM_HEIGHT + header_offset;
        let is_hovered = hovered_rel == Some(i);

        if selected {
            let mut hi = Paint::default();
            hi.set_color_rgba8(176, 128, 255, 110);
            if let Some(path) = rounded_rect_path(
                x as f32,
                row_y as f32 + 2.0,
                list_w as f32,
                ITEM_HEIGHT as f32 - 4.0,
                6.0,
            ) {
                pixmap.fill_path(&path, &hi, FillRule::Winding, t, None);
            }
            let mut bar = Paint::default();
            bar.set_color_rgba8(176, 128, 255, 255);
            if let Some(rect) = Rect::from_xywh(
                x as f32,
                row_y as f32 + 6.0,
                3.0,
                (ITEM_HEIGHT - 12) as f32,
            ) {
                pixmap.fill_rect(rect, &bar, t, None);
            }
        } else if is_hovered {
            let mut hv = Paint::default();
            hv.set_color_rgba8(255, 255, 255, 16);
            if let Some(path) = rounded_rect_path(
                x as f32,
                row_y as f32 + 2.0,
                list_w as f32,
                ITEM_HEIGHT as f32 - 4.0,
                6.0,
            ) {
                pixmap.fill_path(&path, &hv, FillRule::Winding, t, None);
            }
        }

        let icon_x = x + 8;
        let icon_y = row_y + (ITEM_HEIGHT - ICON_SIZE as i32) / 2;
        let mut drew_icon = false;
        if let Some(icon_name) = app.icon.as_deref() {
            if let Some(icon) = icons.get(icon_name) {
                // Icon is already at physical size (IconCache rebuilt per scale);
                // draw at physical position with identity transform.
                let paint = PixmapPaint::default();
                pixmap.draw_pixmap(
                    icon_x * s_i,
                    icon_y * s_i,
                    icon.as_ref(),
                    &paint,
                    Transform::identity(),
                    None,
                );
                drew_icon = true;
            }
        }
        if !drew_icon {
            draw_fallback_icon(pixmap, text, icon_x, icon_y, &app.name, abs, scale);
        }

        let name_x = icon_x + ICON_SIZE as i32 + 12;
        let name_y = row_y + 10;
        let name_color = if selected {
            (255, 255, 255, 255)
        } else {
            (220, 220, 225, 255)
        };

        // Reserve space for the × button if this row has history, so names
        // don't overlap it. Button is only drawn on hover / selection, but
        // the space reservation is constant to avoid reflow on hover.
        let is_history = state.is_history_row(abs);
        let name_right_reserve = if is_history {
            FORGET_BTN_SIZE + FORGET_BTN_INSET + 4
        } else {
            8
        };
        text.draw(
            pixmap,
            name_x * s_i,
            name_y * s_i,
            &app.name,
            14.0 * s_f,
            (list_w - (name_x - x) - name_right_reserve) as f32 * s_f,
            name_color,
        );

        // × button — visible on selected OR hovered rows that have history.
        if is_history && (selected || is_hovered) {
            let btn_right = x + list_w - FORGET_BTN_INSET;
            let btn_left = btn_right - FORGET_BTN_SIZE;
            let btn_top = row_y + (ITEM_HEIGHT - FORGET_BTN_SIZE) / 2;
            draw_forget_button(pixmap, btn_left, btn_top, FORGET_BTN_SIZE, selected, scale);
        }
    }

    if needs_rail {
        draw_scroll_rail(pixmap, state, x + w - SCROLL_RAIL_W, y_start, total, scale);
    }
}

fn draw_forget_button(pixmap: &mut Pixmap, x: i32, y: i32, size: i32, prominent: bool, scale: u32) {
    let t = scale_tf(scale);
    // Subtle filled circle behind the ×. Brighter when the row is selected.
    let (bg_r, bg_g, bg_b, bg_a) = if prominent {
        (176, 128, 255, 96)
    } else {
        (180, 180, 200, 60)
    };
    let mut bg = Paint::default();
    bg.set_color_rgba8(bg_r, bg_g, bg_b, bg_a);
    bg.anti_alias = true;
    if let Some(path) = rounded_rect_path(x as f32, y as f32, size as f32, size as f32, (size / 2) as f32)
    {
        pixmap.fill_path(&path, &bg, FillRule::Winding, t, None);
    }

    // × itself — two crossed strokes.
    let mut fg = Paint::default();
    fg.set_color_rgba8(235, 235, 240, 255);
    fg.anti_alias = true;
    let stroke = tiny_skia::Stroke { width: 1.6, ..Default::default() };
    let pad = 6.0;
    let mut pb = PathBuilder::new();
    pb.move_to(x as f32 + pad, y as f32 + pad);
    pb.line_to(x as f32 + size as f32 - pad, y as f32 + size as f32 - pad);
    pb.move_to(x as f32 + size as f32 - pad, y as f32 + pad);
    pb.line_to(x as f32 + pad, y as f32 + size as f32 - pad);
    if let Some(path) = pb.finish() {
        pixmap.stroke_path(&path, &fg, &stroke, t, None);
    }
}

fn draw_fallback_icon(
    pixmap: &mut Pixmap,
    text: &mut TextRenderer,
    x: i32,
    y: i32,
    name: &str,
    seed: usize,
    scale: u32,
) {
    let t = scale_tf(scale);
    let s_i = scale as i32;
    let s_f = scale as f32;
    let hues = [
        (120, 108, 180),
        (146, 118, 180),
        (108, 140, 180),
        (180, 128, 160),
        (140, 160, 180),
        (170, 140, 110),
    ];
    let (r, g, b) = hues[seed % hues.len()];
    let mut bg = Paint::default();
    bg.set_color_rgba8(r, g, b, 200);
    bg.anti_alias = true;
    if let Some(path) = rounded_rect_path(x as f32, y as f32, ICON_SIZE as f32, ICON_SIZE as f32, 6.0) {
        pixmap.fill_path(&path, &bg, FillRule::Winding, t, None);
    }
    let letter: String = name
        .chars()
        .find(|c| c.is_alphanumeric())
        .map(|c| c.to_uppercase().to_string())
        .unwrap_or_else(|| "?".to_string());
    let glyph_w_phys = text.measure_width(&letter, 14.0 * s_f);
    let tx_phys = x as f32 * s_f + (ICON_SIZE as f32 * s_f - glyph_w_phys) / 2.0;
    let ty = y + 4;
    text.draw(
        pixmap,
        tx_phys as i32,
        ty * s_i,
        &letter,
        14.0 * s_f,
        ICON_SIZE as f32 * s_f,
        (250, 250, 255, 255),
    );
}

fn draw_section_header(
    pixmap: &mut Pixmap,
    text: &mut TextRenderer,
    x: i32,
    y: i32,
    _w: i32,
    label: &str,
    scale: u32,
) {
    let s_i = scale as i32;
    let s_f = scale as f32;
    // Faint uppercase label — design cue that this is a grouping, not an app row.
    text.draw(
        pixmap,
        (x + 6) * s_i,
        (y + 4) * s_i,
        label,
        11.0 * s_f,
        200.0 * s_f,
        (150, 150, 165, 200),
    );
}

fn draw_scroll_rail(
    pixmap: &mut Pixmap,
    state: &PickerState,
    x: i32,
    y_start: i32,
    total: usize,
    scale: u32,
) {
    let t = scale_tf(scale);
    let rail_h = VISIBLE_ITEMS as i32 * ITEM_HEIGHT;
    let mut track = Paint::default();
    track.set_color_rgba8(60, 62, 75, 180);
    if let Some(rect) = Rect::from_xywh(x as f32, y_start as f32, SCROLL_RAIL_W as f32, rail_h as f32) {
        pixmap.fill_rect(rect, &track, t, None);
    }

    let visible = VISIBLE_ITEMS.min(total) as f32;
    let total_f = total as f32;
    let thumb_h = ((visible / total_f) * rail_h as f32).max(12.0);
    let max_travel = rail_h as f32 - thumb_h;
    let max_offset = (total - VISIBLE_ITEMS).max(1) as f32;
    let thumb_y = y_start as f32 + (state.scroll_offset() as f32 / max_offset) * max_travel;

    let mut thumb = Paint::default();
    thumb.set_color_rgba8(176, 128, 255, 220);
    if let Some(rect) = Rect::from_xywh(x as f32, thumb_y, SCROLL_RAIL_W as f32, thumb_h) {
        pixmap.fill_rect(rect, &thumb, t, None);
    }
}
