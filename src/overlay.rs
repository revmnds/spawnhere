use crate::apps::{self, App, IconCache};
use crate::config::EffectiveMode;
use crate::history::History;
use crate::picker::{self, PickerState, TextRenderer};
use crate::stroke::{Bbox, Stroke};
use anyhow::{Context, Result};
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use calloop::{timer::{TimeoutAction, Timer}, EventLoop, LoopHandle, LoopSignal};
use calloop_wayland_source::WaylandSource;
use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState},
    delegate_compositor, delegate_keyboard, delegate_layer, delegate_output, delegate_pointer,
    delegate_registry, delegate_seat, delegate_shm,
    output::{OutputHandler, OutputState},
    reexports::protocols::wp::cursor_shape::v1::client::wp_cursor_shape_device_v1::{
        Shape, WpCursorShapeDeviceV1,
    },
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    seat::{
        keyboard::{KeyEvent, KeyboardHandler, Keysym, Modifiers},
        pointer::{
            cursor_shape::CursorShapeManager, PointerData, PointerEvent, PointerEventKind,
            PointerHandler, BTN_LEFT, BTN_RIGHT,
        },
        Capability, SeatHandler, SeatState,
    },
    shell::{
        wlr_layer::{
            Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface,
            LayerSurfaceConfigure,
        },
        WaylandSurface,
    },
    shm::{slot::SlotPool, Shm, ShmHandler},
};
use wayland_client::Proxy;
use crate::picker::text::Weight;
use tiny_skia::{Color, FillRule, Paint, PathBuilder, Pixmap, Stroke as SkStroke, Transform};
use wayland_client::{
    globals::registry_queue_init,
    protocol::{wl_keyboard, wl_output, wl_pointer, wl_seat, wl_shm, wl_surface},
    Connection, QueueHandle,
};

pub enum Outcome {
    /// User completed a gesture and (if no preset `--spawn`) chose an app.
    /// `bbox` is post-padding + min-size enforcement, in the overlay's
    /// surface-local coord frame (== monitor-local for a fullscreen layer
    /// surface on a single output). `screen` is the overlay's rect, handed
    /// back so the caller can re-clamp after applying per-app rule expansions.
    Spawn { bbox: Bbox, exec: String, screen: Bbox },
    Cancelled,
}

pub struct RunConfig {
    pub preset_exec: Option<String>,
    pub padding: u32,
    pub history: History,
    pub gesture: crate::config::GestureConfig,
}

/// Caret blink half-period (ms). Standard system blink is ~530ms per half cycle.
const CARET_BLINK_MS: u128 = 530;
/// How often we wake the loop to redraw the caret. Half the half-period so we
/// never miss a transition.
const BLINK_TICK: Duration = Duration::from_millis(265);
/// Shimmer tick for the stroke gradient animation. ~30 fps — fast enough to
/// look smooth, slow enough to stay cheap. The timer only actually dirties
/// the surface when a stroke is being drawn or previewed.
const SHIMMER_TICK: Duration = Duration::from_millis(33);
/// Full travel time of the shimmer pulse along the stroke, in seconds.
/// Short = energetic, long = calm. 2.5 s reads as "alive but not distracting".
const SHIMMER_CYCLE_S: f32 = 2.5;

fn caret_visible_at(since_reset: Duration) -> bool {
    (since_reset.as_millis() / CARET_BLINK_MS).is_multiple_of(2)
}

const BG_DIM_ALPHA: u8 = 64;
/// Saturated vaporwave-lila as the resting stroke color. The dual-hue pulses
/// (magenta + cyan) ride on top of this base.
const STROKE_COLOR: (u8, u8, u8, u8) = (170, 100, 255, 255);
const STROKE_WIDTH: f32 = 4.0;
const STROKE_DIM_ALPHA: u8 = 102; // ~0.4 * 255 — used during Picking phase
/// Two pulse hues that travel along the stroke — pink and cyan in opposing
/// phase produces a synthwave / chromatic-aberration feel as they cross the
/// midpoint together.
const PULSE_COLORS: &[(u8, u8, u8)] = &[
    (255, 60, 200),  // hot magenta
    (80, 230, 255),  // electric cyan
];

#[derive(Clone, PartialEq)]
enum Decision {
    Pending,
    Spawn(String),
    Cancel,
}

#[derive(Clone, Copy, PartialEq)]
enum Phase {
    Drawing,
    Picking,
}

struct AppState {
    registry_state: RegistryState,
    output_state: OutputState,
    shm: Shm,
    seat_state: SeatState,

    pool: SlotPool,
    layer: LayerSurface,

    width: u32,
    height: u32,
    /// Compositor-advertised output scale (1 for standard, 2 for HiDPI, etc.).
    /// Updated via `scale_factor_changed`; drives physical-pixel allocation
    /// and buffer scaling so the UI renders sharp on 2×/3× displays.
    scale: i32,
    configured: bool,
    needs_redraw: bool,

    pointer: Option<wl_pointer::WlPointer>,
    keyboard: Option<wl_keyboard::WlKeyboard>,

    /// Hyprland always advertises `wp_cursor_shape_v1`; we rely on it to show
    /// a proper arrow during Picking phase.
    cursor_shape_manager: CursorShapeManager,
    cursor_shape_device: Option<WpCursorShapeDeviceV1>,

    cursor: Option<(f32, f32)>,
    drawing: bool,
    has_drawn: bool,
    stroke: Stroke,
    /// Live Shift modifier. With the default config (`Shift` = square) this
    /// constrains the rectangle to 1:1; users can remap it via config.
    shift_held: bool,
    /// Live Ctrl modifier. Used for two orthogonal things: (a) Ctrl+P in the
    /// picker pins the selected app as the `--default` target, and (b) with
    /// the default config (`Ctrl` = freehand) it switches the drag to freehand
    /// mode. At release time, a held Ctrl also escapes from an active
    /// `--default` preset so the picker opens for this spawn — see the
    /// long-form note in the release handler below.
    ctrl_held: bool,
    /// Live Alt modifier. Unused in the default config but reserved: users can
    /// remap `square_modifier` or `freehand_modifier` to `"alt"`.
    alt_held: bool,
    /// Press-point of a rectangle-style drag (plain rectangle or Shift-square).
    /// `Some` while the current drag is in rectangle mode, `None` for freehand.
    /// Set when the drag threshold is crossed, not at press time, so a click
    /// never leaves stale state behind.
    rect_start: Option<(f32, f32)>,
    /// Press-point of the current gesture. Recorded on mouse-down; compared
    /// against every subsequent motion to decide whether the user has actually
    /// started dragging. Cleared on release.
    drag_origin: Option<(f32, f32)>,
    /// `true` once the cursor has travelled more than `gesture.drag_threshold_px`
    /// from `drag_origin`. While `false`, the stroke stays empty so a
    /// micro-jittered click doesn't spawn a sliver window.
    drag_committed: bool,
    /// User-tunable gesture behaviour: which mode is default, which modifiers
    /// select the others, minimum drag distance, dimension readout toggle.
    gesture_cfg: crate::config::GestureConfig,

