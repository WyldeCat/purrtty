//! purtty-ui — GPU rendering and input handling.
//!
//! Owns the wgpu device/surface, renders a `purtty_term::Grid` via
//! `glyphon`, and translates keyboard/mouse events into PTY bytes.
//!
//! M0: stubs only. Real renderer lands in M1.

#![forbid(unsafe_code)]

/// Placeholder renderer. Will own wgpu state in M1.
#[derive(Debug, Default)]
pub struct Renderer {
    _private: (),
}

impl Renderer {
    pub fn new() -> Self {
        Self::default()
    }
}
