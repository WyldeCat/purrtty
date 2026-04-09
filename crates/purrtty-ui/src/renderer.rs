//! wgpu + glyphon renderer.
//!
//! Single `cosmic_text::Buffer` covering the whole grid (one BufferLine
//! per row), per-line `set_text` for dirty rows only, and per-glyph
//! foreground colors via `AttrsList`. Backgrounds, reverse video, and
//! the cursor are drawn as solid wgpu quads on either side of the text
//! pass via `QuadRenderer`.
//!
//! See `docs/perf.md` for the design rationale.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use glyphon::cosmic_text::{Attrs, AttrsList, BufferLine, LineEnding};
use glyphon::{
    Buffer, Cache, Color as GlyphColor, Family, FontSystem, Metrics, Resolution, Shaping,
    SwashCache, TextArea, TextAtlas, TextBounds, TextRenderer, Viewport, Wrap,
};
use purrtty_term::cell::Color as TermColor;
use purrtty_term::grid::WIDE_CONT;
use purrtty_term::{Attrs as CellAttrs, Cell, Grid};
use wgpu::{
    CompositeAlphaMode, DeviceDescriptor, Instance, InstanceDescriptor, LoadOp, MultisampleState,
    Operations, PresentMode, RenderPassColorAttachment, RenderPassDescriptor,
    RequestAdapterOptions, StoreOp, SurfaceConfiguration, TextureUsages, TextureViewDescriptor,
};
use winit::{dpi::PhysicalSize, window::Window};

use crate::quad::{QuadRenderer, QuadVertex};

const FONT_SIZE: f32 = 18.0;
const LINE_HEIGHT: f32 = 22.0;
const PAD_X: f32 = 16.0;
const PAD_Y: f32 = 16.0;