    phase: Phase,
    preset_exec: Option<String>,
    picker: PickerState,
    text: TextRenderer,
    icons: IconCache,
    apps_rx: Option<Receiver<Vec<App>>>,
    last_card: Option<picker::CardRect>,

    /// Timestamp the shimmer animation uses as phase zero. Reset when a new
    /// stroke begins so the pulse always starts at the beginning of the line.
    stroke_anim_start: Instant,

    /// Reset on every typing event so the caret stays solid for one half-cycle
    /// after input (matches standard text-input UX).
    caret_blink_reset: Instant,
    /// Last caret visibility we committed to the screen. Used to suppress
    /// redraws on blink ticks when nothing actually changed.
    caret_last_visible: bool,

    /// Reused across frames — reallocated only when the surface size changes.
    /// Avoids a full-screen malloc/free per draw call.
    pixmap: Option<Pixmap>,

    decision: Decision,
    loop_handle: LoopHandle<'static, AppState>,
    loop_signal: LoopSignal,
}

pub fn run(cfg: RunConfig) -> Result<Outcome> {
    let RunConfig { preset_exec, padding, history, gesture } = cfg;
    let conn = Connection::connect_to_env().context("connecting to Wayland display")?;
    let (globals, event_queue) =
        registry_queue_init(&conn).context("initializing Wayland registry")?;
    let qh = event_queue.handle();

    let compositor_state =
        CompositorState::bind(&globals, &qh).context("wl_compositor not available")?;
    let layer_shell = LayerShell::bind(&globals, &qh).context(
        "zwlr_layer_shell_v1 not advertised by the Wayland compositor. \
         spawnhere draws its overlay via this protocol; Hyprland supports it \
         natively. If you're seeing this under Hyprland, your Hyprland version \
         may be too old — 0.30+ is recommended.",
    )?;
    let shm = Shm::bind(&globals, &qh).context("wl_shm not available")?;

    let surface = compositor_state.create_surface(&qh);
    let layer =
        layer_shell.create_layer_surface(&qh, surface, Layer::Overlay, Some("spawnhere"), None);
    layer.set_anchor(Anchor::TOP | Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT);
    layer.set_keyboard_interactivity(KeyboardInteractivity::Exclusive);
    layer.set_exclusive_zone(-1);
    layer.set_size(0, 0);
    layer.commit();

    // Hyprland always exposes `wp_cursor_shape_v1`; bail hard if missing so
    // we fail loudly rather than silently falling back to a hidden cursor.
    let cursor_shape_manager = CursorShapeManager::bind(&globals, &qh).context(
        "wp_cursor_shape_v1 not advertised. This is part of every modern Hyprland \
         build — please update Hyprland (0.34+ recommended).",
    )?;

    // Initial pool is a hint, not a cap — SlotPool grows on demand. Start
    // with 1080p @ 2× so typical HiDPI laptops don't need to grow on first use.
    let pool = SlotPool::new(1920 * 1080 * 4 * 2, &shm).context("creating shm pool")?;

    // Kick off background app discovery only if we'll need the picker.
    let apps_rx = if preset_exec.is_none() {
        let (rx, _) = apps::discover_async();
        Some(rx)
    } else {
        None
    };

    let mut event_loop: EventLoop<AppState> =
        EventLoop::try_new().context("creating calloop event loop")?;
    let loop_handle = event_loop.handle();
    let loop_signal = event_loop.get_signal();

    WaylandSource::new(conn.clone(), event_queue)
        .insert(loop_handle.clone())
        .map_err(|e| anyhow::anyhow!("inserting Wayland source: {e}"))?;

    // Blink tick: only dirty the surface when the caret visibility would
    // actually change. Without this, the picker repaints ~4×/sec even while
    // the user is idle (compositor frame callback then re-arms at 60 fps).
    loop_handle
        .insert_source(Timer::from_duration(BLINK_TICK), |_, _, state: &mut AppState| {
            if state.phase == Phase::Picking {
                let want = caret_visible_at(state.caret_blink_reset.elapsed());
                if want != state.caret_last_visible {
                    state.needs_redraw = true;
                }
            }
            TimeoutAction::ToDuration(BLINK_TICK)
        })
        .map_err(|e| anyhow::anyhow!("inserting blink timer: {e}"))?;

    // Shimmer tick: while a stroke is being drawn, the gradient pulse along
    // the line needs ~30 fps to feel continuous. Dirty only when there's
    // something to animate so the wakeup cost is near-zero on an idle overlay.
    loop_handle
        .insert_source(Timer::from_duration(SHIMMER_TICK), |_, _, state: &mut AppState| {
            if state.phase == Phase::Drawing && state.stroke.points().len() >= 2 {
                state.needs_redraw = true;
            }
            TimeoutAction::ToDuration(SHIMMER_TICK)
        })
        .map_err(|e| anyhow::anyhow!("inserting shimmer timer: {e}"))?;

    let mut state = AppState {
        registry_state: RegistryState::new(&globals),
        output_state: OutputState::new(&globals, &qh),
        shm,
        seat_state: SeatState::new(&globals, &qh),

        pool,
        layer,

        width: 0,
        height: 0,
        scale: 1,
        configured: false,
        needs_redraw: false,

        pointer: None,
        keyboard: None,

        cursor_shape_manager,
        cursor_shape_device: None,

        cursor: None,
        drawing: false,
        has_drawn: false,
        stroke: Stroke::new(),
        shift_held: false,
        ctrl_held: false,
        alt_held: false,
        rect_start: None,
        drag_origin: None,
        drag_committed: false,
        gesture_cfg: gesture,

        phase: Phase::Drawing,
        preset_exec,
        picker: PickerState::new(history),
        text: TextRenderer::new(),
        icons: IconCache::new(crate::picker::ICON_SIZE),
        apps_rx,
        last_card: None,

        stroke_anim_start: Instant::now(),
        caret_blink_reset: Instant::now(),
        caret_last_visible: true,
        pixmap: None,

        decision: Decision::Pending,
        loop_handle,
        loop_signal,
    };

    event_loop
        .run(Duration::from_millis(16), &mut state, |state| {
            // Drain background app scan.
            if state.picker.loading() {
                if let Some(rx) = &state.apps_rx {
                    if let Ok(apps) = rx.try_recv() {
                        state.picker.set_apps(apps);
                        state.needs_redraw = true;
                    }
                }
            }

            if state.configured && state.needs_redraw {
                if let Err(e) = state.draw(&qh) {
                    eprintln!("spawnhere: draw failed: {e:#}");
                }
                state.needs_redraw = false;
            }

            if state.decision != Decision::Pending {
                state.loop_signal.stop();
            }
        })
        .context("running calloop event loop")?;

    let screen = Bbox { x: 0, y: 0, w: state.width, h: state.height };
    let stroke = std::mem::take(&mut state.stroke);
    Ok(match std::mem::replace(&mut state.decision, Decision::Cancel) {
        Decision::Spawn(exec) => {
            // Respect whatever the user drew — 0×0 (bare click), a 5 px line,
            // or a big rectangle. The downstream spawn honors the bbox as-is
            // (or omits `size` entirely when it's 0×0 so the app uses its
            // natural default). clamp_to_rect just keeps the spawn point
            // inside the monitor's safe area.
            let raw = stroke.bbox(padding);
            let bbox = raw.clamp_to_rect(screen);
            Outcome::Spawn { bbox, exec, screen }
        }
        _ => Outcome::Cancelled,
    })
}

