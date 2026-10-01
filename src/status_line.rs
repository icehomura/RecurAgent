#![forbid(unsafe_code)]

//! Powerline status line, footer, and sticky HUDs (OMP-ADOPT / bd-cv653.9.4).
//!
//! Provides customizable status-line rendering with powerline glyphs, segment
//! priority-based responsive dropping, and per-session accent hue calculation.

use serde::{Deserialize, Serialize};

// Width is measured in terminal cells, never in `char`s: CJK ideographs and
// emoji occupy two cells, so a chars-based approximation understates the width
// of Chinese/Japanese/Korean text by a factor of two and produces over-long
// status lines. `unicode-width` is an unconditional dependency for this reason
// (it also arrives transitively via `rich_rust`), so there is no degraded
// non-TUI fallback to keep in sync.
fn display_width(text: &str) -> usize {
    unicode_width::UnicodeWidthStr::width(text)
}

fn truncate_display_width(text: &str, maximum_width: usize) -> String {
    let mut end = 0;
    for (index, character) in text.char_indices() {
        let candidate_end = index + character.len_utf8();
        if display_width(&text[..candidate_end]) > maximum_width {
            break;
        }
        end = candidate_end;
    }
    text[..end].to_string()
}

/// Predefined status line presets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StatusLinePreset {
    #[default]
    Default,
    Minimal,
    Compact,
    Full,
    Nerd,
    Ascii,
}

/// Powerline separator style.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SeparatorStyle {
    #[default]
    Powerline, //  / 
    Thin,  //  / 
    Slash, // /
    Dot,   // •
    Pipe,  // |
}

impl SeparatorStyle {
    #[must_use]
    pub const fn glyph(self) -> &'static str {
        match self {
            Self::Powerline => "",
            Self::Thin => "",
            Self::Slash => "/",
            Self::Dot => "•",
            Self::Pipe => "|",
        }
    }
}

// The glyph set itself lives in `config` — it is a settings value, and
// `config` is not behind the `tui` feature that gates this module. Only the
// renderer's mapping from it to a separator belongs here.
pub use crate::config::StatusLineChrome;

impl crate::config::StatusLineChrome {
    /// Separator drawn between segments.
    #[must_use]
    pub const fn separator(self) -> SeparatorStyle {
        match self {
            Self::Unicode => SeparatorStyle::Pipe,
            Self::Ascii => SeparatorStyle::Pipe,
            Self::Nerd => SeparatorStyle::Powerline,
        }
    }
}

/// Status segment identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SegmentId {
    Model,
    Thinking,
    Mode,
    Path,
    Git,
    ContextPct,
    Cost,
    Tokens,
    Subagents,
    SessionName,
    Time,
}

/// Status segment rendering context.
#[derive(Debug, Clone, Default)]
pub struct StatusContext<'a> {
    pub model: &'a str,
    pub thinking_level: Option<&'a str>,
    pub mode: &'a str,
    pub cwd: &'a str,
    pub git_branch: Option<&'a str>,
    pub git_dirty: bool,
    pub context_pct: u8,
    pub cost_usd: f64,
    pub tokens_used: u64,
    pub subagent_count: usize,
    pub session_name: &'a str,
    pub timestamp_str: &'a str,
}

/// Individual segment definition with priority and rendering logic.
#[derive(Debug, Clone)]
pub struct StatusSegment {
    pub id: SegmentId,
    pub priority: u8, // 1 = highest, 10 = lowest (dropped first on narrow terminals)
    pub min_width: usize,
}

impl StatusSegment {
    #[must_use]
    pub const fn new(id: SegmentId, priority: u8, min_width: usize) -> Self {
        Self {
            id,
            priority,
            min_width,
        }
    }

    #[must_use]
    pub fn render(&self, ctx: &StatusContext) -> Option<String> {
        self.render_with_chrome(ctx, StatusLineChrome::default())
    }

