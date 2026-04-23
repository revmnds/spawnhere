use crate::apps::{self, App, IconCache};
use crate::history::History;
use crate::picker::{self, PickerState, TextRenderer};
use crate::stroke::{Bbox, Stroke};
use anyhow::{Context, Result};
use std::collections::HashMap;
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
use tiny_skia::{Color, Paint, PathBuilder, Pixmap, Stroke as SkStroke, Transform};
use wayland_client::Proxy;
use wayland_client::{
    globals::registry_queue_init,
    protocol::{wl_keyboard, wl_output, wl_pointer, wl_seat, wl_shm, wl_surface},
    Connection, QueueHandle,
};

pub enum Outcome {
    /// User completed a gesture and (if no preset `--spawn`) chose an app.
    /// `bbox` is post-padding + min-size enforcement in **global** compositor
    /// coords (i.e. the coordinate space Hyprland's `[move X Y]` dispatch
    /// expects). `output_rect` is the monitor rect the window will land on —
    /// the caller should clamp to this after applying per-app rule expansions.
    Spawn { bbox: Bbox, exec: String, output_rect: Bbox },
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

/// One overlay per monitor. Each owns its own surface, pixmap, and scale.
/// Strokes + bboxes live in a single global coord frame (the compositor's
/// logical coordinate space); each overlay translates to its local frame at
/// render time via `origin`.
struct OutputOverlay {
    output: wl_output::WlOutput,
    surface: LayerSurface,
    /// Top-left in the compositor's global logical coord space. `(0, 0)` is a
    /// sane default for single-monitor setups where Hyprland doesn't emit a
    /// logical-position event until layout settles.
    origin: (i32, i32),
    /// Surface size in **logical** pixels (== this output's logical size for
    /// fullscreen layer surfaces).
    width: u32,
    height: u32,
    scale: i32,
    configured: bool,
    /// Reused across frames — reallocated only when physical dims change.
    pixmap: Option<Pixmap>,
}

impl OutputOverlay {
    fn rect(&self) -> Bbox {
        Bbox {
            x: self.origin.0,
            y: self.origin.1,
            w: self.width,
            h: self.height,
        }
    }

    fn contains_global(&self, gx: i32, gy: i32) -> bool {
        self.rect().contains_point(gx, gy)
    }
}

struct AppState {
    registry_state: RegistryState,
    output_state: OutputState,
    shm: Shm,
    seat_state: SeatState,
    compositor_state: CompositorState,
    layer_shell: LayerShell,

    pool: SlotPool,
    /// One overlay per attached output. Populated via `OutputHandler::new_output`
    /// and torn down via `output_destroyed`.
    overlays: Vec<OutputOverlay>,
    /// Per-scale icon caches so a 1× + 2× mixed setup stays sharp on both
    /// without duplicate rasterization work.
    icon_caches: HashMap<i32, IconCache>,

    needs_redraw: bool,

    pointer: Option<wl_pointer::WlPointer>,
    keyboard: Option<wl_keyboard::WlKeyboard>,

    /// Hyprland always advertises `wp_cursor_shape_v1`; we rely on it to show
    /// a proper arrow during Picking phase.
    cursor_shape_manager: CursorShapeManager,
    cursor_shape_device: Option<WpCursorShapeDeviceV1>,

    /// Cursor position in GLOBAL compositor logical coords. `None` when the
    /// pointer is outside all of our overlays (not physically possible on a
    /// fully-covered desktop, but can happen briefly during output hotplug).
    cursor_global: Option<(f32, f32)>,
    drawing: bool,
    has_drawn: bool,
    /// Stroke points are stored in **global** coords. Each overlay subtracts
    /// its `origin` at render time to draw its slice.
    stroke: Stroke,
    /// Live shift-modifier state. When true during the press/drag, the stroke
    /// is replaced with a clean rectangle from press point to current cursor
    /// instead of capturing freehand points.
    shift_held: bool,
    /// Press-point of a rectangle drag, in global coords. `Some` while
    /// `drawing && shift_held`, `None` in freehand mode.
    rect_start: Option<(f32, f32)>,

