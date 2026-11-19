//! CPU renderer for Remote Desktop.
//!
//! In an RDP session there is usually no real GPU, and every GPU-presented frame is sent
//! to the client as a full-window image. That makes any hover effect lag badly. This
//! runner rasterises egui on the CPU, compares each frame with the previous one in
//! 64×64 tiles, and presents only the changed tiles through GDI (`softbuffer`). RDP then
//! transmits just those small rectangles — the same reason classic Win32 apps feel fast
//! over Remote Desktop.

use crate::ui::App;
use egui_software_backend::{BufferMutRef, ColorFieldOrder, EguiSoftwareRender};
use std::num::NonZeroU32;
use std::rc::Rc;
use std::time::{Duration, Instant};
use winit::application::ApplicationHandler;
use winit::event::{StartCause, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::window::{Window, WindowId};

const TILE: usize = 64;
const BG: [u8; 4] = [32, 26, 24, 255]; // BGRA of the panel background (24, 26, 32)

enum UserEvent {
    Repaint(Duration),
}

struct Gfx {
    window: Rc<Window>,
    surface: softbuffer::Surface<Rc<Window>, Rc<Window>>,
    egui_state: egui_winit::State,
    renderer: EguiSoftwareRender,
    frame: Vec<[u8; 4]>,
    prev: Vec<[u8; 4]>,
    size: (usize, usize),
}

struct Runner {
    ctx: egui::Context,
    app: Option<App>,
    initial: Option<std::path::PathBuf>,
    gfx: Option<Gfx>,
    proxy: EventLoopProxy<UserEvent>,
    next_repaint: Option<Instant>,
    last_cpu: Option<f32>,
}

pub fn run(initial: Option<std::path::PathBuf>) -> Result<(), String> {
    let event_loop = EventLoop::<UserEvent>::with_user_event().build().map_err(|e| e.to_string())?;
    let proxy = event_loop.create_proxy();
    let ctx = egui::Context::default();
    // Same system-font fallback eframe uses (symbols/emoji in buttons, CJK names, …).
    ctx.add_font_provider(std::sync::Arc::new(egui_system_font_provider::SystemFontProvider::new()));
    {
        // Wake the event loop when another thread (scan, delete) asks for a repaint.
        let proxy = proxy.clone();
        ctx.set_request_repaint_callback(move |info| {
            let _ = proxy.send_event(UserEvent::Repaint(info.delay));
        });
    }
    let mut runner = Runner { ctx, app: None, initial, gfx: None, proxy, next_repaint: None, last_cpu: None };
    event_loop.run_app(&mut runner).map_err(|e| e.to_string())
}

impl Runner {
    fn redraw(&mut self) {
        let Some(g) = self.gfx.as_mut() else { return };
        let Some(app) = self.app.as_mut() else { return };
        let t0 = Instant::now();
        let raw = g.egui_state.take_egui_input(&g.window);
        let cpu = self.last_cpu;
        let out = self.ctx.run_ui(raw, |ui| app.frame(ui, cpu));
        g.egui_state.handle_platform_output(&g.window, out.platform_output);

        // schedule the next frame
        let delay = out.viewport_output.get(&egui::ViewportId::ROOT).map(|v| v.repaint_delay).unwrap_or(Duration::MAX);
        self.next_repaint = if delay == Duration::ZERO {
            Some(Instant::now())
        } else if delay < Duration::from_secs(3600) {
            Some(Instant::now() + delay)
        } else {
            None
        };
        if let Some(vo) = out.viewport_output.get(&egui::ViewportId::ROOT) {
            for cmd in &vo.commands {
                if let egui::ViewportCommand::Close = cmd {
                    std::process::exit(0);
                }
            }
        }

        let size = g.window.inner_size();
        let (w, h) = (size.width as usize, size.height as usize);
        if w == 0 || h == 0 {
            return;
        }
        let resized = g.size != (w, h);
        if resized {
            g.size = (w, h);
            let _ = g.surface.resize(NonZeroU32::new(w as u32).unwrap(), NonZeroU32::new(h as u32).unwrap());
            g.prev = vec![[0, 0, 0, 0]; w * h];
        }
        let prims = self.ctx.tessellate(out.shapes, out.pixels_per_point);
        g.frame.clear();
        g.frame.resize(w * h, BG);
        {
            let mut buf = BufferMutRef::new(&mut g.frame, w, h);
            g.renderer.render(&mut buf, &prims, &out.textures_delta, out.pixels_per_point);
        }

        // Diff against the previous frame: only changed tiles are presented.
        let mut damage: Vec<softbuffer::Rect> = Vec::new();
        let (tx, ty) = (w.div_ceil(TILE), h.div_ceil(TILE));
        for tyi in 0..ty {
            let y0 = tyi * TILE;
            let y1 = (y0 + TILE).min(h);
            let mut run_start: Option<usize> = None;
            for txi in 0..=tx {
                let dirty = txi < tx && {
                    let x0 = txi * TILE;
                    let x1 = (x0 + TILE).min(w);
                    resized || (y0..y1).any(|y| g.frame[y * w + x0..y * w + x1] != g.prev[y * w + x0..y * w + x1])
                };
                match (dirty, run_start) {
                    (true, None) => run_start = Some(txi),
                    (false, Some(s)) => {
                        let x0 = s * TILE;
                        let x1 = (txi * TILE).min(w);
                        damage.push(softbuffer::Rect {
                            x: x0 as u32,
                            y: y0 as u32,
                            width: NonZeroU32::new((x1 - x0) as u32).unwrap(),
                            height: NonZeroU32::new((y1 - y0) as u32).unwrap(),
                        });
                        run_start = None;
                    }
                    _ => {}
                }
            }
        }
        if !damage.is_empty()
            && let Ok(mut buffer) = g.surface.buffer_mut()
        {
            for (dst, src) in buffer.iter_mut().zip(g.frame.iter()) {
                *dst = u32::from_le_bytes([src[0], src[1], src[2], 0]);
            }
            let _ = buffer.present_with_damage(&damage);
            std::mem::swap(&mut g.frame, &mut g.prev);
        }
        self.last_cpu = Some(t0.elapsed().as_secs_f32());
    }
}

impl ApplicationHandler<UserEvent> for Runner {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.gfx.is_some() {
            return;
        }
        let attrs = Window::default_attributes()
            .with_title("DiskSpacer")
            .with_inner_size(winit::dpi::LogicalSize::new(1480.0, 900.0))
            .with_min_inner_size(winit::dpi::LogicalSize::new(900.0, 560.0));
        let window = Rc::new(event_loop.create_window(attrs).expect("create window"));
        let context = softbuffer::Context::new(window.clone()).expect("softbuffer context");
        let surface = softbuffer::Surface::new(&context, window.clone()).expect("softbuffer surface");
        let egui_state = egui_winit::State::new(
            self.ctx.clone(),
            egui::ViewportId::ROOT,
            event_loop,
            Some(window.scale_factor() as f32),
            None,
            Some(8192),
        );
        self.gfx = Some(Gfx {
            window: window.clone(),
            surface,
            egui_state,
            renderer: EguiSoftwareRender::new(ColorFieldOrder::Bgra),
            frame: Vec::new(),
            prev: Vec::new(),
            size: (0, 0),
        });
        if self.app.is_none() {
            self.app = Some(App::new(&self.ctx, self.initial.take()));
        }
        window.request_redraw();
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        let Some(g) = self.gfx.as_mut() else { return };
        match &event {
            WindowEvent::CloseRequested => {
                event_loop.exit();
                return;
            }
            WindowEvent::RedrawRequested => {
                self.redraw();
                return;
            }
            _ => {}
        }
        let resp = g.egui_state.on_window_event(&g.window, &event);
        if resp.repaint || matches!(event, WindowEvent::Resized(_)) {
            g.window.request_redraw();
        }
    }

    fn user_event(&mut self, _event_loop: &ActiveEventLoop, event: UserEvent) {
        let UserEvent::Repaint(delay) = event;
        let at = Instant::now() + delay;
        if self.next_repaint.is_none_or(|t| at < t) {
            self.next_repaint = Some(at);
        }
        if delay.is_zero()
            && let Some(g) = &self.gfx
        {
            g.window.request_redraw();
        }
    }

    fn new_events(&mut self, _event_loop: &ActiveEventLoop, cause: StartCause) {
        if let StartCause::ResumeTimeReached { .. } = cause
            && let Some(g) = &self.gfx
        {
            g.window.request_redraw();
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        match self.next_repaint {
            Some(t) if t <= Instant::now() => {
                self.next_repaint = None;
                if let Some(g) = &self.gfx {
                    g.window.request_redraw();
                }
                event_loop.set_control_flow(ControlFlow::Wait);
            }
            Some(t) => event_loop.set_control_flow(ControlFlow::WaitUntil(t)),
            None => event_loop.set_control_flow(ControlFlow::Wait),
        }
        let _ = &self.proxy;
    }
}