impl AppState {
    fn finalize_spawn(&mut self) {
        if let Some(app) = self.picker.selected_app() {
            self.decision = Decision::Spawn(app.exec.clone());
        } else if self.picker.loading() {
            // Apps still scanning — keep waiting; user can ESC to cancel.
        } else {
            // No matches for query — do nothing on Enter.
        }
    }

    /// Replace the current stroke with a rounded-rectangle outline from
    /// `rect_start` to `(cx, cy)`. The stroke renderer draws a polyline, so
    /// each 90° corner is tesselated into a few short segments and the
    /// straight edges fall out as the connecting `line_to`s between arc
    /// endpoints. The radius is subtle (8 logical px) and clamps to half the
    /// smaller side so tiny rects don't become circles.
    ///
    /// When `square` is true, the stroke is constrained to a 1:1 aspect ratio
    /// anchored at `rect_start`. The short leg of the drag wins so the cursor
    /// only pulls the opposite corner along the diagonal — matches the
    /// Photoshop/Figma behaviour of Shift-constraining a rectangle tool.
    fn update_rect_stroke(&mut self, cx: f32, cy: f32, square: bool) {
        let Some((sx, sy)) = self.rect_start else { return };
        self.stroke.clear();

        let (x0, x1, y0, y1) = if square {
            // Cursor direction drives which diagonal we collapse onto. The
            // drag distance on the shorter axis caps the box so a 1:1 always
            // fits inside the user's gesture.
            let dx = cx - sx;
            let dy = cy - sy;
            let side = dx.abs().min(dy.abs());
            let ex = sx + side * dx.signum();
            let ey = sy + side * dy.signum();
            (sx.min(ex), sx.max(ex), sy.min(ey), sy.max(ey))
        } else {
            (sx.min(cx), sx.max(cx), sy.min(cy), sy.max(cy))
        };

        let w = x1 - x0;
        let h = y1 - y0;
        let r = 8.0_f32.min(w * 0.5).min(h * 0.5);
        if r < 1.0 {
            // Too small to round — fall back to the sharp 5-point rectangle.
            self.stroke.push(x0, y0);
            self.stroke.push(x1, y0);
            self.stroke.push(x1, y1);
            self.stroke.push(x0, y1);
            self.stroke.push(x0, y0);
            return;
        }
        push_corner_arc(&mut self.stroke, x1 - r, y0 + r, r, 270.0, 360.0); // top-right
        push_corner_arc(&mut self.stroke, x1 - r, y1 - r, r, 0.0,   90.0);  // bottom-right
        push_corner_arc(&mut self.stroke, x0 + r, y1 - r, r, 90.0,  180.0); // bottom-left
        push_corner_arc(&mut self.stroke, x0 + r, y0 + r, r, 180.0, 270.0); // top-left
        // Close: back to the start of the top-right arc so the top edge
        // renders as the final line_to segment.
        self.stroke.push(x1 - r, y0);
    }

    /// Drive the stroke forward from a motion event. Handles three cases:
    ///   1. Haven't crossed the drag threshold yet → no-op (stroke stays
    ///      empty; a release here counts as a click).
    ///   2. Just crossed the threshold → initialise the stroke in whichever
    ///      effective mode the current modifiers select.
    ///   3. Already committed → extend the existing stroke.
    fn commit_or_extend_drag(&mut self, x: f32, y: f32) {
        if !self.drag_committed {
            let Some((ox, oy)) = self.drag_origin else { return };
            let dx = x - ox;
            let dy = y - oy;
            if (dx * dx + dy * dy).sqrt() < self.gesture_cfg.drag_threshold_px {
                return;
            }
            self.drag_committed = true;
            self.stroke.clear();
            match self.effective_mode() {
                EffectiveMode::Rectangle => {
                    self.rect_start = Some((ox, oy));
                    self.update_rect_stroke(x, y, false);
                }
                EffectiveMode::Square => {
                    self.rect_start = Some((ox, oy));
                    self.update_rect_stroke(x, y, true);
                }
                EffectiveMode::Freehand => {
                    self.rect_start = None;
                    self.stroke.push(ox, oy);
                    self.stroke.push(x, y);
                }
            }
            return;
        }

        match self.effective_mode() {
            EffectiveMode::Rectangle => self.update_rect_stroke(x, y, false),
            EffectiveMode::Square => self.update_rect_stroke(x, y, true),
            EffectiveMode::Freehand => self.stroke.push(x, y),
        }
    }

    fn effective_mode(&self) -> EffectiveMode {
        self.gesture_cfg
            .resolve(self.shift_held, self.ctrl_held, self.alt_held)
    }

    fn enter_picker_phase(&mut self) {
        if let Some(exec) = &self.preset_exec {
            // No picker — spawn directly with the preset exec.
            self.decision = Decision::Spawn(exec.clone());
            return;
        }
        self.phase = Phase::Picking;
        self.caret_blink_reset = Instant::now();
        // Re-show the cursor now that we're out of the drawing phase. If the
        // pointer isn't currently over our surface, this is a no-op until the
        // next Enter — which will hit the phase-aware branch below.
        self.refresh_cursor();
        self.needs_redraw = true;
    }

    /// Apply the cursor appropriate to the current phase, using the most recent
    /// enter serial tracked by SCTK. Safe to call whenever phase changes.
    fn refresh_cursor(&self) {
        let Some(pointer) = self.pointer.as_ref() else {
            return;
        };
        let Some(serial) = pointer
            .data::<PointerData>()
            .and_then(|d| d.latest_enter_serial())
        else {
            return;
        };
        self.apply_cursor_for_phase(pointer, serial);
    }

    fn apply_cursor_for_phase(&self, pointer: &wl_pointer::WlPointer, serial: u32) {
        match self.phase {
            // Drawing: hide the cursor — our crosshair is what the user tracks.
            Phase::Drawing => pointer.set_cursor(serial, None, 0, 0),
            // Picking: show the standard arrow via cursor-shape-v1.
            Phase::Picking => {
                if let Some(device) = self.cursor_shape_device.as_ref() {
                    device.set_shape(serial, Shape::Default);
                }
            }
        }
    }