    phase: Phase,
    preset_exec: Option<String>,
    picker: PickerState,
    text: TextRenderer,
    apps_rx: Option<Receiver<Vec<App>>>,
    /// Picker card's position in GLOBAL coords.
    last_card: Option<picker::CardRect>,

    /// Reset on every typing event so the caret stays solid for one half-cycle
    /// after input (matches standard text-input UX).
    caret_blink_reset: Instant,
    /// Last caret visibility we committed to the screen. Used to suppress
    /// redraws on blink ticks when nothing actually changed.
    caret_last_visible: bool,

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
         magicwand draws its overlay via this protocol; Hyprland supports it \
         natively. If you're seeing this under Hyprland, your Hyprland version \
         may be too old — 0.30+ is recommended.",
    )?;
    let shm = Shm::bind(&globals, &qh).context("wl_shm not available")?;

    // Hyprland always exposes `wp_cursor_shape_v1`; bail hard if missing so
    // we fail loudly rather than silently falling back to a hidden cursor.
    let cursor_shape_manager = CursorShapeManager::bind(&globals, &qh).context(
        "wp_cursor_shape_v1 not advertised. This is part of every modern Hyprland \
         build — please update Hyprland (0.34+ recommended).",
    )?;

    // SlotPool grows on demand. Start with 1080p @ 2× so a typical HiDPI
    // laptop overlay doesn't need to grow on first frame.
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
        compositor_state,
        layer_shell,

        pool,
        overlays: Vec::new(),
        icon_caches: HashMap::new(),

        needs_redraw: false,

        pointer: None,
        keyboard: None,

        cursor_shape_manager,
        cursor_shape_device: None,

        cursor_global: None,
        drawing: false,
        has_drawn: false,
        stroke: Stroke::new(),
        shift_held: false,
        rect_start: None,

        phase: Phase::Drawing,
        preset_exec,
        picker: PickerState::new(history),
        text: TextRenderer::new(),
        apps_rx,
        last_card: None,

        caret_blink_reset: Instant::now(),
        caret_last_visible: true,

        decision: Decision::Pending,
        loop_handle,
        loop_signal,
    };

    event_loop
        .run(Duration::from_millis(16), &mut state, |state| {
            if state.picker.loading() {
                if let Some(rx) = &state.apps_rx {
                    if let Ok(apps) = rx.try_recv() {
                        state.picker.set_apps(apps);
                        state.needs_redraw = true;
                    }
                }
            }

            if state.needs_redraw && state.any_configured() {
                if let Err(e) = state.draw_all(&qh) {
                    eprintln!("magicwand: draw failed: {e:#}");
                }
                state.needs_redraw = false;
            }

            if state.decision != Decision::Pending {
                state.loop_signal.stop();
            }
        })
        .context("running calloop event loop")?;

    // Union of all output rects — used if we somehow can't resolve a specific
    // output for the gesture (shouldn't happen, but we never want to spawn at
    // 0×0).
    let desktop = state.desktop_rect();
    let stroke = std::mem::take(&mut state.stroke);
    Ok(match std::mem::replace(&mut state.decision, Decision::Cancel) {
        Decision::Spawn(exec) => {
            let raw = stroke.bbox(padding);
            if raw.w == 0 && raw.h == 0 {
                Outcome::Cancelled
            } else {
                let enforced = raw.enforce_min(min_width, min_height);
                let cx = enforced.x + enforced.w as i32 / 2;
                let cy = enforced.y + enforced.h as i32 / 2;
                let output_rect = state
                    .overlay_containing_global(cx, cy)
                    .map(|i| state.overlays[i].rect())
                    .unwrap_or(desktop);
                let bbox = enforced.clamp_to_rect(output_rect);
                Outcome::Spawn { bbox, exec, output_rect }
            }
        }
        _ => Outcome::Cancelled,
    })
}

impl AppState {
    fn any_configured(&self) -> bool {
        self.overlays.iter().any(|o| o.configured)
    }

