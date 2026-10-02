//! `/fork` candidate selection, shared by both interactive stacks.
//!
//! The classic bubbletea tree overlay was removed; only the stack-independent
//! `ForkCandidate` model and its selection helpers remain here.

use crate::session::{Session, SessionEntry, SessionMessage};

use super::conversation::user_content_to_text;

#[derive(Debug, Clone)]
pub struct ForkCandidate {
    pub id: String,
    pub summary: String,
}

/// `/fork list`: the numbered candidates, as both stacks print them.
pub fn format_fork_candidates(candidates: &[ForkCandidate]) -> String {
    let list = candidates
        .iter()
        .enumerate()
        .map(|(i, c)| format!("  {}. {} - {}", i + 1, c.id, c.summary))
        .collect::<Vec<_>>()
        .join("\n");
    format!("Forkable user messages (use /fork <id|index>):\n{list}")
}

/// Resolve `/fork [args]` against the candidates: empty picks the latest user
/// message, a number is a 1-based index, anything else is an id or unique id
/// prefix. The error is the message to show.
pub fn select_fork_candidate(
    candidates: &[ForkCandidate],
    args: &str,
) -> Result<ForkCandidate, String> {
    let Some(last) = candidates.last() else {
        return Err("No user messages to fork from".to_string());
    };
    if args.is_empty() {
        return Ok(last.clone());
    }
    if let Ok(index) = args.parse::<usize>() {
        if index == 0 || index > candidates.len() {
            return Err(format!("Invalid index: {index} (1-{})", candidates.len()));
        }
        return Ok(candidates[index - 1].clone());
    }
    let matches = candidates
        .iter()
        .filter(|c| c.id == args || c.id.starts_with(args))
        .collect::<Vec<_>>();
    match matches.as_slice() {
        [] => Err(format!("No user message id matches \"{args}\"")),
        [only] => Ok((*only).clone()),
        many => Err(format!("Ambiguous id \"{args}\" (matches {})", many.len())),
    }
}

pub fn fork_candidates(session: &Session) -> Vec<ForkCandidate> {
    let mut out = Vec::new();

    for entry in session.entries_for_current_path() {
        let SessionEntry::Message(message_entry) = entry else {
            continue;
        };

        let Some(id) = message_entry.base.id.as_ref() else {
            continue;
        };

        let SessionMessage::User { content, .. } = &message_entry.message else {
            continue;
        };

        let text = user_content_to_text(content);
        let first_line = text
            .lines()
            .find(|line| !line.trim().is_empty())
            .unwrap_or("")
            .trim();
        let summary = if first_line.is_empty() {
            "(empty)".to_string()
        } else {
            super::truncate(first_line, 80)
        };

        out.push(ForkCandidate {
            id: id.clone(),
            summary,
        });
    }

    out
}
