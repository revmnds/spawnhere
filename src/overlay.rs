use crate::apps::{self, App, IconCache};
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
use tiny_skia::{Color, Paint, PathBuilder, Pixmap, Stroke as SkStroke, Transform};
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
    pub min_width: u32,
    pub min_height: u32,
    pub history: History,
}

/// Caret blink half-period (ms). Standard system blink is ~530ms per half cycle.
const CARET_BLINK_MS: u128 = 530;
/// How often we wake the loop to redraw the caret. Half the half-period so we
/// never miss a transition.
const BLINK_TICK: Duration = Duration::from_millis(265);

fn caret_visible_at(since_reset: Duration) -> bool {
    (since_reset.as_millis() / CARET_BLINK_MS).is_multiple_of(2)
}

const BG_DIM_ALPHA: u8 = 64;
const STROKE_COLOR: (u8, u8, u8, u8) = (176, 128, 255, 255);
const STROKE_WIDTH: f32 = 4.0;
const STROKE_DIM_ALPHA: u8 = 102; // ~0.4 * 255 — used during Picking phase

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
    /// Live shift-modifier state. When true during the press/drag, the stroke
    /// is replaced with a clean rectangle from press point to current cursor
    /// instead of capturing freehand points.
    shift_held: bool,
    /// Press-point of a rectangle drag. `Some` while `drawing && shift_held`
    /// (and set at press time), `None` in freehand mode.
    rect_start: Option<(f32, f32)>,

    phase: Phase,
    preset_exec: Option<String>,
    picker: PickerState,
    text: TextRenderer,
    icons: IconCache,
    apps_rx: Option<Receiver<Vec<App>>>,
    last_card: Option<picker::CardRect>,

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
    let RunConfig { preset_exec, padding, min_width, min_height, history } = cfg;
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
        rect_start: None,

        phase: Phase::Drawing,
        preset_exec,
        picker: PickerState::new(history),
        text: TextRenderer::new(),
        icons: IconCache::new(24),
        apps_rx,
        last_card: None,

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
            let raw = stroke.bbox(padding);
            if raw.w == 0 && raw.h == 0 {
                // Click without drag — treat as cancel rather than spawn at 0×0.
                Outcome::Cancelled
            } else {
                let bbox = raw.enforce_min(min_width, min_height).clamp_to_rect(screen);
                Outcome::Spawn { bbox, exec, screen }
            }
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
    fn update_rect_stroke(&mut self, cx: f32, cy: f32) {
        let Some((sx, sy)) = self.rect_start else { return };
        self.stroke.clear();
        let (x0, x1) = (sx.min(cx), sx.max(cx));
        let (y0, y1) = (sy.min(cy), sy.max(cy));
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
        draw_stroke(pixmap, &self.stroke, stroke_alpha, scale);

        if self.phase == Phase::Drawing && !self.drawing && !self.has_drawn {
            if let Some((cx, cy)) = self.cursor {
                draw_crosshair(pixmap, cx, cy, scale);
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
            let card = picker::draw(
                pixmap,
                &self.picker,
                &mut self.text,
                &mut self.icons,
                (cx, cy),
                (w_log, h_log),
                caret_visible,
                hovered_rel,
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

fn draw_stroke(pixmap: &mut Pixmap, stroke: &Stroke, alpha: u8, scale: u32) {
    let pts = stroke.points();
    if pts.len() < 2 {
        return;
    }
    let mut pb = PathBuilder::new();
    pb.move_to(pts[0].x, pts[0].y);
    for p in &pts[1..] {
        pb.line_to(p.x, p.y);
    }
    let Some(path) = pb.finish() else { return };
    let mut paint = Paint::default();
    paint.set_color_rgba8(STROKE_COLOR.0, STROKE_COLOR.1, STROKE_COLOR.2, alpha);
    paint.anti_alias = true;
    let s = scale as f32;
    let sk = SkStroke {
        width: STROKE_WIDTH * s,
        line_cap: tiny_skia::LineCap::Round,
        line_join: tiny_skia::LineJoin::Round,
        ..Default::default()
    };
    pixmap.stroke_path(&path, &paint, &sk, Transform::from_scale(s, s), None);
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
        self.icons = IconCache::new(24 * factor as u32);
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
                    self.cursor = Some((x as f32, y as f32));
                    if self.phase == Phase::Drawing && self.drawing {
                        if self.rect_start.is_some() {
                            self.update_rect_stroke(x as f32, y as f32);
                        } else {
                            self.stroke.push(x as f32, y as f32);
                        }
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
                        if self.shift_held {
                            self.rect_start = Some((xf, yf));
                            self.update_rect_stroke(xf, yf);
                        } else {
                            self.rect_start = None;
                            self.stroke.push(xf, yf);
                        }
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
        self.shift_held = modifiers.shift;
        // If the user pressed/released Shift mid-drag, snap the stroke to
        // the corresponding mode immediately.
        if self.phase == Phase::Drawing && self.drawing {
            if self.shift_held && self.rect_start.is_none() {
                if let Some((cx, cy)) = self.cursor {
                    self.rect_start = Some((cx, cy));
                    self.update_rect_stroke(cx, cy);
                }
            } else if !self.shift_held && self.rect_start.is_some() {
                self.rect_start = None;
                self.stroke.clear();
                if let Some((cx, cy)) = self.cursor {
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
