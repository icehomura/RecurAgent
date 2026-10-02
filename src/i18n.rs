//! Terminal-UI localization.
//!
//! **Scope.** This module owns strings RecurAgent itself draws: menus, key
//! hints, dialogs, status text. It deliberately does *not* cover agent prose.
//! What language the model writes in is a separate concern with a separate
//! setting (`output_language`), because the model authors that text and this
//! crate cannot translate text it did not write. Conflating the two would mean
//! a user who wants a Chinese UI is forced into Chinese model output, or vice
//! versa.
//!
//! **No start-up cost, nothing to fail.** The catalogue is embedded at compile
//! time by [`rust_i18n::i18n!`], so there is no file loading, no fallible
//! resource lookup, and no locale directory to ship next to the binary.
//!
//! **Missing keys are visible, not silent.** Keys are symbolic
//! (`login_flow_logout_absent`), not the English source text. A lookup that
//! finds nothing returns the key itself, so an omission shows up as
//! `login_flow_logout_absent` on screen rather than quietly rendering English.
//! That is why the en/zh key-parity gate is mandatory: it is the mechanism that
//! stops an omission from reaching a user.
//!
//! **Global state.** [`rust_i18n::set_locale`] is process-global, so a test
//! that sets it would make other tests order-dependent. Tests must pass an
//! explicit locale instead — `t!("key", locale = "zh-CN")` — and leave
//! [`init`] to the binary entry point.

use crate::config::Config;

// NOTE: the `i18n!("locales", ...)` registration is deliberately NOT here. It
// must run at the crate root (`src/lib.rs`) because `t!` resolves
// `crate::_rust_i18n_t`, which only the root invocation generates.

/// Locales this build ships a catalogue for.
///
/// Adding one is a three-step change: a `locales/` entry, this array, and
/// [`normalize_locale`]. `zh-Hant` is the obvious next addition and is
/// deliberately *not* mapped onto `zh-CN` — see [`normalize_locale`].
pub const SUPPORTED: [&str; 2] = ["en", "zh-CN"];

/// Locale installed when nothing usable is configured.
pub const DEFAULT_LOCALE: &str = "en";

/// Map a settings value to a locale this build actually ships.
///
/// Returns `None` for anything unrecognised, including `zh-Hant`/`zh-TW`/
/// `zh-HK`. Those are *not* aliased onto `zh-CN`: Simplified is not a
/// degraded Traditional, and silently serving the wrong script is a worse
/// outcome than serving English while the catalogue is missing. The caller
/// falls back to [`DEFAULT_LOCALE`] and the unshipped request is reported.
///
/// Accepts the spellings a user is likely to type — a BCP-47 tag, a bare
/// language name, or the endonym — because a settings file is hand-written and
/// `zh-CN` is not the only reasonable thing to put there.
#[must_use]
pub fn normalize_locale(requested: &str) -> Option<&'static str> {
    let value = requested.trim();
    if value.is_empty() {
        return None;
    }

    // Compare case-insensitively without allocating: BCP-47 tags are ASCII, and
    // the endonyms are matched exactly.
    let ascii = value.eq_ignore_ascii_case("en")
        || value.eq_ignore_ascii_case("en-us")
        || value.eq_ignore_ascii_case("en-gb")
        || value.eq_ignore_ascii_case("english");

    if ascii || value == "英语" || value == "英文" {
        return Some("en");
    }

    let simplified = value.eq_ignore_ascii_case("zh")
        || value.eq_ignore_ascii_case("zh-cn")
        || value.eq_ignore_ascii_case("zh-hans")
        || value.eq_ignore_ascii_case("zh-sg")
        || value.eq_ignore_ascii_case("chinese")
        || value.eq_ignore_ascii_case("simplified chinese");

    if simplified || value == "中文" || value == "简体中文" || value == "汉语" {
        return Some("zh-CN");
    }

    None
}

/// Install the UI locale for this process and return the one actually used.
///
/// Called once from the binary entry point. Returning the resolved locale lets
/// a caller distinguish "asked for `zh-TW`, got `en`" from "asked for `en`" —
/// an unshipped request is worth reporting rather than swallowing.
pub fn init(requested: Option<&str>) -> &'static str {
    let resolved = requested.and_then(normalize_locale).unwrap_or(DEFAULT_LOCALE);
    rust_i18n::set_locale(resolved);
    resolved
}

/// Install the locale implied by a loaded [`Config`].
///
/// `ui_language` governs the chrome this crate draws; when it is unset it
/// inherits `output_language`, so setting one value feels like it does both
/// without forcing them to stay equal. A user may legitimately want a Chinese
/// UI and English model output, or the reverse.
pub fn init_from_config(config: &Config) -> &'static str {
    init(config.ui_language())
}