    /// Single entry point for initial key presses and synthetic repeats.
    fn handle_key_event(&mut self, event: KeyEvent) {
        if event.keysym == Keysym::Escape {
            self.decision = Decision::Cancel;
            return;
        }

        if self.phase != Phase::Picking {
            return;
        }

        // Ctrl+P toggles the pinned "default" app on the selected row.
        // Short-circuit before the text-input branch so the control char
        // (0x10) doesn't land in the search query.
        if self.ctrl_held && matches!(event.keysym, Keysym::p | Keysym::P) {
            self.picker.toggle_pin_selected();
            self.needs_redraw = true;
            return;
        }

        match event.keysym {
            Keysym::Return | Keysym::KP_Enter => {
                self.finalize_spawn();
            }
            Keysym::BackSpace => {
                self.picker.pop_char();
                self.caret_blink_reset = Instant::now();
                self.needs_redraw = true;
            }
            Keysym::Up => {
                self.picker.move_selection(-1);
                self.needs_redraw = true;
            }
            Keysym::Down => {
                self.picker.move_selection(1);
                self.needs_redraw = true;
            }
            Keysym::Page_Up => {
                self.picker.move_selection(-(picker::VISIBLE_ITEMS as isize));
                self.needs_redraw = true;
            }
            Keysym::Page_Down => {
                self.picker.move_selection(picker::VISIBLE_ITEMS as isize);
                self.needs_redraw = true;
            }
            Keysym::Delete => {
                // Forget the currently-selected app from history (if it has
                // been picked before). Backspace keeps its text-editing role.
                let selected = self.picker.selected_index();
                if self.picker.is_history_row(selected) {
                    self.picker.forget_at(selected);
                    self.needs_redraw = true;
                }
            }
            _ => {
                if let Some(text) = event.utf8.as_deref() {
                    for c in text.chars() {
                        self.picker.push_char(c);
                    }
                    self.caret_blink_reset = Instant::now();
                    self.needs_redraw = true;
                }
            }
        }
    }

    fn draw(&mut self, qh: &QueueHandle<Self>) -> Result<()> {
        let (w_log, h_log) = (self.width, self.height);
        if w_log == 0 || h_log == 0 {
            return Ok(());
        }
        // Render into a physical-pixel buffer so HiDPI monitors stay sharp.
        // Vector shapes are rendered through a scale transform; cosmic-text
        // gets the scale directly so glyphs rasterize at native density.
        let scale = self.scale.max(1) as u32;
        let w = w_log * scale;
        let h = h_log * scale;
        let stride = w as i32 * 4;
        let (buffer, canvas) = self
            .pool
            .create_buffer(w as i32, h as i32, stride, wl_shm::Format::Argb8888)
            .context("creating shm buffer")?;

        // Keep the pixmap across frames; only reallocate when physical size
        // changes (logical resize OR scale change). Saves a large malloc/free
        // per draw call at steady state.
        let needs_new_pixmap = self
            .pixmap
            .as_ref()
            .map(|p| p.width() != w || p.height() != h)
            .unwrap_or(true);
        if needs_new_pixmap {
            self.pixmap = Some(Pixmap::new(w, h).context("creating pixmap")?);
        }
        let pixmap = self.pixmap.as_mut().expect("pixmap just ensured");
        pixmap.fill(Color::from_rgba8(0, 0, 0, BG_DIM_ALPHA));

        let stroke_alpha = match self.phase {
            Phase::Drawing => STROKE_COLOR.3,
            Phase::Picking => STROKE_DIM_ALPHA,
        };
        let shimmer_on = self.phase == Phase::Drawing;
        let anim_t = self.stroke_anim_start.elapsed().as_secs_f32();
        draw_stroke(pixmap, &self.stroke, stroke_alpha, scale, anim_t, shimmer_on);

        // Crosshair visibility:
        //   * Idle (not yet drawing) — always show, so the user has a clear
        //     "aim point" before they commit to a gesture.
        //   * Mid-rectangle-drag — keep it on the far corner so there's still
        //     something tracking the cursor (the rect outline is fixed-width
        //     and easy to lose). Skipped for freehand since the stroke itself
        //     is already a continuous cursor-follower.
        if self.phase == Phase::Drawing {
            let show_crosshair = match (self.drawing, self.has_drawn, self.drag_committed) {
                (false, false, _) => true,
                (true, _, true) => {
                    let mode = self.gesture_cfg.resolve(
                        self.shift_held,
                        self.ctrl_held,
                        self.alt_held,
                    );
                    matches!(mode, EffectiveMode::Rectangle | EffectiveMode::Square)
                }
                (true, _, false) => true, // pre-threshold — still aiming
                _ => false,
            };
            if show_crosshair {
                if let Some((cx, cy)) = self.cursor {
                    draw_crosshair(pixmap, cx, cy, scale);
                }
            }
        }

        // Live W×H readout next to the cursor while drawing a rectangle. Only
        // shown after the drag threshold is crossed — before that the bbox is
        // zero-size and the number would flicker meaninglessly.
        if self.phase == Phase::Drawing
            && self.drawing
            && self.drag_committed
            && self.gesture_cfg.show_dimensions
        {
            // Resolve via field borrow instead of `&self` method borrow —
            // `canvas` is still alive from the buffer allocation above, so a
            // whole-`self` borrow here trips the borrow checker.
            let mode = self.gesture_cfg.resolve(self.shift_held, self.ctrl_held, self.alt_held);
            if matches!(mode, EffectiveMode::Rectangle | EffectiveMode::Square) {
                if let Some((cx, cy)) = self.cursor {
                    let bbox = self.stroke.bbox(0);
                    draw_dimensions_readout(
                        pixmap,
                        &mut self.text,
                        cx,
                        cy,
                        bbox.w,
                        bbox.h,
                        mode == EffectiveMode::Square,
                        w_log,
                        h_log,
                        scale,
                    );
                }
            }
        }

        // Default-mode hint: tells the user which app this bind launches and
        // how to escape to the picker. Without it, a user who pinned by
        // accident has no way to discover the secondary bind.
        if self.phase == Phase::Drawing {
            if let Some(preset) = self.preset_exec.clone() {
                let display = preset.split_whitespace().next().unwrap_or(&preset).to_string();
                draw_default_banner(pixmap, &mut self.text, w_log, &display, scale, self.ctrl_held);
            }
        }

        if self.phase == Phase::Picking {
            let bbox = self.stroke.bbox(0);
            let cx = bbox.x + (bbox.w as i32 / 2);
            let cy = bbox.y + (bbox.h as i32 / 2);
            let caret_visible = caret_visible_at(self.caret_blink_reset.elapsed());
            self.caret_last_visible = caret_visible;
            // Hover + hit-testing stay in LOGICAL pixels (pointer events are
            // delivered in surface-local logical coords); the picker reports
            // `last_card` in logical so the two naturally align.
            let hovered_rel = self.last_card.and_then(|card| {
                let (cx, cy) = self.cursor?;
                if !card.contains(cx as i32, cy as i32) {
                    return None;
                }
                card.item_at(cy as i32, self.picker.visible_count())
            });
            // Forget-button hover: only valid on history rows, since the × is
            // hidden for non-history rows. We resolve to the absolute index
            // first (rel + scroll_offset) to ask the picker.
            let forget_hover_rel = self.last_card.and_then(|card| {
                let (cx, cy) = self.cursor?;
                let rel = card.item_at(cy as i32, self.picker.visible_count())?;
                if !card.forget_button_hit(cx as i32, cy as i32) {
                    return None;
                }
                let abs = self.picker.scroll_offset() + rel;
                self.picker.is_history_row(abs).then_some(rel)
            });
            let card = picker::draw(
                pixmap,
                &self.picker,
                &mut self.text,
                &mut self.icons,
                (cx, cy),
                (w_log, h_log),
                caret_visible,
                hovered_rel,
                forget_hover_rel,
                scale,
            );
            self.last_card = Some(card);
        }

        // tiny-skia is RGBA in memory; Wayland Argb8888 on LE is BGRA byte order.
        let src = pixmap.data();
        for (dst, chunk) in canvas.chunks_exact_mut(4).zip(src.chunks_exact(4)) {
            dst[0] = chunk[2];
            dst[1] = chunk[1];
            dst[2] = chunk[0];
            dst[3] = chunk[3];
        }

        let surface = self.layer.wl_surface();
        surface.damage_buffer(0, 0, w as i32, h as i32);
        buffer
            .attach_to(surface)
            .context("attaching buffer to surface")?;
        surface.frame(qh, surface.clone());
        surface.commit();
        Ok(())
    }
}

