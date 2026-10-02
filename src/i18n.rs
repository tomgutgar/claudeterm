//! Spanish or English. Every string keeps both versions side by side
//! (`tr("…", "…")`, or `tr!("…", "…")` when it needs formatting), Spanish
//! first. There are no separate tables that can drift apart: a missing
//! translation does not compile.

use std::sync::OnceLock;

use crate::platform;

/// `CCT_LANG` wins; then `LC_ALL` / `LC_MESSAGES` / `LANG` (like gettext);
/// if none is set (the usual case on Windows), the system UI language.
pub fn es() -> bool {
    static ES: OnceLock<bool> = OnceLock::new();
    *ES.get_or_init(|| {
        let var = ["CCT_LANG", "LC_ALL", "LC_MESSAGES", "LANG"]
            .iter()
            .find_map(|v| std::env::var(v).ok().filter(|s| !s.is_empty()));
        is_es(&var.unwrap_or_else(platform::ui_lang))
    })
}

/// "es", "es_ES.UTF-8", "es-MX"… → Spanish. Anything else (or "C") → English.
fn is_es(locale: &str) -> bool {
    locale
        .get(..2)
        .is_some_and(|p| p.eq_ignore_ascii_case("es"))
}

pub fn tr(spanish: &'static str, english: &'static str) -> &'static str {
    if es() { spanish } else { english }
}

/// Like `tr` but with formatting: `tr!("hace {n}s", "{n}s ago")`.
#[macro_export]
macro_rules! tr {
    ($es:literal, $en:literal $(, $arg:expr)* $(,)?) => {
        if $crate::i18n::es() { format!($es $(, $arg)*) } else { format!($en $(, $arg)*) }
    };
}

#[cfg(test)]
mod tests {
    #[test]
    fn locales() {
        assert!(super::is_es("es_ES.UTF-8"));
        assert!(super::is_es("ES"));
        assert!(super::is_es("es-MX"));
        assert!(!super::is_es("en_US.UTF-8"));
        assert!(!super::is_es("C"));
        assert!(!super::is_es(""));
    }
}