const DEFAULT_FG: GlyphColor = GlyphColor::rgb(220, 220, 220);
/// Surface clear color, also used as the default background when reverse
/// video needs an explicit color to swap into the foreground.
const SURFACE_CLEAR: wgpu::Color = wgpu::Color {
    r: 0.05,
    g: 0.05,
    b: 0.08,
    a: 1.0,
};
const DEFAULT_BG_RGBA: [f32; 4] = [
    SURFACE_CLEAR.r as f32,
    SURFACE_CLEAR.g as f32,
    SURFACE_CLEAR.b as f32,
    1.0,
];

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

    quads: QuadRenderer,
    /// Cell advance width measured from the active monospace font once
    /// at startup. Used for both grid sizing and quad placement.
    cell_width: f32,

    /// Single buffer for the entire grid; per-line updates via
    /// `buffer.lines[i].set_text`.
    buffer: Buffer,
    row_hashes: Vec<u64>,
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

        let mut font_system = FontSystem::new();
        let cell_width = measure_cell_width(&mut font_system);
        tracing::debug!(cell_width, "measured monospace cell width");

        let swash_cache = SwashCache::new();
        let cache = Cache::new(&device);
        let viewport = Viewport::new(&device, &cache);
        let mut atlas = TextAtlas::new(&device, &queue, &cache, format);
        let text_renderer =
            TextRenderer::new(&mut atlas, &device, MultisampleState::default(), None);

        let quads = QuadRenderer::new(&device, format)?;

        let mut buffer = Buffer::new(&mut font_system, Metrics::new(FONT_SIZE, LINE_HEIGHT));
        buffer.set_wrap(&mut font_system, Wrap::None);
        buffer.set_size(
            &mut font_system,
            Some(width as f32),
            Some(height as f32),
        );

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
            quads,
            cell_width,
            buffer,
            row_hashes: Vec::new(),
            last_grid_rows: 0,
            last_grid_cols: 0,
        })
    }

    /// Terminal grid dimensions, in cells, that fit the current surface.
    pub fn grid_dimensions(&self) -> (u16, u16) {
        let w = (self.config.width as f32 - 2.0 * PAD_X).max(0.0);
        let h = (self.config.height as f32 - 2.0 * PAD_Y).max(0.0);
        let cols = (w / self.cell_width).floor().max(1.0) as u16;
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
        self.buffer.set_size(
            &mut self.font_system,
            Some(size.width as f32),
            Some(size.height as f32),
        );
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
        let cell_w = self.cell_width;
        let pad_x = PAD_X;
        let pad_y = PAD_Y;

        // Reset on dimension change.
        if rows != self.last_grid_rows || cols != self.last_grid_cols {
            self.buffer.lines.clear();
            for _ in 0..rows {
                self.buffer.lines.push(BufferLine::new(
                    "",
                    LineEnding::default(),
                    AttrsList::new(default_attrs()),
                    Shaping::Advanced,
                ));
            }
            self.row_hashes.clear();
            self.row_hashes.resize(rows, u64::MAX);
            self.last_grid_rows = rows;
            self.last_grid_cols = cols;
        }

        // ---- text: per-line set_text on dirty rows ----
        for view_idx in 0..rows {
            let row = grid.row_at(view_idx, scroll_offset).unwrap_or(&[]);
            let hash = row_hash(row);
            if self.row_hashes[view_idx] == hash {
                continue;
            }
            let (text, attrs_list) = build_line(row);
            self.buffer.lines[view_idx].set_text(text, LineEnding::default(), attrs_list);
            self.row_hashes[view_idx] = hash;
        }
        self.buffer
            .shape_until_scroll(&mut self.font_system, false);

        // ---- background quads ----
        let mut bg_verts: Vec<QuadVertex> = Vec::new();
        for view_idx in 0..rows {
            let row = grid.row_at(view_idx, scroll_offset).unwrap_or(&[]);
            for (col_idx, cell) in row.iter().enumerate() {
                if col_idx >= cols {
                    break;
                }
                if cell.ch == WIDE_CONT {
                    continue;
                }
                let (_fg, bg_opt) = cell_colors(cell);
                let Some(bg) = bg_opt else { continue };
                let next_is_cont = col_idx + 1 < cols
                    && row
                        .get(col_idx + 1)
                        .map(|c| c.ch == WIDE_CONT)
                        .unwrap_or(false);
                let w = if next_is_cont { 2.0 * cell_w } else { cell_w };
                let x = pad_x + col_idx as f32 * cell_w;
                let y = pad_y + view_idx as f32 * LINE_HEIGHT;
                QuadRenderer::push_rect(&mut bg_verts, x, y, w, LINE_HEIGHT, bg);
            }
        }

        // ---- cursor quad (overlay) ----
        let mut overlay_verts: Vec<QuadVertex> = Vec::new();
        if grid.cursor_visible() && scroll_offset == 0 {
            let cursor = grid.cursor();
            if cursor.row < rows && cols > 0 {
                let col = cursor.col.min(cols - 1);
                let x = pad_x + col as f32 * cell_w;
                let y = pad_y + cursor.row as f32 * LINE_HEIGHT;
                // Hollow / alpha-blended block — text remains visible.
                QuadRenderer::push_rect(
                    &mut overlay_verts,
                    x,
                    y,
                    cell_w,
                    LINE_HEIGHT,
                    [0.85, 0.85, 0.85, 0.4],
                );
            }
        }

        self.quads
            .update_resolution(&self.queue, self.config.width, self.config.height);
        self.quads
            .upload_bg(&self.device, &self.queue, &bg_verts);
        self.quads
            .upload_overlay(&self.device, &self.queue, &overlay_verts);

        // ---- glyphon prepare ----
        let bounds = TextBounds {
            left: 0,
            top: 0,
            right: self.config.width as i32,
            bottom: self.config.height as i32,
        };
        let text_area = TextArea {
            buffer: &self.buffer,
            left: pad_x,
            top: pad_y,
            scale: 1.0,
            bounds,
            default_color: DEFAULT_FG,
            custom_glyphs: &[],
        };

        self.text_renderer
            .prepare(
                &self.device,
                &self.queue,
                &mut self.font_system,
                &mut self.atlas,
                &self.viewport,
                [text_area],
                &mut self.swash_cache,
            )
            .context("glyphon prepare")?;

        // ---- render pass: clear → bg quads → text → overlay quads ----
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
                        load: LoadOp::Clear(SURFACE_CLEAR),
                        store: StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });

            self.quads.render_bg(&mut pass);
            self.text_renderer
                .render(&self.atlas, &self.viewport, &mut pass)
                .context("glyphon render")?;
            self.quads.render_overlay(&mut pass);
        }

        self.queue.submit(Some(encoder.finish()));
        frame.present();
        self.atlas.trim();
        let _ = &self.window;
        Ok(())
    }
}

fn default_attrs() -> Attrs<'static> {
    Attrs::new().family(Family::Monospace)
}

/// Measure the advance width of an `M` glyph in the active monospace
/// font. Falls back to a font-size estimate if shaping returns nothing.
fn measure_cell_width(font_system: &mut FontSystem) -> f32 {
    let mut buf = Buffer::new(font_system, Metrics::new(FONT_SIZE, LINE_HEIGHT));
    buf.set_wrap(font_system, Wrap::None);
    buf.set_size(font_system, Some(2000.0), Some(LINE_HEIGHT * 4.0));
    buf.set_text(
        font_system,
        "MMMMMMMMMM",
        Attrs::new().family(Family::Monospace),
        Shaping::Advanced,
    );
    buf.shape_until_scroll(font_system, false);

    let mut max_x: f32 = 0.0;
    if let Some(run) = buf.layout_runs().next() {
        for glyph in run.glyphs.iter() {
            let right = glyph.x + glyph.w;
            if right > max_x {
                max_x = right;
            }
        }
        if max_x > 0.0 {
            return max_x / 10.0;
        }
    }
    FONT_SIZE * 0.6
}