/// Tesselate a quarter-circle arc into short line segments and push each
/// point onto the stroke. The stroke renderer draws line segments between
/// consecutive points, so N segments = N+1 points on the arc.
fn push_corner_arc(stroke: &mut Stroke, cx: f32, cy: f32, r: f32, start_deg: f32, end_deg: f32) {
    const SEGS: u32 = 6;
    for i in 0..=SEGS {
        let t = i as f32 / SEGS as f32;
        let theta = (start_deg + t * (end_deg - start_deg)).to_radians();
        stroke.push(cx + r * theta.cos(), cy + r * theta.sin());
    }
}

fn draw_stroke(
    pixmap: &mut Pixmap,
    stroke: &Stroke,
    alpha: u8,
    scale: u32,
    anim_t: f32,
    shimmer: bool,
) {
    let pts = stroke.points();
    if pts.len() < 2 {
        return;
    }
    let s = scale as f32;
    let transform = Transform::from_scale(s, s);
    let sk_round = SkStroke {
        width: STROKE_WIDTH * s,
        line_cap: tiny_skia::LineCap::Round,
        line_join: tiny_skia::LineJoin::Round,
        ..Default::default()
    };

    // Pass 1: the stroke as ONE continuous subpath in the base color. This
    // preserves `line_join: Round` across every vertex, so rectangular corners
    // and freehand curves alike stay seamless — no segmentation artifacts.
    let mut pb = PathBuilder::new();
    pb.move_to(pts[0].x, pts[0].y);
    for p in &pts[1..] {
        pb.line_to(p.x, p.y);
    }
    if let Some(path) = pb.finish() {
        let mut paint = Paint::default();
        paint.set_color_rgba8(STROKE_COLOR.0, STROKE_COLOR.1, STROKE_COLOR.2, alpha);
        paint.anti_alias = true;
        pixmap.stroke_path(&path, &paint, &sk_round, transform, None);
    }

    if !shimmer {
        return;
    }

    // Pass 2: a bright highlight painted on top of the stroke in the region
    // near the pulse. Drawn as layered "bands" of increasing width + softer
    // alpha to approximate a Gaussian halo without per-segment color steps.
    //
    // Freehand strokes naturally have dense, short segments, but rect-mode
    // strokes encode the 4 sides as *single* long segments between arcs. To
    // give the halo the same resolution on any shape, we resample the
    // polyline into ≤ MAX_SEG-long pieces before computing band inclusion.
    const MAX_SEG: f32 = 2.0;
    let mut dense: Vec<(f32, f32)> = Vec::with_capacity(pts.len() * 2);
    dense.push((pts[0].x, pts[0].y));
    for i in 1..pts.len() {
        let (x0, y0) = (pts[i - 1].x, pts[i - 1].y);
        let (x1, y1) = (pts[i].x, pts[i].y);
        let dx = x1 - x0;
        let dy = y1 - y0;
        let len = (dx * dx + dy * dy).sqrt();
        if len <= MAX_SEG {
            dense.push((x1, y1));
        } else {
            let n = (len / MAX_SEG).ceil().max(1.0) as usize;
            for k in 1..=n {
                let t = k as f32 / n as f32;
                dense.push((x0 + dx * t, y0 + dy * t));
            }
        }
    }

    let mut cum_dist = vec![0.0_f32; dense.len()];
    for i in 1..dense.len() {
        let dx = dense[i].0 - dense[i - 1].0;
        let dy = dense[i].1 - dense[i - 1].1;
        cum_dist[i] = cum_dist[i - 1] + (dx * dx + dy * dy).sqrt();
    }
    let total = cum_dist[dense.len() - 1].max(1.0);

    // If the stroke's endpoints coincide (rectangle mode always closes the
    // path), treat it as cyclic so the pulse wraps continuously instead of
    // disappearing past the end and re-appearing at the start. We tile three
    // copies of the polyline end-to-end and put the pulse into the *middle*
    // copy. A single halo with half-width up to `sigma * width_max * total`
    // then always has enough slack on both sides to render without clipping,
    // regardless of where in the cycle the pulse sits — no more "half pulse"
    // flash at the seam.
    let (dx_end, dy_end) = (
        dense[dense.len() - 1].0 - dense[0].0,
        dense[dense.len() - 1].1 - dense[0].1,
    );
    let closed = (dx_end * dx_end + dy_end * dy_end).sqrt() < 2.0;
    let (eff_dense, eff_cum) = if closed {
        let mut d = dense.clone();
        let mut c = cum_dist.clone();
        for copy in 1..=2 {
            let offset = copy as f32 * total;
            for i in 1..dense.len() {
                d.push(dense[i]);
                c.push(cum_dist[i] + offset);
            }
        }
        (d, c)
    } else {
        (dense, cum_dist)
    };

    let sigma: f32 = 0.14; // half-width of the pulse in normalized-length units

    // Two pulses 180° out of phase, each in its own hue. As they cross the
    // mid of the stroke their bleed mixes into a momentary white-hot spot —
    // that's the synthwave "chromatic crossover" we want.
    const NUM_PULSES: usize = 2;
    const N_BANDS: usize = 10;

    let base_cycle = (anim_t % SHIMMER_CYCLE_S) / SHIMMER_CYCLE_S;

    for p in 0..NUM_PULSES {
        let (pulse_r, pulse_g, pulse_b) = PULSE_COLORS[p % PULSE_COLORS.len()];
        let pulse_norm = (base_cycle + p as f32 / NUM_PULSES as f32) % 1.0;
        let pulse_dist = if closed {
            total + pulse_norm * total
        } else {
            (pulse_norm * (1.0 + 2.0 * sigma) - sigma) * total
        };

        // For open strokes, cross-fade each pulse near the endpoints using a
        // smoothstep so it enters/exits gently instead of appearing at full
        // brightness with its round cap at the first/last pixel.
        let edge_falloff = if closed {
            1.0
        } else {
            let e_in = (pulse_norm / 0.12).clamp(0.0, 1.0);
            let e_out = ((1.0 - pulse_norm) / 0.12).clamp(0.0, 1.0);
            let t = e_in.min(e_out);
            t * t * (3.0 - 2.0 * t) // smoothstep
        };
        if edge_falloff < 0.02 {
            continue;
        }

        for k in 0..N_BANDS {
            let t = k as f32 / (N_BANDS as f32 - 1.0);
            let width_mult = 2.2 - 1.9 * t;
            // Per-band alpha — outer halo softer, core bright. With two
            // hue-distinct pulses overlaying, total alpha stays visually
            // balanced without saturating the eye.
            let raw_alpha = (18.0 + 22.0 * t) * edge_falloff;
            let alpha = raw_alpha.clamp(0.0, 255.0) as u8;
            // Mix the pulse hue toward white at the core — keeps color in
            // the halo and lets the bright crest read as a heat-flash.
            let mix = t * 0.7;
            let cr = (pulse_r as f32 + (255.0 - pulse_r as f32) * mix) as u8;
            let cg = (pulse_g as f32 + (255.0 - pulse_g as f32) * mix) as u8;
            let cb = (pulse_b as f32 + (255.0 - pulse_b as f32) * mix) as u8;

            // Outer bands get a wider stroke so the halo "bleeds" outside
            // the line — that's the neon-tube glow. Core stays close to the
            // base width for a sharp crest.
            let band_w = STROKE_WIDTH * (0.85 + (1.0 - t) * 1.6);
            let sk_band = SkStroke {
                width: band_w * s,
                line_cap: tiny_skia::LineCap::Round,
                line_join: tiny_skia::LineJoin::Round,
                ..Default::default()
            };

            let half = sigma * width_mult * total;
            let lo = pulse_dist - half;
            let hi = pulse_dist + half;

            let mut pb = PathBuilder::new();
            let mut active = false;
            let mut any = false;
            for i in 1..eff_dense.len() {
                let mid = (eff_cum[i - 1] + eff_cum[i]) * 0.5;
                if mid >= lo && mid <= hi {
                    if !active {
                        pb.move_to(eff_dense[i - 1].0, eff_dense[i - 1].1);
                        active = true;
                        any = true;
                    }
                    pb.line_to(eff_dense[i].0, eff_dense[i].1);
                } else {
                    active = false;
                }
            }
            if !any {
                continue;
            }
            let Some(path) = pb.finish() else { continue };
            let mut paint = Paint::default();
            paint.set_color_rgba8(cr, cg, cb, alpha);
            paint.anti_alias = true;
            pixmap.stroke_path(&path, &paint, &sk_band, transform, None);
        }
    }
}