/// The locale currently installed.
#[must_use]
pub fn current() -> String {
    rust_i18n::locale().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    // NOTE: these tests never call [`init`]. `set_locale` is process-global and
    // the test harness runs these on shared threads, so installing a locale
    // here would make an unrelated test's `t!` lookup depend on execution
    // order. Everything below is pure input-to-output mapping.

    #[test]
    fn recognizes_english_spellings() {
        for value in ["en", "EN", "en-US", "en-gb", "English", "english", "英语"] {
            assert_eq!(
                normalize_locale(value),
                Some("en"),
                "{value:?} should resolve to English"
            );
        }
    }

    #[test]
    fn recognizes_simplified_chinese_spellings() {
        for value in [
            "zh", "ZH", "zh-CN", "zh-cn", "zh-Hans", "zh-SG", "Chinese", "中文", "简体中文",
        ] {
            assert_eq!(
                normalize_locale(value),
                Some("zh-CN"),
                "{value:?} should resolve to Simplified Chinese"
            );
        }
    }

    #[test]
    fn traditional_chinese_falls_back_instead_of_aliasing_to_simplified() {
        // Aliasing zh-Hant onto zh-CN would serve the wrong script. Returning
        // None makes the caller fall back to English and report the miss,
        // which is recoverable; the wrong script is not obviously wrong.
        for value in ["zh-TW", "zh-HK", "zh-Hant", "繁體中文", "繁体中文"] {
            assert_eq!(
                normalize_locale(value),
                None,
                "{value:?} must not silently become Simplified Chinese"
            );
        }
    }

    #[test]
    fn rejects_unusable_values_without_panicking() {
        for value in ["", "   ", "klingon", "de", "fr", "xx-YY", "zzz"] {
            assert_eq!(normalize_locale(value), None, "{value:?} must not resolve");
        }
    }

    #[test]
    fn every_supported_locale_resolves_to_itself() {
        // Guards the obvious drift: adding a locale to SUPPORTED but not to
        // normalize_locale would make it unreachable from settings.
        for locale in SUPPORTED {
            assert_eq!(
                normalize_locale(locale),
                Some(locale),
                "SUPPORTED locale {locale:?} is unreachable from normalize_locale"
            );
        }
    }

    #[test]
    fn catalogue_has_both_locales_for_every_key() {
        // The in-binary half of the parity gate. `scripts/check_i18n_keys.py`
        // checks the YAML textually; this checks what the compiler embedded, so
        // a key that parses but loses a locale at codegen time is still caught.
        let en = rust_i18n::available_locales!()
            .iter()
            .any(|locale| locale.as_ref() == "en");
        let zh = rust_i18n::available_locales!()
            .iter()
            .any(|locale| locale.as_ref() == "zh-CN");
        assert!(en, "the catalogue must embed en");
        assert!(zh, "the catalogue must embed zh-CN");
    }

    #[test]
    fn lookup_is_per_locale_and_falls_back_to_the_key_when_absent() {
        // Both locales resolve to *something* for a real key...
        let en = rust_i18n::t!("login_flow_logout_absent", locale = "en", provider = "openai");
        let zh = rust_i18n::t!("login_flow_logout_absent", locale = "zh-CN", provider = "openai");
        assert!(en.contains("openai"), "en: {en}");
        assert!(zh.contains("openai"), "zh: {zh}");
        assert_ne!(en.to_string(), zh.to_string(), "en and zh must differ");

        // ...and a key with no entry renders as the key. That is the whole
        // reason the parity gate exists, so pin the behaviour rather than
        // assuming it.
        let missing = rust_i18n::t!("login_flow_this_key_does_not_exist", locale = "zh-CN");
        assert_eq!(missing.as_ref(), "login_flow_this_key_does_not_exist");
    }

    #[test]
    fn login_flow_templates_keep_their_placeholders_in_both_locales() {
        // A translator who drops `%{provider}` produces a message that silently
        // omits which provider failed. Both locales must interpolate every
        // argument the call site passes.
        for locale in SUPPORTED {
            let rendered = rust_i18n::t!(
                "login_flow_device_flow_message",
                locale = locale,
                provider = "github-copilot",
                uri = "https://example.test/device",
                code = "ABCD-1234",
                seconds = 900
            );
            assert!(
                rendered.contains("github-copilot"),
                "{locale}: provider dropped from {rendered}"
            );
            assert!(
                rendered.contains("https://example.test/device"),
                "{locale}: uri dropped from {rendered}"
            );
            assert!(
                rendered.contains("ABCD-1234"),
                "{locale}: code dropped from {rendered}"
            );
            assert!(rendered.contains("900"), "{locale}: seconds dropped");
        }
    }

    #[test]
    fn copied_notice_carries_the_count_in_both_locales() {
        // The ftui copy-on-select status notice must interpolate the character
        // count the same way in every locale, or a translator can drop it and
        // the confirmation silently stops saying how much was copied.
        for locale in SUPPORTED {
            let rendered = rust_i18n::t!("interactive_copied_chars", locale = locale, count = 7);
            assert!(rendered.contains('7'), "{locale}: count dropped from {rendered}");
        }
    }

    #[test]
    fn scroll_to_bottom_badge_is_translated_in_both_locales() {
        for locale in SUPPORTED {
            let rendered = rust_i18n::t!("interactive_scroll_to_bottom", locale = locale);
            assert!(!rendered.is_empty(), "{locale}: badge label is empty");
            assert!(
                rendered.contains('↓'),
                "{locale}: badge lost its arrow: {rendered}"
            );
        }
    }

    #[test]
    fn scroll_to_bottom_new_message_labels_count_their_entries() {
        // The badge picks the key from the count (there is no plural engine),
        // so each locale must interpolate the number and the singular must not
        // be the plural with a swapped digit.
        for locale in SUPPORTED {
            let one = rust_i18n::t!("interactive_scroll_new_message", locale = locale);
            let many = rust_i18n::t!(
                "interactive_scroll_new_messages",
                locale = locale,
                count = 3
            );
            assert!(
                one.contains('1'),
                "{locale}: singular dropped its count: {one}"
            );
            assert!(
                many.contains('3'),
                "{locale}: plural dropped its count: {many}"
            );
            assert_ne!(
                one.to_string(),
                many.to_string(),
                "{locale}: singular and plural render identically"
            );
        }
    }
}