/// Build the cosmic-text line text and the matching `AttrsList` from a
/// grid row. Cells with identical effective fg/attrs (after reverse-video
/// resolution) are compacted into one span.
fn build_line(row: &[Cell]) -> (String, AttrsList) {
    let mut attrs_list = AttrsList::new(default_attrs());
    let mut text = String::with_capacity(row.len());

    let mut run_start: Option<usize> = None;
    let mut run_fg = DEFAULT_FG;
    let mut run_attrs = CellAttrs::empty();

    for cell in row {
        if cell.ch == WIDE_CONT {
            continue;
        }
        let (fg, _bg) = cell_colors(cell);
        // Drop REVERSE from the per-glyph attrs since we already swapped
        // colors and bold/italic still apply.
        let effective_attrs = cell.attrs & !CellAttrs::REVERSE;
        let started_new_run = match run_start {
            Some(_) => {
                fg.r() != run_fg.r()
                    || fg.g() != run_fg.g()
                    || fg.b() != run_fg.b()
                    || fg.a() != run_fg.a()
                    || effective_attrs != run_attrs
            }
            None => true,
        };
        if started_new_run {
            if let Some(start) = run_start {
                if start < text.len() {
                    attrs_list.add_span(start..text.len(), make_attrs(run_fg, run_attrs));
                }
            }
            run_start = Some(text.len());
            run_fg = fg;
            run_attrs = effective_attrs;
        }
        text.push(cell.ch);
    }
    if let Some(start) = run_start {
        if start < text.len() {
            attrs_list.add_span(start..text.len(), make_attrs(run_fg, run_attrs));
        }
    }

    (text, attrs_list)
}

/// Return the foreground glyph color and (optionally) a background quad
/// color for a cell, applying reverse-video swap.
fn cell_colors(cell: &Cell) -> (GlyphColor, Option<[f32; 4]>) {
    let fg = resolve_color(cell.fg, DEFAULT_FG);
    let bg_opt = match cell.bg {
        TermColor::Default => None,
        other => Some(resolve_color(other, DEFAULT_FG)),
    };

    if cell.attrs.contains(CellAttrs::REVERSE) {
        // Swap. A "default" background becomes the surface clear color.
        let new_fg_glyph = bg_opt.unwrap_or_else(|| {
            GlyphColor::rgb(
                (DEFAULT_BG_RGBA[0] * 255.0) as u8,
                (DEFAULT_BG_RGBA[1] * 255.0) as u8,
                (DEFAULT_BG_RGBA[2] * 255.0) as u8,
            )
        });
        let new_bg_rgba = glyph_to_rgba(fg);
        return (new_fg_glyph, Some(new_bg_rgba));
    }

    (fg, bg_opt.map(glyph_to_rgba))
}

fn make_attrs(fg: GlyphColor, attrs: CellAttrs) -> Attrs<'static> {
    use glyphon::cosmic_text::{Style, Weight};
    let mut a = default_attrs().color(fg);
    if attrs.contains(CellAttrs::BOLD) {
        a = a.weight(Weight::BOLD);
    }
    if attrs.contains(CellAttrs::ITALIC) {
        a = a.style(Style::Italic);
    }
    a
}

fn resolve_color(c: TermColor, default: GlyphColor) -> GlyphColor {
    match c {
        TermColor::Default => default,
        TermColor::Indexed(i) => indexed_color(i),
        TermColor::Rgb(r, g, b) => GlyphColor::rgb(r, g, b),
    }
}

fn glyph_to_rgba(c: GlyphColor) -> [f32; 4] {
    [
        c.r() as f32 / 255.0,
        c.g() as f32 / 255.0,
        c.b() as f32 / 255.0,
        c.a() as f32 / 255.0,
    ]
}

/// 16-color ANSI palette + xterm 256-color cube + grayscale ramp.
fn indexed_color(i: u8) -> GlyphColor {
    const ANSI_16: [(u8, u8, u8); 16] = [
        (0, 0, 0),
        (205, 49, 49),
        (13, 188, 121),
        (229, 229, 16),
        (36, 114, 200),
        (188, 63, 188),
        (17, 168, 205),
        (229, 229, 229),
        (102, 102, 102),
        (241, 76, 76),
        (35, 209, 139),
        (245, 245, 67),
        (59, 142, 234),
        (214, 112, 214),
        (41, 184, 219),
        (255, 255, 255),
    ];
    if (i as usize) < ANSI_16.len() {
        let (r, g, b) = ANSI_16[i as usize];
        return GlyphColor::rgb(r, g, b);
    }
    if i >= 16 && i <= 231 {
        let n = i - 16;
        let r = n / 36;
        let g = (n % 36) / 6;
        let b = n % 6;
        let lvl = |c: u8| -> u8 {
            if c == 0 {
                0
            } else {
                55 + c * 40
            }
        };
        return GlyphColor::rgb(lvl(r), lvl(g), lvl(b));
    }
    let g = 8 + (i - 232) * 10;
    GlyphColor::rgb(g, g, g)
}

fn row_hash(row: &[Cell]) -> u64 {
    let mut h = DefaultHasher::new();
    for cell in row {
        cell.hash(&mut h);
    }
    h.finish()
}
