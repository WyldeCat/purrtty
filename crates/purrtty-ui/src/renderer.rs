//! wgpu + glyphon renderer.
//!
//! Renders a [`Grid`] as text on a dark background. The grid is split
//! into one cosmic-text `Buffer` per row; a frame-level dirty check
//! re-shapes only the rows whose content actually changed since the
//! previous frame. Colors and cursor are still pending (later M4 stages).

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use glyphon::{
    Attrs, Buffer, Cache, Color as GlyphColor, Family, FontSystem, Metrics, Resolution, Shaping,
    SwashCache, TextArea, TextAtlas, TextBounds, TextRenderer, Viewport, Wrap,
};
use purrtty_term::grid::WIDE_CONT;
use purrtty_term::{Cell, Grid};
use wgpu::{
    CompositeAlphaMode, DeviceDescriptor, Instance, InstanceDescriptor, LoadOp, MultisampleState,
    Operations, PresentMode, RenderPassColorAttachment, RenderPassDescriptor,
    RequestAdapterOptions, StoreOp, SurfaceConfiguration, TextureUsages, TextureViewDescriptor,
};
use winit::{dpi::PhysicalSize, window::Window};

/// Font size in physical pixels.
const FONT_SIZE: f32 = 18.0;
/// Line height in physical pixels (font size * ~1.22).
const LINE_HEIGHT: f32 = 22.0;
/// Approximate monospace advance width in physical pixels. Slightly over a
/// half em for most monospace fonts at 18px. Refined by measurement in a
/// later stage.
const CELL_WIDTH: f32 = 10.0;
/// Inner window padding (physical pixels).
const PAD_X: f32 = 16.0;
const PAD_Y: f32 = 16.0;

/// Owns wgpu + glyphon state tied to a single window/surface.
pub struct Renderer {
    window: Arc<Window>,
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: SurfaceConfiguration,

    font_system: FontSystem,
    swash_cache: SwashCache,
    viewport: Viewport,
    atlas: TextAtlas,
    text_renderer: TextRenderer,

    /// One glyphon buffer per visible row. Rebuilt on resize.
    row_buffers: Vec<Buffer>,
    /// Cached content hash per row; a row is re-shaped only when its hash
    /// changes between frames.
    row_hashes: Vec<u64>,
    /// Cached number of grid rows the `row_buffers` were sized for. Used to
    /// detect a grid-dimension change between renders.
    last_grid_rows: usize,
    last_grid_cols: usize,
}

impl Renderer {
    pub fn new(window: Arc<Window>) -> Result<Self> {
        let size = window.inner_size();
        let width = size.width.max(1);
        let height = size.height.max(1);

        let instance = Instance::new(InstanceDescriptor::default());
        let surface = instance
            .create_surface(window.clone())
            .context("create wgpu surface")?;

        let adapter = pollster::block_on(instance.request_adapter(&RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            force_fallback_adapter: false,
            compatible_surface: Some(&surface),
        }))
        .ok_or_else(|| anyhow!("no suitable wgpu adapter found"))?;

        let (device, queue) =
            pollster::block_on(adapter.request_device(&DeviceDescriptor::default(), None))
                .context("request wgpu device")?;

        let caps = surface.get_capabilities(&adapter);
        let format = caps
            .formats
            .iter()
            .copied()
            .find(|f| f.is_srgb())
            .unwrap_or(caps.formats[0]);

        let config = SurfaceConfiguration {
            usage: TextureUsages::RENDER_ATTACHMENT,
            format,
            width,
            height,
            present_mode: PresentMode::Fifo,
            alpha_mode: CompositeAlphaMode::Auto,
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
        };
        surface.configure(&device, &config);

        let font_system = FontSystem::new();
        let swash_cache = SwashCache::new();
        let cache = Cache::new(&device);
        let viewport = Viewport::new(&device, &cache);
        let mut atlas = TextAtlas::new(&device, &queue, &cache, format);
        let text_renderer =
            TextRenderer::new(&mut atlas, &device, MultisampleState::default(), None);

