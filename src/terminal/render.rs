//! Glyph atlas → framebuffer blitter.
//!
//! Consumes a `Term` snapshot plus a `GlyphAtlas` and produces an RGBA buffer
//! suitable for display in a Slint `Image` element. The full-frame redraw cost
//! for an 80×24 grid at 16 px is well under a millisecond on a modern laptop,
//! so for MVP we do not bother with per-cell damage tracking yet.

use alacritty_terminal::event::EventListener;
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::Term;
use alacritty_terminal::term::cell::{Cell, Flags};
use alacritty_terminal::vte::ansi::{Color, NamedColor};
use slint::{Rgba8Pixel, SharedPixelBuffer};

use super::font::{Glyph, GlyphAtlas};

/// Default background — matches the window chrome for a seamless terminal pane.
pub const DEFAULT_BG: [u8; 4] = [0x0e, 0x0f, 0x13, 0xff];
/// Default foreground — soft off-white for body text.
pub const DEFAULT_FG: [u8; 4] = [0xd4, 0xd4, 0xd4, 0xff];

/// Owning handle for the pixel buffer that backs the terminal's `Image`.
pub struct Framebuffer {
    pub buffer: SharedPixelBuffer<Rgba8Pixel>,
    pub cols: usize,
    pub rows: usize,
    pub cell_w: usize,
    pub cell_h: usize,
    pub baseline: usize,
}

impl Framebuffer {
    pub fn new(cols: usize, rows: usize, atlas: &GlyphAtlas) -> Self {
        let w = (cols * atlas.cell_w) as u32;
        let h = (rows * atlas.cell_h) as u32;
        Self {
            buffer: SharedPixelBuffer::<Rgba8Pixel>::new(w, h),
            cols,
            rows,
            cell_w: atlas.cell_w,
            cell_h: atlas.cell_h,
            baseline: atlas.baseline,
        }
    }

    #[inline]
    pub fn width(&self) -> usize {
        self.cols * self.cell_w
    }

    #[inline]
    pub fn height(&self) -> usize {
        self.rows * self.cell_h
    }

    /// Redraw the entire framebuffer from the current `Term` state.
    pub fn blit_from_term<T: EventListener>(
        &mut self,
        term: &Term<T>,
        atlas: &GlyphAtlas,
    ) {
        let fb_w = self.width();
        let fb_h = self.height();
        let cell_w = self.cell_w;
        let cell_h = self.cell_h;
        let cols = self.cols;
        let rows = self.rows;
        let baseline = self.baseline;
        let bytes = self.buffer.make_mut_bytes();

        // Clear the whole buffer to the default background.
        for px in bytes.chunks_exact_mut(4) {
            px.copy_from_slice(&DEFAULT_BG);
        }

        let grid = term.grid();

        for row in 0..rows {
            let line = Line(row as i32);
            for col in 0..cols {
                let cell = &grid[line][Column(col)];
                let (fg, bg) = resolve_colors(cell);

                if bg != DEFAULT_BG {
                    fill_rect(
                        bytes, fb_w,
                        col * cell_w, row * cell_h,
                        cell_w, cell_h,
                        bg,
                    );
                }

                if cell.c != ' '
                    && cell.c != '\0'
                    && let Some(glyph) = atlas.glyph(cell.c)
                {
                    draw_glyph(
                        bytes, fb_w, fb_h,
                        col * cell_w, row * cell_h,
                        baseline,
                        &glyph,
                        fg,
                    );
                }
            }
        }

        // Cursor overlay: solid block inverted against the cell.
        let cursor = grid.cursor.point;
        let cursor_col = cursor.column.0;
        let cursor_row = cursor.line.0;
        if cursor_row >= 0
            && (cursor_row as usize) < rows
            && cursor_col < cols
        {
            let cr = cursor_row as usize;
            fill_rect(
                bytes, fb_w,
                cursor_col * cell_w, cr * cell_h,
                cell_w, cell_h,
                [0xe6, 0xe7, 0xea, 0xff],
            );
            let cell = &grid[Line(cr as i32)][Column(cursor_col)];
            if cell.c != ' '
                && cell.c != '\0'
                && let Some(glyph) = atlas.glyph(cell.c)
            {
                draw_glyph(
                    bytes, fb_w, fb_h,
                    cursor_col * cell_w, cr * cell_h,
                    baseline,
                    &glyph,
                    DEFAULT_BG,
                );
            }
        }
    }
}

#[inline]
fn fill_rect(
    bytes: &mut [u8],
    fb_w: usize,
    x: usize,
    y: usize,
    w: usize,
    h: usize,
    color: [u8; 4],
) {
    for dy in 0..h {
        let row_start = ((y + dy) * fb_w + x) * 4;
        for dx in 0..w {
            let idx = row_start + dx * 4;
            bytes[idx..idx + 4].copy_from_slice(&color);
        }
    }
}