    fn desktop_rect(&self) -> Bbox {
        let mut it = self.overlays.iter().filter(|o| o.configured);
        let Some(first) = it.next() else {
            return Bbox { x: 0, y: 0, w: 1, h: 1 };
        };
        let mut min_x = first.origin.0;
        let mut min_y = first.origin.1;
        let mut max_x = first.origin.0 + first.width as i32;
        let mut max_y = first.origin.1 + first.height as i32;
        for o in it {
            min_x = min_x.min(o.origin.0);
            min_y = min_y.min(o.origin.1);
            max_x = max_x.max(o.origin.0 + o.width as i32);
            max_y = max_y.max(o.origin.1 + o.height as i32);
        }
        Bbox {
            x: min_x,
            y: min_y,
            w: (max_x - min_x).max(1) as u32,
            h: (max_y - min_y).max(1) as u32,
        }
    }

    fn overlay_for_surface(&self, surface: &wl_surface::WlSurface) -> Option<usize> {
        self.overlays
            .iter()
            .position(|o| o.surface.wl_surface().id() == surface.id())
    }

    fn overlay_for_layer(&self, layer: &LayerSurface) -> Option<usize> {
        self.overlays
            .iter()
            .position(|o| o.surface.wl_surface().id() == layer.wl_surface().id())
    }

    fn overlay_containing_global(&self, gx: i32, gy: i32) -> Option<usize> {
        self.overlays
            .iter()
            .position(|o| o.configured && o.contains_global(gx, gy))
    }

    /// Lazily create an IconCache for the given scale. All overlays on that
    /// scale share the cache.
    fn ensure_icon_cache(&mut self, scale: i32) {
        self.icon_caches
            .entry(scale)
            .or_insert_with(|| IconCache::new(24 * scale.max(1) as u32));
    }

    fn finalize_spawn(&mut self) {
        if let Some(app) = self.picker.selected_app() {
            self.decision = Decision::Spawn(app.exec.clone());
        } else if self.picker.loading() {
            // Apps still scanning — keep waiting; user can ESC to cancel.
        } else {
            // No matches for query — do nothing on Enter.
        }
    }

    /// Replace the current stroke with the 5 corner points of an axis-aligned
    /// rectangle from `rect_start` to `(cx, cy)`. Points are in global coords.
    fn update_rect_stroke(&mut self, cx: f32, cy: f32) {
        let Some((sx, sy)) = self.rect_start else { return };
        self.stroke.clear();
        let (x0, x1) = (sx.min(cx), sx.max(cx));
        let (y0, y1) = (sy.min(cy), sy.max(cy));
        self.stroke.push(x0, y0);
        self.stroke.push(x1, y0);
        self.stroke.push(x1, y1);
        self.stroke.push(x0, y1);
        self.stroke.push(x0, y0);
    }

    fn enter_picker_phase(&mut self) {
        if let Some(exec) = &self.preset_exec {
            self.decision = Decision::Spawn(exec.clone());
            return;
        }
        self.phase = Phase::Picking;
        self.caret_blink_reset = Instant::now();
        self.refresh_cursor();
        self.needs_redraw = true;
    }