        Ok(Self {
            window,
            surface,
            device,
            queue,
            config,
            font_system,
            swash_cache,
            viewport,
            atlas,
            text_renderer,
            row_buffers: Vec::new(),
            row_hashes: Vec::new(),
            last_grid_rows: 0,
            last_grid_cols: 0,
        })
    }

    /// Terminal grid dimensions, in cells, that fit the current surface.
    pub fn grid_dimensions(&self) -> (u16, u16) {
        let w = (self.config.width as f32 - 2.0 * PAD_X).max(0.0);
        let h = (self.config.height as f32 - 2.0 * PAD_Y).max(0.0);
        let cols = (w / CELL_WIDTH).floor().max(1.0) as u16;
        let rows = (h / LINE_HEIGHT).floor().max(1.0) as u16;
        (rows, cols)
    }

    pub fn resize(&mut self, size: PhysicalSize<u32>) {
        if size.width == 0 || size.height == 0 {
            return;
        }
        self.config.width = size.width;
        self.config.height = size.height;
        self.surface.configure(&self.device, &self.config);
        // Row buffers are rebuilt lazily in render() when the grid
        // dimensions actually change.
    }

    pub fn render(&mut self, grid: &Grid, scroll_offset: usize) -> Result<()> {
        self.viewport.update(
            &self.queue,
            Resolution {
                width: self.config.width,
                height: self.config.height,
            },
        );

        let rows = grid.rows();
        let cols = grid.cols();

        // Rebuild row buffers if the grid dimensions changed.
        if rows != self.last_grid_rows || cols != self.last_grid_cols {
            self.row_buffers.clear();
            self.row_hashes.clear();
            self.row_buffers.reserve(rows);
            self.row_hashes.reserve(rows);
            for _ in 0..rows {
                let mut buf = Buffer::new(
                    &mut self.font_system,
                    Metrics::new(FONT_SIZE, LINE_HEIGHT),
                );
                buf.set_wrap(&mut self.font_system, Wrap::None);
                // One row's worth of horizontal space; vertical space just
                // needs to be at least one line high.
                buf.set_size(
                    &mut self.font_system,
                    Some(cols as f32 * CELL_WIDTH * 2.0),
                    Some(LINE_HEIGHT * 2.0),
                );
                self.row_buffers.push(buf);
                // Sentinel hash that guarantees first-frame update.
                self.row_hashes.push(u64::MAX);
            }
            self.last_grid_rows = rows;
            self.last_grid_cols = cols;
        }

        // Update dirty rows: for each visible row, compute its content hash
        // and re-shape its buffer only when the hash changes.
        for view_idx in 0..rows {
            let row = grid.row_at(view_idx, scroll_offset).unwrap_or(&[]);
            let hash = row_hash(row);
            if self.row_hashes[view_idx] != hash {
                let text: String = row
                    .iter()
                    .filter(|c| c.ch != WIDE_CONT)
                    .map(|c| c.ch)
                    .collect();
                let buffer = &mut self.row_buffers[view_idx];
                buffer.set_text(
                    &mut self.font_system,
                    &text,
                    Attrs::new().family(Family::Monospace),
                    Shaping::Advanced,
                );
                buffer.shape_until_scroll(&mut self.font_system, false);
                self.row_hashes[view_idx] = hash;
            }
        }

        // Collect a TextArea per row. Rows are positioned manually at
        // `(PAD_X, PAD_Y + row * LINE_HEIGHT)` so rows are exactly aligned
        // regardless of what cosmic-text does inside a single row.
        let default_color = GlyphColor::rgb(220, 220, 220);
        let bounds = TextBounds {
            left: 0,
            top: 0,
            right: self.config.width as i32,
            bottom: self.config.height as i32,
        };
        let text_areas: Vec<TextArea> = self
            .row_buffers
            .iter()
            .enumerate()
            .map(|(row_idx, buffer)| TextArea {
                buffer,
                left: PAD_X,
                top: PAD_Y + row_idx as f32 * LINE_HEIGHT,
                scale: 1.0,
                bounds,
                default_color,
                custom_glyphs: &[],
            })
            .collect();

        self.text_renderer
            .prepare(
                &self.device,
                &self.queue,
                &mut self.font_system,
                &mut self.atlas,
                &self.viewport,
                text_areas,
                &mut self.swash_cache,
            )
            .context("glyphon prepare")?;

        let frame = self
            .surface
            .get_current_texture()
            .context("acquire surface texture")?;
        let view = frame.texture.create_view(&TextureViewDescriptor::default());
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("purrtty.encoder"),
            });

        {
            let mut pass = encoder.begin_render_pass(&RenderPassDescriptor {
                label: Some("purrtty.main"),
                color_attachments: &[Some(RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    ops: Operations {
                        load: LoadOp::Clear(wgpu::Color {
                            r: 0.05,
                            g: 0.05,
                            b: 0.08,
                            a: 1.0,
                        }),
                        store: StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });

            self.text_renderer
                .render(&self.atlas, &self.viewport, &mut pass)
                .context("glyphon render")?;
        }

        self.queue.submit(Some(encoder.finish()));
        frame.present();
        self.atlas.trim();
        let _ = &self.window;
        Ok(())
    }
}

/// Content hash for one grid row, including every cell's char/fg/bg/attrs.
fn row_hash(row: &[Cell]) -> u64 {
    let mut h = DefaultHasher::new();
    for cell in row {
        cell.hash(&mut h);
    }
    h.finish()
}