/// Blend a rasterized glyph's alpha mask into the framebuffer, over whatever
/// background has already been drawn.
#[allow(clippy::too_many_arguments)]
fn draw_glyph(
    bytes: &mut [u8],
    fb_w: usize,
    fb_h: usize,
    cell_x: usize,
    cell_y: usize,
    baseline: usize,
    glyph: &Glyph,
    fg: [u8; 4],
) {
    let gx = cell_x as i32 + glyph.xmin;
    let gy = cell_y as i32 + baseline as i32 - glyph.ymin - glyph.height as i32;

    for row in 0..glyph.height {
        for col in 0..glyph.width {
            let alpha = glyph.bitmap[row * glyph.width + col];
            if alpha == 0 {
                continue;
            }

            let px = gx + col as i32;
            let py = gy + row as i32;
            if px < 0
                || py < 0
                || (px as usize) >= fb_w
                || (py as usize) >= fb_h
            {
                continue;
            }

            let idx = ((py as usize) * fb_w + px as usize) * 4;
            let bg_r = bytes[idx] as u16;
            let bg_g = bytes[idx + 1] as u16;
            let bg_b = bytes[idx + 2] as u16;
            let fg_r = fg[0] as u16;
            let fg_g = fg[1] as u16;
            let fg_b = fg[2] as u16;
            let a = alpha as u16;
            let inv_a = 255 - a;
            bytes[idx] = ((fg_r * a + bg_r * inv_a) / 255) as u8;
            bytes[idx + 1] = ((fg_g * a + bg_g * inv_a) / 255) as u8;
            bytes[idx + 2] = ((fg_b * a + bg_b * inv_a) / 255) as u8;
            bytes[idx + 3] = 0xff;
        }
    }
}

fn resolve_colors(cell: &Cell) -> ([u8; 4], [u8; 4]) {
    let inverse = cell.flags.contains(Flags::INVERSE);
    let fg = color_to_rgba(cell.fg);
    let bg = color_to_rgba(cell.bg);
    if inverse { (bg, fg) } else { (fg, bg) }
}

fn color_to_rgba(color: Color) -> [u8; 4] {
    match color {
        Color::Named(named) => named_to_rgba(named),
        Color::Spec(rgb) => [rgb.r, rgb.g, rgb.b, 0xff],
        Color::Indexed(idx) => {
            if idx < 16 {
                named_to_rgba(named_from_ansi_index(idx))
            } else {
                approximate_256(idx)
            }
        }
    }
}

/// VS Code-ish dark palette. Picked for good legibility on the dark chrome.
fn named_to_rgba(c: NamedColor) -> [u8; 4] {
    use NamedColor::*;
    match c {
        Foreground | BrightForeground => DEFAULT_FG,
        Background => DEFAULT_BG,
        Cursor => [0xe6, 0xe7, 0xea, 0xff],
        Black         => [0x1e, 0x1e, 0x1e, 0xff],
        Red           => [0xf1, 0x4c, 0x4c, 0xff],
        Green         => [0x23, 0xd1, 0x8b, 0xff],
        Yellow        => [0xf5, 0xf5, 0x43, 0xff],
        Blue          => [0x3b, 0x8e, 0xea, 0xff],
        Magenta       => [0xd6, 0x70, 0xd6, 0xff],
        Cyan          => [0x29, 0xb8, 0xdb, 0xff],
        White         => [0xe5, 0xe5, 0xe5, 0xff],
        BrightBlack   => [0x66, 0x66, 0x66, 0xff],
        BrightRed     => [0xff, 0x6b, 0x6b, 0xff],
        BrightGreen   => [0x44, 0xe0, 0xa0, 0xff],
        BrightYellow  => [0xff, 0xff, 0x66, 0xff],
        BrightBlue    => [0x6e, 0xbb, 0xfc, 0xff],
        BrightMagenta => [0xff, 0x85, 0xff, 0xff],
        BrightCyan    => [0x4a, 0xda, 0xf2, 0xff],
        BrightWhite   => [0xff, 0xff, 0xff, 0xff],
        DimBlack      => [0x14, 0x14, 0x14, 0xff],
        DimRed        => [0xa0, 0x30, 0x30, 0xff],
        DimGreen      => [0x17, 0x8a, 0x5c, 0xff],
        DimYellow     => [0xa0, 0xa0, 0x2c, 0xff],
        DimBlue       => [0x27, 0x5e, 0x9c, 0xff],
        DimMagenta    => [0x8c, 0x49, 0x8c, 0xff],
        DimCyan       => [0x1b, 0x7a, 0x91, 0xff],
        DimWhite      => [0x96, 0x96, 0x96, 0xff],
        DimForeground => [0x96, 0x96, 0x96, 0xff],
    }
}

fn named_from_ansi_index(idx: u8) -> NamedColor {
    use NamedColor::*;
    match idx {
        0 => Black,
        1 => Red,
        2 => Green,
        3 => Yellow,
        4 => Blue,
        5 => Magenta,
        6 => Cyan,
        7 => White,
        8 => BrightBlack,
        9 => BrightRed,
        10 => BrightGreen,
        11 => BrightYellow,
        12 => BrightBlue,
        13 => BrightMagenta,
        14 => BrightCyan,
        15 => BrightWhite,
        _ => Foreground,
    }
}

/// Classic xterm 256-colour palette approximation (6×6×6 cube + grayscale).
fn approximate_256(idx: u8) -> [u8; 4] {
    if (16..232).contains(&idx) {
        let n = idx - 16;
        let r = n / 36;
        let g = (n / 6) % 6;
        let b = n % 6;
        let to_rgb = |x: u8| -> u8 {
            if x == 0 { 0 } else { 55 + x * 40 }
        };
        [to_rgb(r), to_rgb(g), to_rgb(b), 0xff]
    } else if idx >= 232 {
        let n = idx - 232;
        let v = 8u16 + (n as u16) * 10;
        let v = v.min(255) as u8;
        [v, v, v, 0xff]
    } else {
        DEFAULT_FG
    }
}
