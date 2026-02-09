mod app;
mod audio;
mod pixel;
mod platform;
mod render;
mod types;

use std::sync::Arc;

use anyhow::Result;
use app::App;
use egui_winit::State as EguiWinitState;
use winit::application::ApplicationHandler;
use winit::event::{ElementState, KeyEvent, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{Key, NamedKey};
use winit::monitor::VideoModeHandle;
use winit::window::{Window, WindowId};

struct MainState {
    window: Option<Arc<Window>>,
    render: Option<render::RenderState>,
    egui_state: Option<EguiWinitState>,
    egui_renderer: Option<egui_wgpu::Renderer>,
    egui_ctx: egui::Context,
    app: App,
    fullscreen_aspect: Option<bool>,
    fullscreen_exclusive: bool,
    keep_awake: Option<platform::KeepAwake>,
}

impl MainState {
    fn new() -> Result<Self> {
        Ok(Self {
            window: None,
            render: None,
            egui_state: None,
            egui_renderer: None,
            egui_ctx: egui::Context::default(),
            app: App::new()?,
            fullscreen_aspect: None,
            fullscreen_exclusive: false,
            keep_awake: None,
        })
    }

    fn redraw(&mut self, window: &Window) {
        self.apply_fullscreen(window);
        self.apply_keep_awake();
        let Some(render) = self.render.as_mut() else { return };
        let Some(egui_state) = self.egui_state.as_mut() else { return };
        let Some(egui_renderer) = self.egui_renderer.as_mut() else { return };
        let raw_input = egui_state.take_egui_input(window);
        let full_output = self.egui_ctx.run(raw_input, |ctx| self.app.ui(ctx));
        egui_state.handle_platform_output(window, full_output.platform_output);
        if let Some(frame) = self.app.take_latest_frame() {
            render.update_frame(&frame);
        }
        let aspect = self.app.aspect_correction_enabled()
            && (!self.app.is_fullscreen() || !self.fullscreen_exclusive);
        render.set_aspect_correction(aspect);
        let clipped = if full_output.shapes.is_empty() {
            Vec::new()
        } else {
            self.egui_ctx
                .tessellate(full_output.shapes, full_output.pixels_per_point)
        };
        let pixels_per_point = egui_winit::pixels_per_point(&self.egui_ctx, window);
        let _ = render.render(
            window,
            egui_renderer,
            &full_output.textures_delta,
            &clipped,
            pixels_per_point,
        );
    }

    fn update_target_capture_size(&mut self) {
        let Some(window) = self.window.as_ref() else { return };
        let monitor = window.current_monitor();
        let size = monitor.map(|m| m.size());
        self.app
            .set_target_capture_size(size.map(|s| (s.width, s.height)));
    }

    fn apply_fullscreen(&mut self, window: &Window) {
        let aspect = self.app.aspect_correction_enabled();
        if let Some(request) = self.app.take_fullscreen_request() {
            if request {
                if aspect {
                    if let Some(mode) = self.match_capture_mode(window) {
                        window.set_fullscreen(Some(winit::window::Fullscreen::Exclusive(mode)));
                        self.fullscreen_exclusive = true;
                    } else {
                        window.set_fullscreen(Some(winit::window::Fullscreen::Borderless(
                            window.current_monitor(),
                        )));
                        self.fullscreen_exclusive = false;
                    }
                } else {
                    window.set_fullscreen(Some(winit::window::Fullscreen::Borderless(
                        window.current_monitor(),
                    )));
                    self.fullscreen_exclusive = false;
                }
                self.fullscreen_aspect = Some(aspect);
            } else {
                window.set_fullscreen(None);
                self.fullscreen_aspect = None;
                self.fullscreen_exclusive = false;
            }
            self.app.set_fullscreen_state(request);
        } else if self.app.is_fullscreen() && self.fullscreen_aspect != Some(aspect) {
            self.fullscreen_aspect = Some(aspect);
            if aspect {
                if let Some(mode) = self.match_capture_mode(window) {
                    window.set_fullscreen(Some(winit::window::Fullscreen::Exclusive(mode)));
                    self.fullscreen_exclusive = true;
                } else {
                    window.set_fullscreen(Some(winit::window::Fullscreen::Borderless(
                        window.current_monitor(),
                    )));
                    self.fullscreen_exclusive = false;
                }
            } else {
                window.set_fullscreen(Some(winit::window::Fullscreen::Borderless(
                    window.current_monitor(),
                )));
                self.fullscreen_exclusive = false;
            }
        }
    }

    fn apply_keep_awake(&mut self) {
        if let Some(request) = self.app.take_keep_awake_request() {
            if request {
                if self.keep_awake.is_none() {
                    self.keep_awake = platform::KeepAwake::new();
                }
            } else {
                self.keep_awake = None;
            }
        }
    }

    fn match_capture_mode(&self, window: &Window) -> Option<VideoModeHandle> {
        let (w, h) = self.app.capture_size()?;
        let monitor = window.current_monitor()?;
        monitor
            .video_modes()
            .find(|mode| mode.size().width == w && mode.size().height == h)
    }
}

impl ApplicationHandler for MainState {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        event_loop.set_control_flow(ControlFlow::Poll);
        let monitor = event_loop.primary_monitor();
        let window_attrs = if let Some(monitor) = monitor {
            Window::default_attributes()
                .with_title("CaptureCardGaming")
                .with_inner_size(monitor.size())
        } else {
            Window::default_attributes().with_title("CaptureCardGaming")
        };
        let window = match event_loop.create_window(window_attrs) {
            Ok(w) => Arc::new(w),
            Err(e) => {
                eprintln!("{e}");
                event_loop.exit();
                return;
            }
        };
        let render = match pollster::block_on(render::RenderState::new(window.clone())) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("{e}");
                event_loop.exit();
                return;
            }
        };
        let egui_state = EguiWinitState::new(
            self.egui_ctx.clone(),
            egui::ViewportId::ROOT,
            window.as_ref(),
            None,
            window.theme(),
            None,
        );
        let egui_renderer = egui_wgpu::Renderer::new(
            render.device(),
            render.config.format,
            egui_wgpu::RendererOptions::default(),
        );
        self.window = Some(window);
        self.render = Some(render);
        self.egui_state = Some(egui_state);
        self.egui_renderer = Some(egui_renderer);
        self.update_target_capture_size();
        self.apply_keep_awake();
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        _window_id: WindowId,
        event: WindowEvent,
    ) {
        let Some(window) = self.window.as_ref().cloned() else { return };
        let Some(egui_state) = self.egui_state.as_mut() else { return };
        if matches!(event, WindowEvent::CloseRequested) {
            event_loop.exit();
            return;
        }
        if let WindowEvent::CursorMoved { position, .. } = event {
            self.app.set_mouse_y(position.y as f32);
        }
        let response = egui_state.on_window_event(window.as_ref(), &event);
        if response.repaint {
            window.request_redraw();
        }
        match event {
            WindowEvent::KeyboardInput {
                event:
                    KeyEvent {
                        logical_key: Key::Named(NamedKey::F3),
                        state: ElementState::Pressed,
                        ..
                    },
                ..
            } => {
                self.app.toggle_stats();
                window.request_redraw();
            }
            WindowEvent::Resized(size) => {
                if let Some(render) = self.render.as_mut() {
                    render.resize(size);
                }
            }
            WindowEvent::ScaleFactorChanged { .. } => {
                if let Some(render) = self.render.as_mut() {
                    render.resize(window.inner_size());
                }
                self.update_target_capture_size();
            }
            WindowEvent::Moved { .. } => {
                self.update_target_capture_size();
            }
            WindowEvent::RedrawRequested => {
                self.redraw(window.as_ref());
            }
            _ => {}
        }
    }

    fn about_to_wait(&mut self, _event_loop: &ActiveEventLoop) {
        if let Some(window) = self.window.as_ref() {
            window.request_redraw();
        }
    }
}

fn main() -> Result<()> {
    let event_loop = EventLoop::new()?;
    let mut state = MainState::new()?;
    event_loop.run_app(&mut state)?;
    Ok(())
}
