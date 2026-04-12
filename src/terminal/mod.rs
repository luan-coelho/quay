//! Terminal widget: PTY + alacritty_terminal + Slint Image blitting.

pub mod detect;
pub mod font;
pub mod input;
pub mod render;
pub mod replay;
pub mod session;

pub use font::GlyphAtlas;
pub use input::key_text_to_bytes;
pub use render::Framebuffer;
pub use session::PtySession;