    fn render_with_chrome(
        &self,
        ctx: &StatusContext,
        chrome: StatusLineChrome,
    ) -> Option<String> {
        let use_icons = chrome.uses_icons();
        let use_symbols = chrome.uses_symbols();
        fn text(value: &str) -> Option<String> {
            let sanitized: String = value
                .chars()
                .filter(|character| !character.is_control())
                .collect();
            let sanitized = sanitized.trim();
            (!sanitized.is_empty()).then(|| sanitized.to_string())
        }

        match self.id {
            SegmentId::Model => {
                let model = text(ctx.model)?;
                if use_icons {
                    Some(format!("󰚩 {model}"))
                } else {
                    Some(model)
                }
            }
            SegmentId::Thinking => {
                let level = text(ctx.thinking_level?)?;
                let rendered = if use_icons {
                    format!("󱜙 {level}")
                } else if use_symbols {
                    // `∴` is the marker the transcript already uses for a
                    // reasoning entry, and it sits in a block every monospace
                    // font carries — unlike the Font Awesome glyph above.
                    format!("∴ {level}")
                } else {
                    format!("think:{level}")
                };
                Some(rendered)
            }
            SegmentId::Mode => {
                let mode = text(ctx.mode)?;
                Some(mode.to_uppercase())
            }
            SegmentId::Path => {
                let cwd = text(ctx.cwd)?;
                if use_icons {
                    Some(format!(" {cwd}"))
                } else {
                    Some(cwd)
                }
            }
            SegmentId::Git => {
                let branch = text(ctx.git_branch?)?;
                let status_icon = if ctx.git_dirty { "*" } else { "" };
                Some(if use_icons {
                    format!(" {branch}{status_icon}")
                } else {
                    format!("git:{branch}{status_icon}")
                })
            }
            SegmentId::ContextPct => Some(format!("ctx: {}%", ctx.context_pct)),
            SegmentId::Cost => {
                if ctx.cost_usd > 0.0 {
                    Some(format!("${:.3}", ctx.cost_usd))
                } else {
                    None
                }
            }
            SegmentId::Tokens => {
                if ctx.tokens_used > 0 {
                    Some(format!("{} tok", ctx.tokens_used))
                } else {
                    None
                }
            }
            SegmentId::Subagents => {
                if ctx.subagent_count > 0 {
                    Some(if use_icons {
                        format!("󰭻 {}", ctx.subagent_count)
                    } else {
                        format!("agents:{}", ctx.subagent_count)
                    })
                } else {
                    None
                }
            }
            SegmentId::SessionName => {
                let session_name = text(ctx.session_name)?;
                if use_icons {
                    Some(format!("🏷 {session_name}"))
                } else {
                    Some(format!("session:{session_name}"))
                }
            }
            SegmentId::Time => text(ctx.timestamp_str),
        }
    }
}

/// Powerline status line renderer.
#[derive(Debug, Clone)]
pub struct PowerlineStatusLine {
    pub preset: StatusLinePreset,
    /// Glyph set: separators and icons. Derived from the preset by
    /// [`Self::with_preset`], or set explicitly from `statusLine.chrome` in
    /// settings via [`Self::with_preset_and_chrome`].
    pub chrome: StatusLineChrome,
    pub separator: SeparatorStyle,
    pub segments: Vec<StatusSegment>,
}

impl Default for PowerlineStatusLine {
    fn default() -> Self {
        Self::with_preset(StatusLinePreset::Default)
    }
}

impl PowerlineStatusLine {
    #[must_use]
    pub fn with_preset(preset: StatusLinePreset) -> Self {
        let segments = match preset {
            StatusLinePreset::Minimal => vec![
                StatusSegment::new(SegmentId::Model, 1, 10),
                StatusSegment::new(SegmentId::Mode, 2, 6),
            ],
            StatusLinePreset::Compact => vec![
                StatusSegment::new(SegmentId::Model, 1, 10),
                StatusSegment::new(SegmentId::Thinking, 3, 8),
                StatusSegment::new(SegmentId::Mode, 2, 6),
                StatusSegment::new(SegmentId::Git, 4, 12),
                StatusSegment::new(SegmentId::ContextPct, 5, 8),
            ],
            StatusLinePreset::Default | StatusLinePreset::Nerd => vec![
                StatusSegment::new(SegmentId::Model, 1, 10),
                StatusSegment::new(SegmentId::Thinking, 3, 8),
                StatusSegment::new(SegmentId::Mode, 2, 6),
                StatusSegment::new(SegmentId::Path, 4, 14),
                StatusSegment::new(SegmentId::Git, 5, 12),
                StatusSegment::new(SegmentId::ContextPct, 6, 8),
                StatusSegment::new(SegmentId::Cost, 7, 8),
                StatusSegment::new(SegmentId::Subagents, 8, 6),
            ],
            StatusLinePreset::Full => vec![
                StatusSegment::new(SegmentId::Model, 1, 10),
                StatusSegment::new(SegmentId::Thinking, 3, 8),
                StatusSegment::new(SegmentId::Mode, 2, 6),
                StatusSegment::new(SegmentId::Path, 4, 14),
                StatusSegment::new(SegmentId::Git, 5, 12),
                StatusSegment::new(SegmentId::ContextPct, 6, 8),
                StatusSegment::new(SegmentId::Tokens, 7, 10),
                StatusSegment::new(SegmentId::Cost, 8, 8),
                StatusSegment::new(SegmentId::Subagents, 9, 6),
                StatusSegment::new(SegmentId::SessionName, 10, 12),
                StatusSegment::new(SegmentId::Time, 11, 8),
            ],
            StatusLinePreset::Ascii => vec![
                StatusSegment::new(SegmentId::Model, 1, 10),
                StatusSegment::new(SegmentId::Mode, 2, 6),
                StatusSegment::new(SegmentId::Git, 3, 10),
                StatusSegment::new(SegmentId::ContextPct, 4, 8),
            ],
        };

        // Private-use / Font-Awesome glyphs are opt-in only. Only the `Nerd`
        // preset asks for them; every other preset gets the portable glyph
        // set, so a terminal with no patched font never renders a row of
        // replacement boxes.
        let chrome = match preset {
            StatusLinePreset::Nerd => StatusLineChrome::Nerd,
            StatusLinePreset::Ascii => StatusLineChrome::Ascii,
            _ => StatusLineChrome::Unicode,
        };

        Self {
            preset,
            chrome,
            separator: chrome.separator(),
            segments,
        }
    }

