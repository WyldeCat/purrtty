//! purtty — binary entry point.
//!
//! M0: open an empty winit window. Closing the window exits the app.
//! Real rendering arrives in M1, PTY in M3, full integration in M4.

#![forbid(unsafe_code)]

use anyhow::Result;
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;
use winit::application::ApplicationHandler;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::window::{Window, WindowId};

/// Top-level application state held across the winit event loop.
#[derive(Default)]
struct PurttyApp {
    window: Option<Window>,
}

impl ApplicationHandler for PurttyApp {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let attrs = Window::default_attributes()
            .with_title("purtty")
            .with_inner_size(winit::dpi::LogicalSize::new(960.0, 600.0));
        match event_loop.create_window(attrs) {
            Ok(window) => {
                info!(
                    size = ?window.inner_size(),
                    scale_factor = window.scale_factor(),
                    "window created"
                );
                self.window = Some(window);
            }
            Err(err) => {
                warn!(?err, "failed to create window");
                event_loop.exit();
            }
        }
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        _window_id: WindowId,
        event: WindowEvent,
    ) {
        match event {
            WindowEvent::CloseRequested => {
                info!("close requested, exiting");
                event_loop.exit();
            }
            WindowEvent::Resized(size) => {
                info!(?size, "resized");
            }
            WindowEvent::RedrawRequested => {
                // Nothing to draw yet. GPU renderer lands in M1.
            }
            _ => {}
        }
    }
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,purtty=debug"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .init();
}

fn main() -> Result<()> {
    init_tracing();
    info!(version = env!("CARGO_PKG_VERSION"), "starting purtty");

    let event_loop = EventLoop::new()?;
    event_loop.set_control_flow(ControlFlow::Wait);

    let mut app = PurttyApp::default();
    event_loop.run_app(&mut app)?;
    Ok(())
}
