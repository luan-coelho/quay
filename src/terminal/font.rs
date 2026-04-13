//! Monospace glyph atlas built on top of `fontdue`.
//!
//! Loads Geist Mono Regular (bundled via `include_bytes!`) once at startup,
//! computes the cell metrics and pre-rasterizes the printable ASCII range. Non-
//! ASCII glyphs are rasterized lazily on first use and cached so that typical
//! streams of terminal output pay the rasterization cost at most once per char.
//!
//! All glyphs are stored as 8-bit alpha coverage masks — the renderer in
//! `render.rs` blends them against the cell foreground/background colours.
//!
//! The terminal atlas uses the same monospace face (Geist Mono) that the
//! Slint chrome loads via `Tokens.font-mono`, so there's a single coherent
//! typographic identity between the PTY surface and the surrounding UI.
//!
//! Fallback chain: Geist Mono → JetBrains Mono → DejaVu Sans Mono.
//! Characters not in Geist Mono (e.g. ☐☑✓▸※❯) are rendered from the
//! next font that has them. This avoids tofu for the Unicode symbols that
//! CLI tools like Claude Code routinely emit.

use std::cell::RefCell;
use std::collections::HashMap;

use fontdue::{Font, FontSettings};

/// Bundled primary font. Geist Mono Regular — SIL Open Font License.
pub const FONT_DATA: &[u8] =
    include_bytes!("../../assets/fonts/GeistMono-Regular.ttf");

/// Fallback #1. JetBrains Mono Regular — SIL Open Font License.
/// Covers ▸▪✓❯ and other symbols missing from Geist Mono.
const FALLBACK_JETBRAINS: &[u8] =
    include_bytes!("../../assets/fonts/JetBrainsMono-Regular.ttf");

/// Fallback #2. DejaVu Sans Mono — Bitstream Vera license (permissive).
/// Covers ☐☑☒※✔ and the remaining symbols missing from both above.
const FALLBACK_DEJAVU: &[u8] =
    include_bytes!("../../assets/fonts/DejaVuSansMono.ttf");

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

/// Cached glyph atlas with a font fallback chain.
pub struct GlyphAtlas {
    pub cell_w: usize,
    pub cell_h: usize,
    pub baseline: usize,
    font_size: f32,
    /// Fallback chain: [Geist Mono, JetBrains Mono, DejaVu Sans Mono].
    fonts: Vec<Font>,
    /// `None` means "tried every font in the chain, got nothing visible".
    cache: RefCell<HashMap<char, Option<Glyph>>>,
}

impl GlyphAtlas {
    /// Build the atlas at a given pixel height. A good default is around
    /// 16.0–18.0 for comfortable reading on a typical HiDPI display.
    pub fn new(font_size: f32) -> Self {
        let primary = Font::from_bytes(FONT_DATA, FontSettings::default())
            .expect("bundled Geist Mono font must be valid");

        // Monospace: use 'M' as the canonical cell width reference.
        let (m_metrics, _) = primary.rasterize('M', font_size);
        let cell_w = m_metrics.advance_width.ceil() as usize;

        let lm = primary
            .horizontal_line_metrics(font_size)
            .expect("Geist Mono has horizontal line metrics");
        let cell_h = (lm.ascent - lm.descent + lm.line_gap).ceil() as usize;
        let baseline = lm.ascent.round() as usize;

        // Pre-rasterize printable ASCII from the primary font.
        let mut cache: HashMap<char, Option<Glyph>> = HashMap::with_capacity(128);
        for code in 0x20u32..0x7f {
            if let Some(ch) = char::from_u32(code) {
                let (m, bitmap) = primary.rasterize(ch, font_size);
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

        // Build fallback chain.
        let mut fonts = vec![primary];
        if let Ok(f) = Font::from_bytes(FALLBACK_JETBRAINS, FontSettings::default()) {
            fonts.push(f);
        }
        if let Ok(f) = Font::from_bytes(FALLBACK_DEJAVU, FontSettings::default()) {
            fonts.push(f);
        }

        Self {
            cell_w,
            cell_h,
            baseline,
            font_size,
            fonts,
            cache: RefCell::new(cache),
        }
    }

    /// Look up a glyph, rasterizing it on demand the first time a new character
    /// is requested. Walks the fallback chain (Geist Mono → JetBrains Mono →
    /// DejaVu Sans Mono) to find a font that actually contains the glyph.
    /// Returns `None` for whitespace-like glyphs that have no pixels to draw.
    pub fn glyph(&self, ch: char) -> Option<Glyph> {
        if let Some(cached) = self.cache.borrow().get(&ch) {
            return cached.clone();
        }

        let glyph = self.rasterize_with_fallback(ch);
        self.cache.borrow_mut().insert(ch, glyph.clone());
        glyph
    }

    /// Try each font in the chain. Use `has_glyph()` to skip fonts that would
    /// only produce the `.notdef` tofu rectangle.
    fn rasterize_with_fallback(&self, ch: char) -> Option<Glyph> {
        for font in &self.fonts {
            if !font.has_glyph(ch) {
                continue;
            }
            let (m, bitmap) = font.rasterize(ch, self.font_size);
            if m.width == 0 || m.height == 0 {
                continue;
            }
            return Some(Glyph {
                bitmap,
                width: m.width,
                height: m.height,
                xmin: m.xmin,
                ymin: m.ymin,
            });
        }
        None
    }
}