    /// [`Self::with_preset`] with an explicit glyph set, for callers that read
    /// `statusLine.chrome` from settings.
    #[must_use]
    pub fn with_preset_and_chrome(preset: StatusLinePreset, chrome: StatusLineChrome) -> Self {
        let mut line = Self::with_preset(preset);
        line.chrome = chrome;
        line.separator = chrome.separator();
        line
    }

    /// Render status line fitted into `available_width`.
    #[must_use]
    pub fn render(&self, ctx: &StatusContext, available_width: usize) -> String {
        let mut rendered_segments = Vec::new();
        for seg in &self.segments {
            if let Some(text) = seg.render_with_chrome(ctx, self.chrome) {
                rendered_segments.push((seg.priority, text));
            }
        }

        // Sort by priority descending to identify segments to drop first
        let sep_width = display_width(self.separator.glyph()) + 2;
        let sep_str = format!(" {} ", self.separator.glyph());
        while !rendered_segments.is_empty() {
            let total_len: usize = rendered_segments
                .iter()
                .map(|(_, text)| display_width(text))
                .sum::<usize>()
                + if rendered_segments.len() > 1 {
                    (rendered_segments.len() - 1) * sep_width
                } else {
                    0
                };

            if total_len <= available_width || rendered_segments.len() <= 1 {
                break;
            }

            // Drop lowest priority segment (highest priority number)
            if let Some((max_idx, _)) = rendered_segments
                .iter()
                .enumerate()
                .max_by_key(|(_, (p, _))| *p)
            {
                rendered_segments.remove(max_idx);
            }
        }

        let rendered = rendered_segments
            .into_iter()
            .map(|(_, text)| text)
            .collect::<Vec<_>>()
            .join(&sep_str);
        truncate_display_width(&rendered, available_width)
    }
}