    fn refresh_cursor(&self) {
        let Some(pointer) = self.pointer.as_ref() else { return };
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
            Phase::Drawing => pointer.set_cursor(serial, None, 0, 0),
            Phase::Picking => {
                if let Some(device) = self.cursor_shape_device.as_ref() {
                    device.set_shape(serial, Shape::Default);
                }
            }
        }
    }

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

    /// Compute the picker card's **global** position — centered on the
    /// bbox center, then clamped inside the overlay that contains that center
    /// so the card stays on one monitor.
    fn picker_card_center(&self) -> Option<((i32, i32), Bbox)> {
        let bbox = self.stroke.bbox(0);
        let cx = bbox.x + bbox.w as i32 / 2;
        let cy = bbox.y + bbox.h as i32 / 2;
        let idx = self.overlay_containing_global(cx, cy)?;
        Some(((cx, cy), self.overlays[idx].rect()))
    }

    fn draw_all(&mut self, qh: &QueueHandle<Self>) -> Result<()> {
        let picking = self.phase == Phase::Picking;
        let caret_visible = if picking {
            caret_visible_at(self.caret_blink_reset.elapsed())
        } else {
            false
        };
        if picking {
            self.caret_last_visible = caret_visible;
        }

        // Compute picker card anchor once, in global coords.
        let card_anchor = if picking { self.picker_card_center() } else { None };

        // Pre-compute hovered_rel against the (possibly-stale) previous card
        // rect; picker::draw will refresh `last_card` with the new value.
        let hovered_rel = if picking {
            self.last_card.and_then(|card| {
                let (cx, cy) = self.cursor_global?;
                if !card.contains(cx as i32, cy as i32) {
                    return None;
                }
                card.item_at(cy as i32, self.picker.visible_count())
            })
        } else {
            None
        };

        // Ensure an icon cache exists for every active scale BEFORE the render
        // loop so we can borrow mutably one at a time inside the loop without
        // re-hashing per overlay.
        for overlay in &self.overlays {
            self.icon_caches
                .entry(overlay.scale)
                .or_insert_with(|| IconCache::new(24 * overlay.scale.max(1) as u32));
        }

        let mut new_card: Option<picker::CardRect> = None;
        for idx in 0..self.overlays.len() {
            if !self.overlays[idx].configured {
                continue;
            }
            let drawn_card = self.draw_overlay(
                idx,
                qh,
                caret_visible,
                card_anchor,
                hovered_rel,
            )?;
            if let Some(c) = drawn_card {
                new_card = Some(c);
            }
        }
        if picking {
            self.last_card = new_card.or(self.last_card);
        }
        Ok(())
    }

    fn draw_overlay(
        &mut self,
        idx: usize,
        qh: &QueueHandle<Self>,
        caret_visible: bool,
        card_anchor: Option<((i32, i32), Bbox)>,
        hovered_rel: Option<usize>,
    ) -> Result<Option<picker::CardRect>> {
        let (origin, w_log, h_log, scale) = {
            let o = &self.overlays[idx];
            (o.origin, o.width, o.height, o.scale.max(1) as u32)
        };
        if w_log == 0 || h_log == 0 {
            return Ok(None);
        }

        let w_phys = w_log * scale;
        let h_phys = h_log * scale;
        let stride = w_phys as i32 * 4;
        let (buffer, canvas) = self
            .pool
            .create_buffer(w_phys as i32, h_phys as i32, stride, wl_shm::Format::Argb8888)
            .context("creating shm buffer")?;

        // Ensure this overlay's pixmap is sized to physical dims.
        {
            let overlay = &mut self.overlays[idx];
            let needs_new = overlay
                .pixmap
                .as_ref()
                .map(|p| p.width() != w_phys || p.height() != h_phys)
                .unwrap_or(true);
            if needs_new {
                overlay.pixmap = Some(Pixmap::new(w_phys, h_phys).context("creating pixmap")?);
            }
        }

        let phase = self.phase;
        let drawing = self.drawing;
        let has_drawn = self.has_drawn;
        let cursor_global = self.cursor_global;
        let stroke_alpha = match phase {
            Phase::Drawing => STROKE_COLOR.3,
            Phase::Picking => STROKE_DIM_ALPHA,
        };

        // Card (if picking) — translate global position to this overlay's local
        // frame so picker::draw sees a centered card.
        let mut drawn_card: Option<picker::CardRect> = None;

        let pixmap = self.overlays[idx].pixmap.as_mut().expect("pixmap just ensured");
        pixmap.fill(Color::from_rgba8(0, 0, 0, BG_DIM_ALPHA));

        draw_stroke_global(pixmap, &self.stroke, stroke_alpha, scale, origin);

        if phase == Phase::Drawing && !drawing && !has_drawn {
            if let Some((gx, gy)) = cursor_global {
                let lx = gx - origin.0 as f32;
                let ly = gy - origin.1 as f32;
                if lx >= 0.0 && ly >= 0.0 && lx < w_log as f32 && ly < h_log as f32 {
                    draw_crosshair(pixmap, lx, ly, scale);
                }
            }
        }

        if phase == Phase::Picking {
            if let Some(((gcx, gcy), home_rect)) = card_anchor {
                // Only the "home" overlay gets the card rendered; the others
                // just show the dim background (and the faded stroke).
                if home_rect.x == origin.0 && home_rect.y == origin.1 {
                    let local_cx = gcx - origin.0;
                    let local_cy = gcy - origin.1;
                    let icons = self
                        .icon_caches
                        .get_mut(&(scale as i32))
                        .expect("icon cache ensured");
                    let card_local = picker::draw(
                        pixmap,
                        &self.picker,
                        &mut self.text,
                        icons,
                        (local_cx, local_cy),
                        (w_log, h_log),
                        caret_visible,
                        hovered_rel.and_then(|r| {
                            // hovered_rel comes from the *previous* card (global);
                            // pass through only if the cursor is still inside
                            // this overlay's frame.
                            let (gx, gy) = cursor_global?;
                            let lx = gx - origin.0 as f32;
                            let ly = gy - origin.1 as f32;
                            if lx < 0.0 || ly < 0.0 || lx >= w_log as f32 || ly >= h_log as f32 {
                                None
                            } else {
                                Some(r)
                            }
                        }),
                        scale,
                    );
                    // Promote local card rect back to global for hit-tests.
                    drawn_card = Some(picker::CardRect {
                        x: card_local.x + origin.0,
                        y: card_local.y + origin.1,
                        w: card_local.w,
                        h: card_local.h,
                        recent_header_y: card_local.recent_header_y.map(|v| v + origin.1),
                        others_header_y: card_local.others_header_y.map(|v| v + origin.1),
                        recent_count: card_local.recent_count,
                    });
                }
            }
        }

        // tiny-skia is RGBA in memory; Wayland Argb8888 on LE is BGRA byte order.
        let src = pixmap.data();
        for (dst, chunk) in canvas.chunks_exact_mut(4).zip(src.chunks_exact(4)) {
            dst[0] = chunk[2];
            dst[1] = chunk[1];
            dst[2] = chunk[0];
            dst[3] = chunk[3];
        }

        let surface = self.overlays[idx].surface.wl_surface();
        surface.damage_buffer(0, 0, w_phys as i32, h_phys as i32);
        buffer
            .attach_to(surface)
            .context("attaching buffer to surface")?;
        surface.frame(qh, surface.clone());
        surface.commit();
        Ok(drawn_card)
    }
}

