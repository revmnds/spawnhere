use crate::overlay::Outcome;
use crate::stroke::Stroke;
use anyhow::{Context, Result};

use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState},
    delegate_compositor, delegate_keyboard, delegate_layer, delegate_output, delegate_pointer,
    delegate_registry, delegate_seat, delegate_shm,
    output::{OutputHandler, OutputState},
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    seat::{
        keyboard::{KeyEvent, KeyboardHandler, Keysym, Modifiers},
        pointer::{PointerEvent, PointerEventKind, PointerHandler, BTN_LEFT, BTN_RIGHT},
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
use wayland_client::{
    globals::registry_queue_init,
    protocol::{wl_keyboard, wl_output, wl_pointer, wl_seat, wl_shm, wl_surface},
    Connection, QueueHandle,
};

const BG_DIM_ALPHA: u8 = 64;
const STROKE_COLOR: (u8, u8, u8, u8) = (176, 128, 255, 255); // #B080FF
const STROKE_WIDTH: f32 = 4.0;

#[derive(Clone, Copy, PartialEq)]
enum Decision {
    Pending,
    Spawn,
    Cancel,
}

struct AppState {
    registry_state: RegistryState,
    output_state: OutputState,
    #[allow(dead_code)]
    compositor_state: CompositorState,
    shm: Shm,
    seat_state: SeatState,

    pool: SlotPool,
    layer: LayerSurface,

    width: u32,
    height: u32,
    configured: bool,
    needs_redraw: bool,

    pointer: Option<wl_pointer::WlPointer>,
    keyboard: Option<wl_keyboard::WlKeyboard>,

    cursor: Option<(f32, f32)>,
    drawing: bool,
    has_drawn: bool,
    stroke: Stroke,

    decision: Decision,
}

pub fn run() -> Result<Outcome> {
    let conn = Connection::connect_to_env().context("connecting to Wayland display")?;
    let (globals, mut event_queue) =
        registry_queue_init(&conn).context("initializing Wayland registry")?;
    let qh = event_queue.handle();

    let compositor_state =
        CompositorState::bind(&globals, &qh).context("wl_compositor not available")?;
    let layer_shell = LayerShell::bind(&globals, &qh)
        .context("zwlr_layer_shell_v1 unavailable — needs Hyprland/Sway/wlroots")?;
    let shm = Shm::bind(&globals, &qh).context("wl_shm not available")?;

    let surface = compositor_state.create_surface(&qh);
    let layer =
        layer_shell.create_layer_surface(&qh, surface, Layer::Overlay, Some("magicwand"), None);
    layer.set_anchor(Anchor::TOP | Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT);
    layer.set_keyboard_interactivity(KeyboardInteractivity::Exclusive);
    layer.set_exclusive_zone(0);
    layer.set_size(0, 0);
    layer.commit();

    let pool = SlotPool::new(3840 * 2160 * 4, &shm).context("creating shm pool")?;

    let mut state = AppState {
        registry_state: RegistryState::new(&globals),
        output_state: OutputState::new(&globals, &qh),
        compositor_state,
        shm,
        seat_state: SeatState::new(&globals, &qh),

        pool,
        layer,

        width: 0,
        height: 0,
        configured: false,
        needs_redraw: false,

        pointer: None,
        keyboard: None,

        cursor: None,
        drawing: false,
        has_drawn: false,
        stroke: Stroke::new(),

        decision: Decision::Pending,
    };

    while state.decision == Decision::Pending {
        event_queue
            .blocking_dispatch(&mut state)
            .context("dispatching Wayland events")?;

        if state.configured && state.needs_redraw {
            state.draw(&qh)?;
            state.needs_redraw = false;
        }
    }

    Ok(match state.decision {
        Decision::Spawn => Outcome::Spawn(state.stroke),
        _ => Outcome::Cancelled,
    })
}

impl AppState {
    fn draw(&mut self, qh: &QueueHandle<Self>) -> Result<()> {
        let (w, h) = (self.width, self.height);
        if w == 0 || h == 0 {
            return Ok(());
        }
        let stride = w as i32 * 4;
        let (buffer, canvas) = self
            .pool
            .create_buffer(w as i32, h as i32, stride, wl_shm::Format::Argb8888)
            .context("creating shm buffer")?;

        let mut pixmap = Pixmap::new(w, h).context("creating pixmap")?;
        pixmap.fill(Color::from_rgba8(0, 0, 0, BG_DIM_ALPHA));

        let pts = self.stroke.points();
        if pts.len() >= 2 {
            let mut pb = PathBuilder::new();
            pb.move_to(pts[0].x, pts[0].y);
            for p in &pts[1..] {
                pb.line_to(p.x, p.y);
            }
            if let Some(path) = pb.finish() {
                let mut paint = Paint::default();
                paint.set_color_rgba8(
                    STROKE_COLOR.0,
                    STROKE_COLOR.1,
                    STROKE_COLOR.2,
                    STROKE_COLOR.3,
                );
                paint.anti_alias = true;
                let mut sk = SkStroke::default();
                sk.width = STROKE_WIDTH;
                sk.line_cap = tiny_skia::LineCap::Round;
                sk.line_join = tiny_skia::LineJoin::Round;
                pixmap.stroke_path(&path, &paint, &sk, Transform::identity(), None);
            }
        }

        // Cursor crosshair when not yet drawing — visual hint that user must click.
        if !self.drawing && !self.has_drawn {
            if let Some((cx, cy)) = self.cursor {
                draw_crosshair(&mut pixmap, cx, cy);
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

fn draw_crosshair(pixmap: &mut Pixmap, x: f32, y: f32) {
    let mut paint = Paint::default();
    paint.set_color_rgba8(255, 255, 255, 200);
    paint.anti_alias = true;
    let mut sk = SkStroke::default();
    sk.width = 1.5;

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
        pixmap.stroke_path(&path, &paint, &sk, Transform::identity(), None);
    }
}

impl CompositorHandler for AppState {
    fn scale_factor_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: i32,
    ) {
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
        self.needs_redraw = true;
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
            self.pointer = Some(self.seat_state.get_pointer(qh, &seat).expect("pointer"));
        }
        if capability == Capability::Keyboard && self.keyboard.is_none() {
            self.keyboard = Some(
                self.seat_state
                    .get_keyboard(qh, &seat, None)
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
        _: &wl_pointer::WlPointer,
        events: &[PointerEvent],
    ) {
        for ev in events {
            match ev.kind {
                PointerEventKind::Enter { .. } | PointerEventKind::Motion { .. } => {
                    let (x, y) = ev.position;
                    self.cursor = Some((x as f32, y as f32));
                    if self.drawing {
                        self.stroke.push(x as f32, y as f32);
                    }
                    self.needs_redraw = true;
                }
                PointerEventKind::Press { button, .. } => match button {
                    BTN_LEFT => {
                        self.drawing = true;
                        self.has_drawn = true;
                        let (x, y) = ev.position;
                        self.stroke.push(x as f32, y as f32);
                        self.needs_redraw = true;
                    }
                    BTN_RIGHT => {
                        self.decision = Decision::Cancel;
                    }
                    _ => {}
                },
                PointerEventKind::Release { button, .. } => {
                    if button == BTN_LEFT && self.drawing {
                        self.drawing = false;
                        self.decision = Decision::Spawn;
                    }
                }
                PointerEventKind::Leave { .. } => {
                    self.cursor = None;
                    self.needs_redraw = true;
                }
                _ => {}
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
        if event.keysym == Keysym::Escape {
            self.decision = Decision::Cancel;
        }
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
        _: Modifiers,
        _: u32,
    ) {
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