/// Compute a stable per-session accent hue (0..360) based on djb2 hash.
#[must_use]
pub fn compute_session_accent_hue(session_name: &str) -> u16 {
    if session_name.is_empty() {
        return 210; // Default cool blue
    }

    let mut hash: u64 = 5381;
    for byte in session_name.bytes() {
        hash = ((hash << 5).wrapping_add(hash)).wrapping_add(u64::from(byte));
    }

    (hash % 360) as u16
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_powerline_status_line_presets() {
        let ctx = StatusContext {
            model: "claude-3-7-sonnet",
            thinking_level: Some("high"),
            mode: "plan",
            cwd: "recur_agent",
            git_branch: Some("main"),
            git_dirty: true,
            context_pct: 42,
            cost_usd: 0.125,
            tokens_used: 12500,
            subagent_count: 2,
            session_name: "alpha-session",
            timestamp_str: "14:02:00",
        };

        let minimal = PowerlineStatusLine::with_preset(StatusLinePreset::Minimal);
        let min_rendered = minimal.render(&ctx, 120);
        assert!(min_rendered.contains("claude-3-7-sonnet"));
        assert!(min_rendered.contains("PLAN"));

        let full = PowerlineStatusLine::with_preset(StatusLinePreset::Full);
        let full_rendered = full.render(&ctx, 200);
        assert!(full_rendered.contains("claude-3-7-sonnet"));
        assert!(full_rendered.contains("main*"));
        assert!(full_rendered.contains("42%"));
        assert!(full_rendered.contains("$0.125"));
    }

    #[test]
    fn test_status_line_responsive_dropping() {
        let ctx = StatusContext {
            model: "gemini-2.5-pro",
            thinking_level: Some("low"),
            mode: "agent",
            cwd: "/very/long/path/to/project/src",
            git_branch: Some("feature/super-long-branch-name"),
            git_dirty: false,
            context_pct: 88,
            cost_usd: 1.450,
            tokens_used: 98000,
            subagent_count: 5,
            session_name: "long-session-descriptor",
            timestamp_str: "18:45:12",
        };

        let status_line = PowerlineStatusLine::with_preset(StatusLinePreset::Full);
        // Wide terminal: all segments present
        let wide = status_line.render(&ctx, 200);
        assert!(wide.contains("gemini-2.5-pro"));
        assert!(wide.contains("feature/super-long-branch-name"));

        // Narrow terminal: lower priority segments dropped
        let narrow = status_line.render(&ctx, 35);
        assert!(display_width(&narrow) <= 35);
    }

    #[test]
    fn test_status_line_clamps_a_single_long_segment() {
        let ctx = StatusContext {
            model: "model-name-that-is-much-too-long",
            ..StatusContext::default()
        };
        let status_line = PowerlineStatusLine::with_preset(StatusLinePreset::Minimal);
        let rendered = status_line.render(&ctx, 8);
        assert_eq!(display_width(&rendered), 8);
    }

    // Not gated on `tui`: `unicode-width` is unconditional, so wide-character
    // clamping must hold on the `--no-default-features` (SDK) build too. This
    // is the regression guard for the chars-based fallback that used to make
    // CJK status lines twice as wide as the terminal budget allowed.
    #[test]
    fn test_status_line_clamps_wide_unicode_to_terminal_cells() {
        let ctx = StatusContext {
            model: "模型🙂模型🙂",
            ..StatusContext::default()
        };
        let status_line = PowerlineStatusLine::with_preset(StatusLinePreset::Minimal);
        let rendered = status_line.render(&ctx, 7);

        assert!(display_width(&rendered) <= 7, "rendered {rendered:?}");
        // The clamp measures *cells*, so it keeps whole wide glyphs and stops
        // before a 2-cell character would overflow the budget. Anything
        // char-indexed would have kept 7 chars and produced a row twice as
        // wide as the terminal.
        assert!(
            rendered.chars().count() < display_width(&rendered),
            "clamp is not measuring cells: {rendered:?}"
        );
        // Pin the units directly: `chars().count()` would answer 3, not 6, and
        // that factor-of-two error is exactly the regression being guarded.
        assert_eq!(display_width("模型🙂"), 6);
    }

    #[test]
    fn test_ascii_preset_uses_only_ascii_chrome() {
        let ctx = StatusContext {
            model: "gpt-4o",
            mode: "act",
            git_branch: Some("main"),
            git_dirty: true,
            context_pct: 42,
            ..StatusContext::default()
        };
        let status_line = PowerlineStatusLine::with_preset(StatusLinePreset::Ascii);
        let rendered = status_line.render(&ctx, 120);
        assert!(rendered.is_ascii(), "ASCII preset rendered {rendered:?}");
        assert!(rendered.contains("git:main*"));
    }

    /// `statusLine.chrome` round-trips, each variant picks the separator the
    /// renderer documents, and `with_preset_and_chrome` really drives the
    /// output (so `/statusline ascii` cannot silently keep drawing `•`).
    #[test]
    fn chrome_names_round_trip_and_drive_the_rendered_row() {
        for chrome in [
            StatusLineChrome::Unicode,
            StatusLineChrome::Ascii,
            StatusLineChrome::Nerd,
        ] {
            assert_eq!(
                StatusLineChrome::from_name(chrome.name()),
                Some(chrome),
                "{chrome:?} did not round-trip through its own name"
            );
        }
        assert_eq!(
            StatusLineChrome::from_name(" NERD "),
            Some(StatusLineChrome::Nerd)
        );
        assert_eq!(StatusLineChrome::from_name("fancy"), None);
        assert_eq!(StatusLineChrome::default(), StatusLineChrome::Unicode);
        assert_eq!(
            StatusLineChrome::Unicode.separator().glyph(),
            SeparatorStyle::Pipe.glyph()
        );
        assert_eq!(
            StatusLineChrome::Ascii.separator().glyph(),
            SeparatorStyle::Pipe.glyph()
        );
        assert_eq!(
            StatusLineChrome::Nerd.separator().glyph(),
            SeparatorStyle::Powerline.glyph()
        );
        // The preset a bare `with_preset` produces is the portable one.
        assert_eq!(
            PowerlineStatusLine::with_preset(StatusLinePreset::Default).chrome,
            StatusLineChrome::Unicode
        );

        let ctx = StatusContext {
            model: "gpt-4o",
            thinking_level: Some("high"),
            mode: "act",
            git_branch: Some("main"),
            ..StatusContext::default()
        };
        let ascii = PowerlineStatusLine::with_preset_and_chrome(
            StatusLinePreset::Default,
            StatusLineChrome::Ascii,
        );
        assert_eq!(ascii.separator.glyph(), "|");
        let rendered = ascii.render(&ctx, 200);
        assert!(
            rendered.is_ascii(),
            "`ascii` chrome must render 7-bit: {rendered:?}"
        );

        let nerd = PowerlineStatusLine::with_preset_and_chrome(
            StatusLinePreset::Default,
            StatusLineChrome::Nerd,
        );
        assert_ne!(
            nerd.render(&ctx, 200),
            rendered,
            "`nerd` chrome must differ from `ascii` chrome"
        );
    }

    /// Regression: the interactive footer renders `StatusLinePreset::Default`,
    /// and a default must not require a Nerd Font. The failure mode is a row of
    /// replacement boxes, whose cause is private-use codepoints (Nerd Font /
    /// Font Awesome) — so that, not "must be ASCII", is what this pins.
    #[test]
    fn non_nerd_presets_never_emit_private_use_glyphs() {
        /// Nerd Font and Font Awesome both live in a Unicode private-use area,
        /// which is exactly the set no unpatched font is required to cover.
        fn is_private_use(character: char) -> bool {
            matches!(
                u32::from(character),
                0xE000..=0xF8FF | 0xF_0000..=0xF_FFFD | 0x10_0000..=0x10_FFFD
            )
        }

        let ctx = StatusContext {
            model: "gpt-4o",
            thinking_level: Some("high"),
            mode: "act",
            cwd: "recur_agent",
            git_branch: Some("main"),
            git_dirty: true,
            context_pct: 42,
            cost_usd: 0.25,
            tokens_used: 12_500,
            subagent_count: 2,
            session_name: "alpha",
            timestamp_str: "14:02:00",
        };
        for preset in [
            StatusLinePreset::Default,
            StatusLinePreset::Minimal,
            StatusLinePreset::Compact,
            StatusLinePreset::Full,
            StatusLinePreset::Ascii,
        ] {
            let rendered = PowerlineStatusLine::with_preset(preset).render(&ctx, 200);
            assert!(
                !rendered.chars().any(is_private_use),
                "{preset:?} preset emitted a Nerd-Font glyph: {rendered:?}"
            );
        }
        // The `Ascii` preset asks for the 7-bit glyph set outright.
        let ascii = PowerlineStatusLine::with_preset(StatusLinePreset::Ascii).render(&ctx, 200);
        assert!(ascii.is_ascii(), "Ascii preset rendered {ascii:?}");
        // `Nerd` is the documented opt-in, so it keeps its icons.
        let nerd = PowerlineStatusLine::with_preset(StatusLinePreset::Nerd).render(&ctx, 200);
        assert!(
            nerd.chars().any(is_private_use),
            "Nerd preset must keep its icons: {nerd:?}"
        );
    }

    #[test]
    fn test_status_line_strips_controls_from_context_fields() {
        let ctx = StatusContext {
            model: "safe\x1b]2;bad\nmodel",
            mode: "act",
            ..StatusContext::default()
        };
        let status_line = PowerlineStatusLine::with_preset(StatusLinePreset::Minimal);
        let rendered = status_line.render(&ctx, 120);
        assert!(!rendered.contains('\x1b'));
        assert!(!rendered.contains('\n'));
        assert!(rendered.contains("safe]2;badmodel"));
    }

    #[test]
    fn test_session_accent_hue_distribution() {
        let hue1 = compute_session_accent_hue("session-alpha");
        let hue2 = compute_session_accent_hue("session-beta");
        let hue3 = compute_session_accent_hue("session-gamma");

        assert!(hue1 < 360);
        assert!(hue2 < 360);
        assert!(hue3 < 360);
        assert_ne!(hue1, hue2);
        assert_ne!(hue2, hue3);
    }
}
