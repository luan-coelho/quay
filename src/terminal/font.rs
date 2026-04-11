//! Monospace glyph atlas built on top of `fontdue`.
//!
//! Loads JetBrains Mono Regular (bundled via `include_bytes!`) once at startup,
//! computes the cell metrics and pre-rasterizes the printable ASCII range. Non-
//! ASCII glyphs are rasterized lazily on first use and cached so that typical
//! streams of terminal output pay the rasterization cost at most once per char.
//!
//! All glyphs are stored as 8-bit alpha coverage masks — the renderer in
//! `render.rs` blends them against the cell foreground/background colours.

use std::cell::RefCell;
use std::collections::HashMap;

use fontdue::{Font, FontSettings};

/// Bundled font. JetBrains Mono Regular — Apache-2.0, fully compatible with
/// our `MIT OR Apache-2.0` license.
pub const FONT_DATA: &[u8] =
    include_bytes!("../../assets/fonts/JetBrainsMono-Regular.ttf");

/// A single rasterized glyph: an alpha-coverage bitmap plus positioning offsets.
#[derive(Clone)]
pub struct Glyph {
    pub bitmap: Vec<u8>,
    pub width: usize,
    pub height: usize,
    /// Horizontal offset from the cell left edge at which the bitmap begins.
    pub xmin: i32,
    /// Vertical offset from the baseline to the bitmap bottom (positive = above).
    pub ymin: i32,
}

/// Cached glyph atlas for a single font face at a fixed pixel size.
pub struct GlyphAtlas {
    pub cell_w: usize,
    pub cell_h: usize,
    pub baseline: usize,
    font_size: f32,
    font: Font,
    /// `None` means "tried to rasterize, got an empty/invisible glyph".
    cache: RefCell<HashMap<char, Option<Glyph>>>,
}

impl GlyphAtlas {
    /// Build the atlas at a given pixel height. A good default is around
    /// 16.0–18.0 for comfortable reading on a typical HiDPI display.
    pub fn new(font_size: f32) -> Self {
        let font = Font::from_bytes(FONT_DATA, FontSettings::default())
            .expect("bundled JetBrains Mono font must be valid");

        // Monospace: use 'M' as the canonical cell width reference.
        let (m_metrics, _) = font.rasterize('M', font_size);
        let cell_w = m_metrics.advance_width.ceil() as usize;

        let lm = font
            .horizontal_line_metrics(font_size)
            .expect("JetBrains Mono has horizontal line metrics");
        let cell_h = (lm.ascent - lm.descent + lm.line_gap).ceil() as usize;
        let baseline = lm.ascent.round() as usize;

        let mut cache: HashMap<char, Option<Glyph>> = HashMap::with_capacity(128);
        for code in 0x20u32..0x7f {
            if let Some(ch) = char::from_u32(code) {
                let (m, bitmap) = font.rasterize(ch, font_size);
                let glyph = if m.width == 0 || m.height == 0 {
                    None
                } else {
                    Some(Glyph {
                        bitmap,
                        width: m.width,
                        height: m.height,
                        xmin: m.xmin,
                        ymin: m.ymin,
                    })
                };
                cache.insert(ch, glyph);
            }
        }

        Self {
            cell_w,
            cell_h,
            baseline,
            font_size,
            font,
            cache: RefCell::new(cache),
        }
    }

    /// Look up a glyph, rasterizing it on demand the first time a new character
    /// is requested. Returns `None` for whitespace-like glyphs that have no
    /// pixels to draw (space, null, etc.).
    pub fn glyph(&self, ch: char) -> Option<Glyph> {
        if let Some(cached) = self.cache.borrow().get(&ch) {
            return cached.clone();
        }
        let (m, bitmap) = self.font.rasterize(ch, self.font_size);
        let glyph = if m.width == 0 || m.height == 0 {
            None
        } else {
            Some(Glyph {
                bitmap,
                width: m.width,
                height: m.height,
                xmin: m.xmin,
                ymin: m.ymin,
            })
        };
        self.cache.borrow_mut().insert(ch, glyph.clone());
        glyph
    }
}
