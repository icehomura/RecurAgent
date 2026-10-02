//! Terminal-bound text sanitization shared by both interactive stacks.
//!
//! Extracted from the classic `tool_render` module when the bubbletea stack was
//! removed: the ftui stack reaches `sanitize_terminal_text` through the shared
//! conversation/state types, so the filter cannot live with the classic
//! renderers.

use std::borrow::Cow;

/// Strip ANSI escape sequences and non-printing control characters from
/// terminal-bound tool output (bd-p45xh). Commands that emit color codes,
/// cursor movement, alt-screen switches, or `\r`-rewritten progress bars
/// would otherwise be painted straight into the transcript and corrupt the
/// frame. `\n` and `\t` survive; CRLF collapses to LF; a bare CR (progress
/// frame rewrite) becomes LF so successive frames stay readable.
pub(super) fn sanitize_terminal_text(input: &str) -> Cow<'_, str> {
    let needs_work = input
        .bytes()
        .any(|b| b == 0x1b || b == 0x7f || (b < 0x20 && b != b'\n' && b != b'\t'))
        || input
            .chars()
            .any(|ch| ('\u{0080}'..='\u{009f}').contains(&ch));
    if !needs_work {
        return Cow::Borrowed(input);
    }

    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\u{1b}' => match chars.peek() {
                // CSI: ESC '[' params/intermediates then a final byte @..~.
                Some('[') => {
                    chars.next();
                    while let Some(&c) = chars.peek() {
                        chars.next();
                        if ('\u{40}'..='\u{7e}').contains(&c) {
                            break;
                        }
                    }
                }
                // String-payload sequences whose body must be consumed too:
                // OSC (ESC ']'), DCS (ESC 'P', e.g. sixel), SOS (ESC 'X'),
                // PM (ESC '^'), APC (ESC '_', e.g. tmux passthrough).
                // Terminated by ST (ESC '\'); OSC also accepts BEL.
                Some(']' | 'P' | 'X' | '^' | '_') => {
                    let accepts_bel = chars.peek() == Some(&']');
                    chars.next();
                    while let Some(c) = chars.next() {
                        if accepts_bel && c == '\u{07}' {
                            break;
                        }
                        if c == '\u{009c}' {
                            break;
                        }
                        if c == '\u{1b}' {
                            if chars.peek() == Some(&'\\') {
                                chars.next();
                            }
                            break;
                        }
                    }
                }
                // Two-character escape (ESC c, ESC 7, ...) or dangling ESC.
                Some(_) => {
                    chars.next();
                }
                None => {}
            },
            // Single-codepoint C1 CSI.
            '\u{009b}' => {
                while let Some(&c) = chars.peek() {
                    chars.next();
                    if ('\u{40}'..='\u{7e}').contains(&c) {
                        break;
                    }
                }
            }
            // Single-codepoint C1 string controls: DCS, SOS, OSC, PM, APC.
            c @ ('\u{0090}' | '\u{0098}' | '\u{009d}' | '\u{009e}' | '\u{009f}') => {
                let accepts_bel = c == '\u{009d}';
                while let Some(c) = chars.next() {
                    if c == '\u{009c}' || (accepts_bel && c == '\u{07}') {
                        break;
                    }
                    if c == '\u{1b}' && chars.peek() == Some(&'\\') {
                        chars.next();
                        break;
                    }
                }
            }
            '\r' => {
                if chars.peek() != Some(&'\n') {
                    out.push('\n');
                }
            }
            c if ('\u{0080}'..='\u{009f}').contains(&c) => {}
            c if c == '\u{7f}' || (c < '\u{20}' && c != '\n' && c != '\t') => {}
            c => out.push(c),
        }
    }
    Cow::Owned(out)
}

/// Sanitize a terminal-bound value that must remain on one visual line.
///
/// This applies the full escape/control filter above, then replaces the
/// otherwise-preserved newline and tab characters with spaces. Use it for
/// identities, titles, and option labels; use [`sanitize_terminal_text`] for
/// deliberately multiline transcript content.
pub(super) fn sanitize_terminal_line(input: &str) -> Cow<'_, str> {
    let sanitized = sanitize_terminal_text(input);
    if !sanitized.chars().any(|ch| matches!(ch, '\n' | '\t')) {
        return sanitized;
    }

    Cow::Owned(
        sanitized
            .chars()
            .map(|ch| if matches!(ch, '\n' | '\t') { ' ' } else { ch })
            .collect(),
    )
}