/// Translucent pill at the top of the overlay shown only in `--default` mode.
/// Tells the user which app this bind will launch and how to switch to the
/// picker without already knowing the secondary keybind. Ensures the pinning
/// feature is escapable from a single-bind user's perspective.
fn draw_default_banner(
    pixmap: &mut Pixmap,
    text: &mut TextRenderer,
    overlay_w: u32,
    preset: &str,
    scale: u32,
    ctrl_held: bool,
) {
    let s = scale as f32;
    let s_i = scale as i32;
    let t = Transform::from_scale(s, s);

    let font_size = 13.0_f32;
    let body = format!("★ {preset}   ·   Hold CTRL to pick another");
    let text_w_phys = text.measure_width_weighted(&body, font_size * s, Weight::MEDIUM);
    let text_w_log = (text_w_phys / s).ceil() as i32;

    let banner_h = 32;
    let pad_x = 16;
    let banner_w = text_w_log + 2 * pad_x;
    let banner_x = (overlay_w as i32 - banner_w) / 2;
    let banner_y = 80;

    // Ctrl-held lights the pill up in the same vaporwave magenta as the rest
    // of the picker affordances — visual confirmation that the next release
    // will escape to the picker instead of launching the pinned app.
    let (bg_rgba, border_rgba, stroke_w, text_rgba) = if ctrl_held {
        (
            (52, 36, 78, 240),
            (210, 160, 255, 230),
            1.6_f32,
            (250, 248, 255, 255),
        )
    } else {
        (
            (20, 21, 30, 230),
            (170, 100, 255, 130),
            1.0_f32,
            (235, 230, 245, 255),
        )
    };

    let mut bg = Paint::default();
    bg.set_color_rgba8(bg_rgba.0, bg_rgba.1, bg_rgba.2, bg_rgba.3);
    bg.anti_alias = true;
    if let Some(path) = pill_path(banner_x as f32, banner_y as f32, banner_w as f32, banner_h as f32, 16.0) {
        pixmap.fill_path(&path, &bg, FillRule::Winding, t, None);
        let mut border = Paint::default();
        border.set_color_rgba8(border_rgba.0, border_rgba.1, border_rgba.2, border_rgba.3);
        border.anti_alias = true;
        let stroke = SkStroke { width: stroke_w, ..Default::default() };
        pixmap.stroke_path(&path, &border, &stroke, t, None);
    }

    // Same centering formula as the search bar: cap-center ≈ 0.44·size below
    // the draw origin, so putting the draw origin at (center − 0.44·size)
    // lands the cap-center on the pill center.
    let text_y = banner_y + banner_h / 2 - (font_size * 0.44) as i32;
    text.draw_weighted(
        pixmap,
        (banner_x + pad_x) * s_i,
        text_y * s_i,
        &body,
        font_size * s,
        text_w_phys + 2.0 * s,
        (text_rgba.0, text_rgba.1, text_rgba.2, text_rgba.3),
        Weight::MEDIUM,
    );
}

