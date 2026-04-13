//! Locale detection and switching — bridges `rust-i18n` (Rust-side
//! strings) and Slint's gettext-backed `@tr()` (UI-side strings).

use crate::settings::{self, Settings};

/// The set of locales we actually ship translations for.
const SUPPORTED_LOCALES: &[&str] = &["en", "pt-BR"];

/// Initialise the active locale from the user's saved preference,
/// falling back to the OS locale, falling back to `"en"`.
pub fn init_locale(settings: &Settings<'_>) {
    let locale = settings
        .get(settings::KEY_LOCALE)
        .ok()
        .flatten()
        .unwrap_or_else(detect_system_locale);

    apply_locale(&locale);
}

/// Best-effort OS locale detection via `sys-locale`.
pub fn detect_system_locale() -> String {
    sys_locale::get_locale().unwrap_or_else(|| "en".to_string())
}

/// Set the active locale for both translation backends.
pub fn apply_locale(locale: &str) {
    let norm = resolve_locale(locale);
    tracing::info!(raw = %locale, resolved = %norm, "applying locale");

    // rust-i18n (Rust-side t!() strings)
    rust_i18n::set_locale(&norm);

    // Slint gettext (@tr() strings) — select the bundled .mo catalogue.
    if let Err(e) = slint::select_bundled_translation(&norm) {
        tracing::warn!(locale = %norm, err = %e, "failed to load Slint translations, falling back to source strings");
    }
}

/// Map a raw locale string (from the OS or user settings) to one of
/// our supported locale tags.
///
/// The mapping is:
/// - `"pt-BR"`, `"pt_BR"`, `"pt_BR.UTF-8"`, `"pt"` → `"pt-BR"`
/// - Everything else → `"en"`
///
/// This keeps the lookup in sync with our `locales/*.yml` filenames.
pub fn resolve_locale(raw: &str) -> String {
    // Strip encoding suffix ("UTF-8", "utf8", …)
    let base = raw.split('.').next().unwrap_or(raw);
    // Normalise separator
    let norm = base.replace('_', "-");

    // Exact match against supported locales.
    if SUPPORTED_LOCALES.contains(&norm.as_str()) {
        return norm;
    }

    // Try language-only prefix (e.g. "en-US" → "en", "pt" → "pt-BR").
    let lang = norm.split('-').next().unwrap_or(&norm);
    for &supported in SUPPORTED_LOCALES {
        if supported == lang || supported.starts_with(&format!("{lang}-")) {
            return supported.to_string();
        }
    }

    // Default fallback.
    "en".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_exact_match() {
        assert_eq!(resolve_locale("en"), "en");
        assert_eq!(resolve_locale("pt-BR"), "pt-BR");
    }

    #[test]
    fn resolve_strips_encoding() {
        assert_eq!(resolve_locale("pt_BR.UTF-8"), "pt-BR");
        assert_eq!(resolve_locale("en_US.UTF-8"), "en");
    }

    #[test]
    fn resolve_region_to_base() {
        assert_eq!(resolve_locale("en_US"), "en");
        assert_eq!(resolve_locale("en-GB"), "en");
    }

    #[test]
    fn resolve_language_only_pt() {
        assert_eq!(resolve_locale("pt"), "pt-BR");
    }

    #[test]
    fn resolve_unknown_falls_back() {
        assert_eq!(resolve_locale("fr"), "en");
        assert_eq!(resolve_locale("ja_JP"), "en");
    }
}