/// Draw the stroke onto a single overlay's pixmap. `origin` subtracts the
/// overlay's global top-left so points outside this overlay fall off-pixmap
/// (tiny-skia clips naturally); continuous cross-monitor strokes thus render
/// as seamless lines across adjacent overlays.
fn draw_stroke_global(
    pixmap: &mut Pixmap,
    stroke: &Stroke,
    alpha: u8,
    scale: u32,
    origin: (i32, i32),
) {
    let pts = stroke.points();
    if pts.len() < 2 {
        return;
    }
    let ox = origin.0 as f32;
    let oy = origin.1 as f32;
    let mut pb = PathBuilder::new();
    pb.move_to(pts[0].x - ox, pts[0].y - oy);
    for p in &pts[1..] {
        pb.line_to(p.x - ox, p.y - oy);
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
        let Some(idx) = self.overlay_for_surface(surface) else { return };
        if factor == self.overlays[idx].scale {
            return;
        }
        self.overlays[idx].scale = factor;
        surface.set_buffer_scale(factor);
        self.overlays[idx].pixmap = None;
        self.ensure_icon_cache(factor);
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

    fn new_output(&mut self, _: &Connection, qh: &QueueHandle<Self>, output: wl_output::WlOutput) {
        // Avoid double-creating if we somehow get a duplicate event.
        if self
            .overlays
            .iter()
            .any(|o| o.output.id() == output.id())
        {
            return;
        }
        let info = self.output_state.info(&output);
        let origin = info
            .as_ref()
            .and_then(|i| i.logical_position)
            .unwrap_or((0, 0));
        let scale = info.as_ref().map(|i| i.scale_factor).unwrap_or(1).max(1);

        let surface = self.compositor_state.create_surface(qh);
        let layer = self.layer_shell.create_layer_surface(
            qh,
            surface,
            Layer::Overlay,
            Some("magicwand"),
            Some(&output),
        );
        layer.set_anchor(Anchor::TOP | Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT);
        layer.set_keyboard_interactivity(KeyboardInteractivity::Exclusive);
        layer.set_exclusive_zone(-1);
        layer.set_size(0, 0);
        layer.wl_surface().set_buffer_scale(scale);
        layer.commit();

        self.ensure_icon_cache(scale);
        self.overlays.push(OutputOverlay {
            output,
            surface: layer,
            origin,
            width: 0,
            height: 0,
            scale,
            configured: false,
            pixmap: None,
        });
    }

    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, output: wl_output::WlOutput) {
        let info = self.output_state.info(&output);
        let Some(idx) = self.overlays.iter().position(|o| o.output.id() == output.id()) else {
            return;
        };
        if let Some(pos) = info.as_ref().and_then(|i| i.logical_position) {
            if self.overlays[idx].origin != pos {
                self.overlays[idx].origin = pos;
                self.needs_redraw = true;
            }
        }
    }

    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, output: wl_output::WlOutput) {
        self.overlays.retain(|o| o.output.id() != output.id());
        self.needs_redraw = true;
    }
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
            let Some(idx) = self.overlay_for_surface(&ev.surface) else {
                continue;
            };
            let origin = self.overlays[idx].origin;
            let (lx, ly) = ev.position;
            let gx = lx as f32 + origin.0 as f32;
            let gy = ly as f32 + origin.1 as f32;

            match ev.kind {
                PointerEventKind::Enter { serial } => {
                    self.apply_cursor_for_phase(pointer, serial);
                    self.cursor_global = Some((gx, gy));
                    self.needs_redraw = true;
                }
                PointerEventKind::Motion { .. } => {
                    self.cursor_global = Some((gx, gy));
                    if self.phase == Phase::Drawing && self.drawing {
                        if self.rect_start.is_some() {
                            self.update_rect_stroke(gx, gy);
                        } else {
                            self.stroke.push(gx, gy);
                        }
                    }
                    self.needs_redraw = true;
                }
                PointerEventKind::Press { button, .. } => match (self.phase, button) {
                    (Phase::Drawing, BTN_LEFT) => {
                        self.drawing = true;
                        self.has_drawn = true;
                        self.stroke.clear();
                        if self.shift_held {
                            self.rect_start = Some((gx, gy));
                            self.update_rect_stroke(gx, gy);
                        } else {
                            self.rect_start = None;
                            self.stroke.push(gx, gy);
                        }
                        self.needs_redraw = true;
                    }
                    (Phase::Drawing, BTN_RIGHT) => {
                        self.decision = Decision::Cancel;
                    }
                    (Phase::Picking, BTN_LEFT) => {
                        if let Some(card) = self.last_card {
                            if !card.contains(gx as i32, gy as i32) {
                                self.decision = Decision::Cancel;
                                continue;
                            }
                            if let Some(rel_idx) =
                                card.item_at(gy as i32, self.picker.visible_count())
                            {
                                let absolute = self.picker.scroll_offset() + rel_idx;
                                if card.forget_button_hit(gx as i32, gy as i32)
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
                    // Only clear the cursor if the pointer has truly left
                    // every overlay — during a cross-monitor drag we'll see
                    // Leave on A before Enter on B.
                    self.cursor_global = None;
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
        if self.phase == Phase::Drawing && self.drawing {
            if self.shift_held && self.rect_start.is_none() {
                if let Some((cx, cy)) = self.cursor_global {
                    self.rect_start = Some((cx, cy));
                    self.update_rect_stroke(cx, cy);
                }
            } else if !self.shift_held && self.rect_start.is_some() {
                self.rect_start = None;
                self.stroke.clear();
                if let Some((cx, cy)) = self.cursor_global {
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
    fn closed(&mut self, _: &Connection, _: &QueueHandle<Self>, layer: &LayerSurface) {
        // A single output's overlay closing (e.g. hotplug) shouldn't cancel
        // the whole gesture — only cancel if the LAST overlay dies.
        if let Some(idx) = self.overlay_for_layer(layer) {
            self.overlays.remove(idx);
        }
        if self.overlays.is_empty() {
            self.decision = Decision::Cancel;
        }
    }

    fn configure(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        layer: &LayerSurface,
        configure: LayerSurfaceConfigure,
        _: u32,
    ) {
        let Some(idx) = self.overlay_for_layer(layer) else { return };
        let (w, h) = configure.new_size;
        self.overlays[idx].width = w.max(1);
        self.overlays[idx].height = h.max(1);
        self.overlays[idx].configured = true;
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