/// Draw a small `W×H` pill near the cursor while dragging a rectangle. Flips
/// to the opposite side of the cursor when it would fall off the overlay so
/// users near the screen edge still see the number.
#[allow(clippy::too_many_arguments)]
fn draw_dimensions_readout(
    pixmap: &mut Pixmap,
    text: &mut TextRenderer,
    cursor_x: f32,
    cursor_y: f32,
    w_px: u32,
    h_px: u32,
    square: bool,
    overlay_w: u32,
    overlay_h: u32,
    scale: u32,
) {
    let s = scale as f32;
    let s_i = scale as i32;
    let t = Transform::from_scale(s, s);

    let font_size = 12.0_f32;
    let body = if square {
        format!("{w_px}×{h_px}  ·  1:1")
    } else {
        format!("{w_px}×{h_px}")
    };
    let text_w_phys = text.measure_width_weighted(&body, font_size * s, Weight::MEDIUM);
    let text_w_log = (text_w_phys / s).ceil() as i32;

    let pad_x = 8;
    let pill_h = 22;
    let pill_w = text_w_log + 2 * pad_x;

    // Default placement: 14 px down-right of the cursor. Flip horizontally if
    // we'd run past the right edge, vertically if we'd run past the bottom.
    let gap = 14;
    let mut pill_x = cursor_x as i32 + gap;
    let mut pill_y = cursor_y as i32 + gap;
    if pill_x + pill_w > overlay_w as i32 {
        pill_x = cursor_x as i32 - gap - pill_w;
    }
    if pill_y + pill_h > overlay_h as i32 {
        pill_y = cursor_y as i32 - gap - pill_h;
    }
    // Clamp so a cursor in the far corners still keeps the pill on screen.
    pill_x = pill_x.max(4).min(overlay_w as i32 - pill_w - 4);
    pill_y = pill_y.max(4).min(overlay_h as i32 - pill_h - 4);

    let mut bg = Paint::default();
    bg.set_color_rgba8(20, 21, 30, 220);
    bg.anti_alias = true;
    if let Some(path) = pill_path(pill_x as f32, pill_y as f32, pill_w as f32, pill_h as f32, 11.0) {
        pixmap.fill_path(&path, &bg, FillRule::Winding, t, None);
        let mut border = Paint::default();
        border.set_color_rgba8(170, 100, 255, 160);
        border.anti_alias = true;
        let stroke = SkStroke { width: 1.0, ..Default::default() };
        pixmap.stroke_path(&path, &border, &stroke, t, None);
    }

    let text_y = pill_y + (pill_h - font_size as i32) / 2 - 1;
    text.draw_weighted(
        pixmap,
        (pill_x + pad_x) * s_i,
        text_y * s_i,
        &body,
        font_size * s,
        text_w_phys + 2.0 * s,
        (235, 230, 245, 255),
        Weight::MEDIUM,
    );
}

fn pill_path(x: f32, y: f32, w: f32, h: f32, r: f32) -> Option<tiny_skia::Path> {
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

fn draw_crosshair(pixmap: &mut Pixmap, x: f32, y: f32, scale: u32) {
    let mut paint = Paint::default();
    paint.set_color_rgba8(255, 255, 255, 200);
    paint.anti_alias = true;
    let s = scale as f32;
    let sk = SkStroke { width: 1.5 * s, ..Default::default() };

    let r = 12.0;
    let gap = 4.0;
    let mut pb = PathBuilder::new();
    pb.move_to(x - r, y);
    pb.line_to(x - gap, y);
    pb.move_to(x + gap, y);
    pb.line_to(x + r, y);
    pb.move_to(x, y - r);
    pb.line_to(x, y - gap);
    pb.move_to(x, y + gap);
    pb.line_to(x, y + r);
    if let Some(path) = pb.finish() {
        pixmap.stroke_path(&path, &paint, &sk, Transform::from_scale(s, s), None);
    }
}

impl CompositorHandler for AppState {
    fn scale_factor_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        surface: &wl_surface::WlSurface,
        factor: i32,
    ) {
        let factor = factor.max(1);
        if factor == self.scale {
            return;
        }
        self.scale = factor;
        surface.set_buffer_scale(factor);
        // Icons are baked at a fixed physical size; rebuild for the new scale.
        self.icons = IconCache::new(crate::picker::ICON_SIZE * factor as u32);
        // Invalidate the pixmap — next draw reallocates at the new physical size.
        self.pixmap = None;
        self.needs_redraw = true;
    }
    fn transform_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: wl_output::Transform,
    ) {
    }
    fn frame(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: u32,
    ) {
        // `frame` is a compositor "present-ready" signal, not a dirty signal.
        // Actual state changes set `needs_redraw` at their source (pointer,
        // keyboard, axis, configure, blink transition). Setting it here used
        // to lock us into a 60 fps redraw loop while the picker sat idle.
    }
    fn surface_enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }
    fn surface_leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }
}

impl OutputHandler for AppState {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }
    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
}

impl SeatHandler for AppState {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seat_state
    }
    fn new_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
    fn new_capability(
        &mut self,
        _: &Connection,
        qh: &QueueHandle<Self>,
        seat: wl_seat::WlSeat,
        capability: Capability,
    ) {
        if capability == Capability::Pointer && self.pointer.is_none() {
            let pointer = self.seat_state.get_pointer(qh, &seat).expect("pointer");
            self.cursor_shape_device =
                Some(self.cursor_shape_manager.get_shape_device(&pointer, qh));
            self.pointer = Some(pointer);
        }
        if capability == Capability::Keyboard && self.keyboard.is_none() {
            let loop_handle = self.loop_handle.clone();
            self.keyboard = Some(
                self.seat_state
                    .get_keyboard_with_repeat(
                        qh,
                        &seat,
                        None,
                        loop_handle,
                        Box::new(|state, _, event| {
                            state.handle_key_event(event);
                        }),
                    )
                    .expect("keyboard"),
            );
        }
    }
    fn remove_capability(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: wl_seat::WlSeat,
        _: Capability,
    ) {
    }
    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
}

impl PointerHandler for AppState {
    fn pointer_frame(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        pointer: &wl_pointer::WlPointer,
        events: &[PointerEvent],
    ) {
        for ev in events {
            match ev.kind {
                PointerEventKind::Enter { serial } => {
                    self.apply_cursor_for_phase(pointer, serial);
                    let (x, y) = ev.position;
                    self.cursor = Some((x as f32, y as f32));
                    self.needs_redraw = true;
                }
                PointerEventKind::Motion { .. } => {
                    let (x, y) = ev.position;
                    let (xf, yf) = (x as f32, y as f32);
                    self.cursor = Some((xf, yf));
                    if self.phase == Phase::Drawing && self.drawing {
                        self.commit_or_extend_drag(xf, yf);
                    }
                    self.needs_redraw = true;
                }
                PointerEventKind::Press { button, .. } => match (self.phase, button) {
                    (Phase::Drawing, BTN_LEFT) => {
                        self.drawing = true;
                        self.has_drawn = true;
                        let (x, y) = ev.position;
                        let (xf, yf) = (x as f32, y as f32);
                        self.stroke.clear();
                        self.rect_start = None;
                        // Don't commit to a gesture yet — wait for the cursor
                        // to clear `drag_threshold_px`. A release before that
                        // is treated as a click (natural-size spawn) instead
                        // of a zero-size rect.
                        self.drag_origin = Some((xf, yf));
                        self.drag_committed = false;
                        // Restart the shimmer phase so the pulse begins at the
                        // stroke origin every time the user picks up the pen.
                        self.stroke_anim_start = Instant::now();
                        self.needs_redraw = true;
                    }
                    (Phase::Drawing, BTN_RIGHT) => {
                        self.decision = Decision::Cancel;
                    }
                    (Phase::Picking, BTN_LEFT) => {
                        let (x, y) = ev.position;
                        if let Some(card) = self.last_card {
                            if !card.contains(x as i32, y as i32) {
                                self.decision = Decision::Cancel;
                                continue;
                            }
                            if let Some(rel_idx) =
                                card.item_at(y as i32, self.picker.visible_count())
                            {
                                let absolute = self.picker.scroll_offset() + rel_idx;
                                // If the click landed on the × button of a
                                // history row, forget it instead of spawning.
                                if card.forget_button_hit(x as i32, y as i32)
                                    && self.picker.is_history_row(absolute)
                                {
                                    self.picker.forget_at(absolute);
                                    self.needs_redraw = true;
                                    continue;
                                }
                                self.picker.select(absolute);
                                self.finalize_spawn();
                            }
                        }
                    }
                    (Phase::Picking, BTN_RIGHT) => {
                        self.decision = Decision::Cancel;
                    }
                    _ => {}
                },
                PointerEventKind::Release { button, .. } => {
                    if self.phase == Phase::Drawing && button == BTN_LEFT && self.drawing {
                        self.drawing = false;
                        self.rect_start = None;
                        // Click (no drag committed): seed the stroke with a
                        // single point at the press location so `bbox()`
                        // returns a zero-size rect positioned *at the click*,
                        // not (0, 0). Downstream still detects the click via
                        // `bbox.w == 0 && bbox.h == 0`, and now the caller
                        // has a usable centre point for spawning.
                        if !self.drag_committed {
                            self.stroke.clear();
                            if let Some((ox, oy)) = self.drag_origin {
                                self.stroke.push(ox, oy);
                            }
                        }
                        self.drag_origin = None;
                        self.drag_committed = false;
                        // Live-escape from --default: if Ctrl was held at
                        // release, the user wants the picker for this spawn
                        // instead of the pinned app. Clearing the preset makes
                        // enter_picker_phase fall through to the picker
                        // instead of spawning directly.
                        //
                        // Ctrl+drag now *also* draws freehand (see config
                        // `freehand_modifier`). The two behaviours don't
                        // collide because they live on different events:
                        // motion drives the stroke, release drives the
                        // preset-escape. The combined semantics are
                        // coherent — "I want to deviate from the happy path,
                        // both in shape and in target app."
                        //
                        // The apps list wasn't preloaded (we skipped the
                        // background scan assuming we wouldn't need it), so
                        // kick it off here; the picker will show "Loading…"
                        // briefly until the scan finishes.
                        if self.ctrl_held && self.preset_exec.is_some() {
                            self.preset_exec = None;
                            if self.apps_rx.is_none() {
                                let (rx, _) = apps::discover_async();
                                self.apps_rx = Some(rx);
                            }
                        }
                        self.enter_picker_phase();
                    }
                }
                PointerEventKind::Leave { .. } => {
                    self.cursor = None;
                    self.needs_redraw = true;
                }
                PointerEventKind::Axis {
                    vertical,
                    ..
                } => {
                    if self.phase == Phase::Picking {
                        let delta = if vertical.discrete != 0 {
                            vertical.discrete as isize
                        } else if vertical.absolute.abs() > 0.0 {
                            vertical.absolute.signum() as isize
                        } else {
                            0
                        };
                        if delta != 0 {
                            self.picker.scroll_by(delta);
                            self.needs_redraw = true;
                        }
                    }
                }
            }
        }
    }
}

impl KeyboardHandler for AppState {
    fn enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: &wl_surface::WlSurface,
        _: u32,
        _: &[u32],
        _: &[Keysym],
    ) {
    }
    fn leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: &wl_surface::WlSurface,
        _: u32,
    ) {
    }
    fn press_key(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        event: KeyEvent,
    ) {
        self.handle_key_event(event);
    }
    fn release_key(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        _: KeyEvent,
    ) {
    }
    fn update_modifiers(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        modifiers: Modifiers,
        _: u32,
    ) {
        let prev_ctrl = self.ctrl_held;
        self.shift_held = modifiers.shift;
        self.ctrl_held = modifiers.ctrl;
        self.alt_held = modifiers.alt;

        // The default-mode banner lights up while Ctrl is held (signals the
        // next release will escape to the picker). Refresh even when the user
        // isn't dragging so the visual feedback tracks the key state live.
        if self.phase == Phase::Drawing
            && self.preset_exec.is_some()
            && prev_ctrl != self.ctrl_held
        {
            self.needs_redraw = true;
        }

        // If a modifier changes mid-drag, re-resolve the effective mode and
        // reshape the stroke on the fly. We keep the drag origin as the
        // anchor so transitioning between rect/square/freehand feels
        // continuous instead of restarting the gesture.
        if self.phase == Phase::Drawing && self.drawing && self.drag_committed {
            let Some((cx, cy)) = self.cursor else { return };
            let mode = self.effective_mode();
            match mode {
                EffectiveMode::Rectangle | EffectiveMode::Square => {
                    let square = mode == EffectiveMode::Square;
                    if self.rect_start.is_none() {
                        // Coming from freehand: anchor at the drag origin so
                        // the new rect spans the full gesture, not just from
                        // the point where the modifier changed.
                        let anchor = self.drag_origin.unwrap_or((cx, cy));
                        self.rect_start = Some(anchor);
                    }
                    self.update_rect_stroke(cx, cy, square);
                }
                EffectiveMode::Freehand => {
                    // Going into freehand mid-drag: drop whatever shape we
                    // had and start a fresh stroke at the cursor. Seeding it
                    // with `drag_origin` drew a connecting line between the
                    // two points and made the transition feel like the
                    // gesture teleported — clearing and starting at the
                    // cursor matches what the user expects when they
                    // "switch tools" mid-gesture.
                    self.rect_start = None;
                    self.stroke.clear();
                    self.stroke.push(cx, cy);
                }
            }
            self.needs_redraw = true;
        }
    }
}

impl ShmHandler for AppState {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

impl LayerShellHandler for AppState {
    fn closed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &LayerSurface) {
        self.decision = Decision::Cancel;
    }

    fn configure(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &LayerSurface,
        configure: LayerSurfaceConfigure,
        _: u32,
    ) {
        let (w, h) = configure.new_size;
        self.width = w.max(1);
        self.height = h.max(1);
        self.configured = true;
        self.needs_redraw = true;
    }
}

delegate_compositor!(AppState);
delegate_output!(AppState);
delegate_seat!(AppState);
delegate_pointer!(AppState);
delegate_keyboard!(AppState);
delegate_shm!(AppState);
delegate_layer!(AppState);
delegate_registry!(AppState);

impl ProvidesRegistryState for AppState {
    registry_handlers![OutputState, SeatState];
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
}
