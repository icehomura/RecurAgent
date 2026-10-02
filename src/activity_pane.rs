//! Activity pane: bounded tail buffering and plain-text rendering of long-task output.
//!
//! This module does not depend on a terminal or on `ftui`, and performs no IO: it only
//! keeps the most recent output lines of each long task (foreground `bash`, background
//! jobs, DAG, parallel `subagent`) and renders the visible items as fixed-width text
//! lines split into N side-by-side columns. `interactive_ftui` is responsible for
//! turning these plain-text lines into styled ftui rows.
//!
//! Widths are always measured in display cells (`unicode_width`), and a multi-byte
//! character is never split apart.

use std::collections::VecDeque;

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Maximum number of lines kept per activity item (overflow is evicted from the front
/// and counted in `dropped`).
pub const ACTIVITY_MAX_LINES: usize = 400;
/// Panel body height (rows) in the collapsed state, excluding the box borders.
pub const ACTIVITY_COLLAPSED_ROWS: u16 = 5;
/// Maximum panel body height (rows) in the expanded state, excluding the box
/// borders.
pub const ACTIVITY_EXPANDED_MAX_ROWS: u16 = 24;
/// Maximum number of side-by-side columns.
pub const ACTIVITY_MAX_COLUMNS: usize = 4;
/// How long an item stays visible after its task ends (milliseconds).
pub const ACTIVITY_LINGER_MS: u64 = 8_000;

/// Divider between columns (`│`).
const COLUMN_DIVIDER: char = '\u{2502}';
/// Trailing truncation marker.
const ELLIPSIS: char = '\u{2026}';

/// Category of an activity item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ActivityKind {
    /// Foreground `bash` command.
    Bash,
    /// Background job.
    Job,
    /// DAG scheduling.
    Dag,
    /// Parallel `subagent`.
    Subagent,
    /// Any other tool call.
    #[default]
    Tool,
}

impl ActivityKind {
    /// Short label for the category.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Bash => "bash",
            Self::Job => "job",
            Self::Dag => "dag",
            Self::Subagent => "agent",
            Self::Tool => "tool",
        }
    }
}

/// State of an activity item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ActivityState {
    /// Currently running.
    #[default]
    Running,
    /// Finished successfully.
    Done,
    /// Finished with failure.
    Failed,
}

impl ActivityState {
    /// Short label for the state.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Running => "run",
            Self::Done => "ok",
            Self::Failed => "err",
        }
    }
}

/// Activity record for one long task: a bounded tail of output lines plus metadata.
#[derive(Debug, Clone, Default)]
pub struct ActivityItem {
    /// Stable identifier (for example a tool-call id or a job id).
    pub key: String,
    /// Category.
    pub kind: ActivityKind,
    /// Display name.
    pub label: String,
    /// State.
    pub state: ActivityState,
    /// Bounded tail of output lines, oldest first.
    pub lines: VecDeque<String>,
    /// Number of lines evicted from the front by the line cap.
    pub dropped: u64,
    /// Time of the most recent update (milliseconds, monotonic clock).
    pub updated_at_ms: u64,
}

impl ActivityItem {
    /// Whether the item is still running.
    #[must_use]
    pub const fn is_running(&self) -> bool {
        matches!(self.state, ActivityState::Running)
    }

    /// The last `rows` lines, oldest first.
    #[must_use]
    pub fn tail(&self, rows: usize) -> Vec<String> {
        if rows == 0 {
            return Vec::new();
        }
        let start = self.lines.len().saturating_sub(rows);
        self.lines.iter().skip(start).cloned().collect()
    }

    /// Append a chunk of text: split on `\n`, sanitize, enqueue the lines, then trim
    /// to [`ACTIVITY_MAX_LINES`].
    pub fn push_text(&mut self, text: &str) {
        for line in sanitize_lines(text) {
            self.lines.push_back(line);
        }
        self.trim_to_cap();
    }

    /// Reset the output from a chunk of accumulated text; `dropped` is cleared as well
    /// and then recounted against the cap.
    pub fn replace_text(&mut self, text: &str) {
        self.lines.clear();
        self.dropped = 0;
        self.push_text(text);
    }

    /// Trim to the line cap, counting evicted lines in `dropped`.
    fn trim_to_cap(&mut self) {
        while self.lines.len() > ACTIVITY_MAX_LINES {
            self.lines.pop_front();
            self.dropped += 1;
        }
    }
}

/// Activity pane state: keeps all activity items in insertion order.
///
/// `Default` is an empty pane in the collapsed state (`expanded == false`).
#[derive(Debug, Clone, Default)]
pub struct ActivityPane {
    items: Vec<ActivityItem>,
    /// Whether the pane is expanded (toggled by `ctrl+x`; the caller decides how many
    /// rows that maps to).
    pub expanded: bool,
}

impl ActivityPane {
    /// Create a new empty pane (collapsed).
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// All activity items, in insertion order.
    #[must_use]
    pub fn items(&self) -> &[ActivityItem] {
        &self.items
    }

    /// Number of activity items.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.items.len()
    }

    /// Whether there are no activity items.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Look up by `key`.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<&ActivityItem> {
        self.index_of(key).map(|index| &self.items[index])
    }

    /// Upsert: create as `Running` when absent; when present, refresh only the category,
    /// name, and timestamp, leaving the accumulated output and state untouched.
    pub fn touch(
        &mut self,
        key: &str,
        kind: ActivityKind,
        label: &str,
        now_ms: u64,
    ) -> &mut ActivityItem {
        let index = match self.index_of(key) {
            Some(index) => index,
            None => {
                self.items.push(ActivityItem {
                    key: key.to_owned(),
                    ..ActivityItem::default()
                });
                self.items.len() - 1
            }
        };
        let item = &mut self.items[index];
        item.kind = kind;
        item.label.clear();
        item.label.push_str(label);
        item.updated_at_ms = now_ms;
        item
    }

    /// Mutably borrow by `key`.
    pub fn get_mut(&mut self, key: &str) -> Option<&mut ActivityItem> {
        let index = self.index_of(key)?;
        self.items.get_mut(index)
    }

    /// Update the state and timestamp (ignored when `key` is absent).
    pub fn set_state(&mut self, key: &str, state: ActivityState, now_ms: u64) {
        if let Some(item) = self.get_mut(key) {
            item.state = state;
            item.updated_at_ms = now_ms;
        }
    }

    /// Update the display name (ignored when `key` is absent).
    pub fn set_label(&mut self, key: &str, label: &str) {
        if let Some(item) = self.get_mut(key) {
            item.label.clear();
            item.label.push_str(label);
        }
    }

    /// Remove an activity item.
    pub fn remove(&mut self, key: &str) {
        self.items.retain(|item| item.key != key);
    }

    /// Whether any activity item is running.
    #[must_use]
    pub fn has_running(&self) -> bool {
        self.items.iter().any(ActivityItem::is_running)
    }

    /// Number of running activity items.
    #[must_use]
    pub fn running_count(&self) -> usize {
        self.items.iter().filter(|item| item.is_running()).count()
    }

    /// Drop items that ended more than [`ACTIVITY_LINGER_MS`] ago; returns whether any
    /// running item remains, which tells the caller whether a tick chain is still needed.
    pub fn retain_visible(&mut self, now_ms: u64) -> bool {
        self.items.retain(|item| {
            item.is_running() || now_ms.saturating_sub(item.updated_at_ms) < ACTIVITY_LINGER_MS
        });
        self.has_running()
    }

    /// Items to draw: running items first (when running subagents exist, the subagent
    /// group comes first), otherwise recently finished items still inside the linger
    /// window (oldest first, newest last). Never truncated.
    #[must_use]
    pub fn display(&self, now_ms: u64) -> Vec<&ActivityItem> {
        let running: Vec<&ActivityItem> =
            self.items.iter().filter(|item| item.is_running()).collect();
        if running.is_empty() {
            let mut done: Vec<&ActivityItem> = self
                .items
                .iter()
                .filter(|item| {
                    !item.is_running()
                        && now_ms.saturating_sub(item.updated_at_ms) < ACTIVITY_LINGER_MS
                })
                .collect();
            done.sort_by_key(|item| item.updated_at_ms);
            return done;
        }
        if running
            .iter()
            .any(|item| item.kind == ActivityKind::Subagent)
        {
            let mut ordered: Vec<&ActivityItem> = running
                .iter()
                .copied()
                .filter(|item| item.kind == ActivityKind::Subagent)
                .collect();
            ordered.extend(
                running
                    .iter()
                    .copied()
                    .filter(|item| item.kind != ActivityKind::Subagent),
            );
            ordered
        } else {
            running
        }
    }

    /// Single-line pane header, for example `activity · 2 running · ctrl+x expand`; when
    /// running items exceed [`ACTIVITY_MAX_COLUMNS`], the number of columns actually shown
    /// is appended.
    #[must_use]
    pub fn header(&self, expanded: bool) -> String {
        let running = self.running_count();
        let mut out = String::from("activity · ");
        if running == 0 {
            out.push_str("idle");
        } else {
            out.push_str(&format!("{running} running"));
        }
        if running > ACTIVITY_MAX_COLUMNS {
            out.push_str(&format!(" · {ACTIVITY_MAX_COLUMNS} shown"));
        }
        out.push_str(if expanded {
            " · ctrl+x collapse"
        } else {
            " · ctrl+x expand"
        });
        out
    }

    /// Split `width` display cells into `n` columns with 1 divider cell between them.
    /// `n` is first clamped to `1..=ACTIVITY_MAX_COLUMNS`; when the width cannot fit that
    /// many, the column count is reduced further while keeping at least 1 cell per
    /// column. Returns `(start x, width w)` for each column such that
    /// `sum(w) + (n-1) == width`.
    #[must_use]
    pub fn columns(width: usize, n: usize) -> Vec<(usize, usize)> {
        let mut count = n.clamp(1, ACTIVITY_MAX_COLUMNS);
        // Each column needs at least 1 cell, plus n-1 divider cells, so the width
        // must satisfy 2*count - 1 <= width.
        while count > 1 && width < 2 * count - 1 {
            count -= 1;
        }
        let available = width.saturating_sub(count - 1);
        let base = available / count;
        let extra = available % count;
        let mut out = Vec::with_capacity(count);
        let mut x = 0;
        for i in 0..count {
            let w = base + usize::from(i < extra);
            out.push((x, w));
            x += w + 1;
        }
        out
    }

    /// Render one column: row 0 is `[run] <label>` (with ` +<dropped>` appended when
    /// lines were dropped) and the rest are tail output lines indented by two spaces.
    /// Exactly `rows` rows, each exactly `width` display cells (truncated with a
    /// trailing `…`, padded with spaces when shorter).
    #[must_use]
    pub fn render_column(item: &ActivityItem, width: usize, rows: usize) -> Vec<String> {
        let mut out = Vec::with_capacity(rows);
        if rows == 0 {
            return out;
        }
        let state = item.state.as_str();
        let label = item.label.as_str();
        let dropped = item.dropped;
        let mut head = format!("[{state}] {label}");
        if dropped > 0 {
            head.push_str(&format!(" +{dropped}"));
        }
        out.push(fit_cells(&head, width));
        for line in item.tail(rows - 1) {
            let mut text = String::with_capacity(line.len() + 2);
            text.push_str("  ");
            text.push_str(&line);
            out.push(fit_cells(&text, width));
        }
        while out.len() < rows {
            out.push(fit_cells("", width));
        }
        out
    }

    /// Pane body: lays out the items from [`ActivityPane::display`] side by side in
    /// columns joined by `│`. Exactly `rows` rows, each exactly `width` display cells;
    /// with no visible items the first row is `no active tasks` and the rest are blank.
    /// Returns an empty `Vec` when `rows == 0 || width == 0`.
    #[must_use]
    pub fn render(&self, width: usize, rows: usize, now_ms: u64) -> Vec<String> {
        if rows == 0 || width == 0 {
            return Vec::new();
        }
        let visible = self.display(now_ms);
        if visible.is_empty() {
            let mut out = vec![fit_cells("no active tasks", width)];
            while out.len() < rows {
                out.push(fit_cells("", width));
            }
            return out;
        }
        let count = visible.len().min(ACTIVITY_MAX_COLUMNS);
        let geometry = Self::columns(width, count);
        let rendered: Vec<Vec<String>> = visible
            .iter()
            .zip(geometry.iter())
            .map(|(item, &(_, w))| Self::render_column(item, w, rows))
            .collect();
        let mut out = vec![String::with_capacity(width); rows];
        for (column_index, column) in rendered.iter().enumerate() {
            for (row, text) in column.iter().enumerate() {
                if column_index > 0 {
                    out[row].push(COLUMN_DIVIDER);
                }
                out[row].push_str(text);
            }
        }
        out
    }

    /// Rounded box around the pane: the header sits in the top border, the
    /// body columns sit between `│` side borders, and a bottom border closes
    /// the box. Exactly `rows` lines of exactly `width` cells; falls back to
    /// [`ActivityPane::render`] when there is no room for a box.
    #[must_use]
    pub fn render_boxed(&self, width: usize, rows: usize, now_ms: u64) -> Vec<String> {
        const MIN_BOX_ROWS: usize = 3;
        if rows < MIN_BOX_ROWS || width < 4 {
            return self.render(width, rows, now_ms);
        }
        let inner = width - 2;
        let head = clip_cells(&self.header(self.expanded), inner.saturating_sub(4).max(1));
        let head_width = UnicodeWidthStr::width(head.as_str());
        let fill = inner.saturating_sub(head_width + 3);
        let mut top = String::with_capacity(width);
        top.push_str("╭─ ");
        top.push_str(&head);
        top.push(' ');
        for _ in 0..fill {
            top.push('─');
        }
        top.push('╮');

        let mut out = Vec::with_capacity(rows);
        out.push(fit_cells(&top, width));
        for line in self.render(inner, rows - 2, now_ms) {
            out.push(format!("│{line}│"));
        }
        let mut bottom = String::with_capacity(width);
        bottom.push('╰');
        for _ in 0..inner {
            bottom.push('─');
        }
        bottom.push('╯');
        out.push(bottom);
        out
    }

    /// Index of `key`.
    fn index_of(&self, key: &str) -> Option<usize> {
        self.items.iter().position(|item| item.key == key)
    }
}

/// Sanitize a chunk of text into lines: expand `\t` to 4 spaces, drop `\r` and the
/// remaining C0 control characters, split on `\n`, and discard the empty trailing line
/// produced by a final `\n`.
fn sanitize_lines(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    if text.is_empty() {
        return out;
    }
    let mut current = String::new();
    for ch in text.chars() {
        match ch {
            '\n' => out.push(std::mem::take(&mut current)),
            '\t' => current.push_str("    "),
            '\u{0}'..='\u{1f}' => {}
            _ => current.push(ch),
        }
    }
    if !text.ends_with('\n') {
        out.push(current);
    }
    out
}

/// Truncate to at most `max` cells, marking a cut with `…`, without padding.
/// The returned value never exceeds `max` display cells.
fn clip_cells(text: &str, max: usize) -> String {
    if UnicodeWidthStr::width(text) <= max {
        return text.to_string();
    }
    fit_cells(text, max)
}

/// Truncate to `width` cells (ending with `…`) or pad with spaces on the right; the
/// returned value always has a display width of exactly `width`.
fn fit_cells(text: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let used = UnicodeWidthStr::width(text);
    if used <= width {
        let mut out = String::with_capacity(text.len() + (width - used));
        out.push_str(text);
        push_spaces(&mut out, width - used);
        return out;
    }
    let budget = width - 1;
    let mut out = String::with_capacity(text.len());
    let mut filled = 0;
    for ch in text.chars() {
        let char_width = UnicodeWidthChar::width(ch).unwrap_or(0);
        if filled + char_width > budget {
            break;
        }
        out.push(ch);
        filled += char_width;
    }
    out.push(ELLIPSIS);
    push_spaces(&mut out, budget - filled);
    out
}

/// Append spaces on the right.
fn push_spaces(out: &mut String, count: usize) {
    out.extend(std::iter::repeat_n(' ', count));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn display_keys(pane: &ActivityPane, now_ms: u64) -> Vec<String> {
        pane.display(now_ms)
            .iter()
            .map(|item| item.key.clone())
            .collect()
    }

    fn width_of(line: &str) -> usize {
        UnicodeWidthStr::width(line)
    }

    #[test]
    fn zz_scratch_measure_columns() {
        for width in [7usize, 10, 20, 40, 120] {
            for count in [1usize, 4, 8] {
                let mut pane = ActivityPane::new();
                for i in 0..count {
                    let key = format!("k{i}");
                    let item = pane.touch(&key, ActivityKind::Subagent, &format!("agent #{i}"), 0);
                    item.push_text(&format!(
                        "alpha{i} beta gamma delta epsilon zeta eta theta\nsecond line of output\n"
                    ));
                }
                let shown = count.min(ACTIVITY_MAX_COLUMNS);
                let geometry = ActivityPane::columns(width, shown);
                let cols: Vec<usize> = geometry.iter().map(|&(_, w)| w).collect();
                let body = pane.render(width, 5, 0);
                let heads: Vec<String> = pane
                    .display(0)
                    .iter()
                    .zip(geometry.iter())
                    .map(|(item, &(_, w))| {
                        ActivityPane::render_column(item, w, 5)[0].trim_end().to_string()
                    })
                    .collect();
                eprintln!(
                    "PANE width={width} items={count} cols={cols:?} row0={:?} heads={:?}",
                    body[0].trim_end(),
                    heads
                );
            }
        }
    }

    #[test]
    fn line_cap_keeps_tail_and_counts_dropped() {
        let mut item = ActivityItem::default();
        let total = ACTIVITY_MAX_LINES + 3;
        for i in 0..total {
            item.push_text(&format!("line {i}\n"));
        }
        assert_eq!(item.lines.len(), ACTIVITY_MAX_LINES);
        assert_eq!(item.dropped, 3);
        assert_eq!(item.lines.front().map(String::as_str), Some("line 3"));
        assert_eq!(
            item.lines.back().map(String::as_str),
            Some(format!("line {}", total - 1).as_str())
        );
        assert_eq!(
            item.tail(2),
            vec!["line 401".to_string(), "line 402".to_string()]
        );
    }

    #[test]
    fn replace_text_resets_lines_and_dropped() {
        let mut item = ActivityItem::default();
        let mut big = String::new();
        for i in 0..ACTIVITY_MAX_LINES + 5 {
            big.push_str(&format!("old {i}\n"));
        }
        item.replace_text(&big);
        assert_eq!(item.lines.len(), ACTIVITY_MAX_LINES);
        assert_eq!(item.dropped, 5);

        item.replace_text("fresh\nlines\n");
        assert_eq!(item.lines.len(), 2);
        assert_eq!(item.dropped, 0);
        assert_eq!(item.tail(9), vec!["fresh".to_string(), "lines".to_string()]);
    }

    #[test]
    fn touch_creates_then_refreshes_without_resetting_output() {
        let mut pane = ActivityPane::new();
        pane.touch("b1", ActivityKind::Bash, "cargo test", 100);
        pane.get_mut("b1").unwrap().push_text("compiling\n");
        pane.set_state("b1", ActivityState::Done, 200);

        let item = pane.touch("b1", ActivityKind::Job, "relabeled", 300);
        assert_eq!(item.kind, ActivityKind::Job);
        assert_eq!(item.label, "relabeled");
        assert_eq!(item.updated_at_ms, 300);
        assert_eq!(item.state, ActivityState::Done);
        assert_eq!(item.lines.front().map(String::as_str), Some("compiling"));
        assert_eq!(pane.len(), 1);

        let fresh = pane.touch("j9", ActivityKind::Job, "job 9", 400);
        assert_eq!(fresh.state, ActivityState::Running);
        assert_eq!(fresh.kind, ActivityKind::Job);
        assert!(fresh.lines.is_empty());
        assert_eq!(pane.len(), 2);
    }

    #[test]
    fn display_prefers_running_then_linger_window() {
        let mut pane = ActivityPane::new();
        pane.touch("first", ActivityKind::Tool, "first", 0);
        pane.set_state("first", ActivityState::Done, 1_000);
        pane.touch("second", ActivityKind::Tool, "second", 0);
        pane.set_state("second", ActivityState::Done, 2_000);

        // No running items: within the linger window, ordered oldest to newest by update time
        assert_eq!(
            display_keys(&pane, 2_000),
            vec!["first".to_string(), "second".to_string()]
        );

        // Running items appear: only running items are shown
        pane.touch("live", ActivityKind::Bash, "live", 2_500);
        assert_eq!(display_keys(&pane, 2_500), vec!["live".to_string()]);

        // Finished items past the linger window are filtered out; running items remain
        assert_eq!(
            display_keys(&pane, 2_500 + ACTIVITY_LINGER_MS),
            vec!["live".to_string()]
        );

        // Cleared once the running item ends and falls outside the window
        pane.set_state("live", ActivityState::Done, 20_000);
        assert!(display_keys(&pane, 20_000 + ACTIVITY_LINGER_MS).is_empty());
    }

    #[test]
    fn display_groups_running_subagents_first() {
        let mut pane = ActivityPane::new();
        pane.touch("bash1", ActivityKind::Bash, "bash", 0);
        pane.touch("agent1", ActivityKind::Subagent, "agent", 0);
        pane.touch("agent2", ActivityKind::Subagent, "agent", 0);
        assert_eq!(
            display_keys(&pane, 0),
            vec![
                "agent1".to_string(),
                "agent2".to_string(),
                "bash1".to_string()
            ]
        );
    }

    #[test]
    fn set_state_remove_and_retain_visible() {
        let mut pane = ActivityPane::new();
        pane.touch("a", ActivityKind::Tool, "a", 10);
        pane.touch("b", ActivityKind::Job, "b", 20);
        pane.set_state("a", ActivityState::Failed, 30);
        assert_eq!(pane.get("a").unwrap().state, ActivityState::Failed);
        assert_eq!(pane.get("a").unwrap().updated_at_ms, 30);
        assert!(pane.has_running());
        assert_eq!(pane.running_count(), 1);

        pane.set_label("b", "job b");
        assert_eq!(pane.get("b").unwrap().label, "job b");

        pane.remove("b");
        assert_eq!(pane.len(), 1);
        assert!(pane.get("b").is_none());

        // Operations on a missing key are no-ops
        pane.set_state("missing", ActivityState::Done, 40);
        pane.set_label("missing", "x");
        assert_eq!(pane.len(), 1);

        pane.set_state("a", ActivityState::Done, 100);
        assert!(!pane.has_running());
        assert!(!pane.retain_visible(100 + ACTIVITY_LINGER_MS));
        assert!(pane.is_empty());
    }

    #[test]
    fn columns_reserve_dividers_and_clamp() {
        assert_eq!(ActivityPane::columns(0, 3), vec![(0, 0)]);
        assert_eq!(ActivityPane::columns(20, 0), vec![(0, 20)]);
        assert_eq!(ActivityPane::columns(20, 9).len(), ACTIVITY_MAX_COLUMNS);

        let cols = ActivityPane::columns(20, 4);
        assert_eq!(cols.len(), 4);
        assert_eq!(cols[0].0, 0);
        for pair in cols.windows(2) {
            assert_eq!(pair[0].0 + pair[0].1 + 1, pair[1].0);
        }
        let total: usize = cols.iter().map(|&(_, w)| w).sum::<usize>() + (cols.len() - 1);
        assert_eq!(total, 20);

        // Too narrow for n columns: the count shrinks, yet the width is still exactly filled
        let tight = ActivityPane::columns(3, 4);
        assert_eq!(tight.len(), 2);
        let total: usize = tight.iter().map(|&(_, w)| w).sum::<usize>() + (tight.len() - 1);
        assert_eq!(total, 3);
    }

    #[test]
    fn render_column_is_exact_width_and_rows() {
        let mut item = ActivityItem::default();
        item.label = "编译中 compile".to_string();
        item.push_text("alpha\nbeta\n");
        item.dropped = 4;

        let lines = ActivityPane::render_column(&item, 30, 4);
        assert_eq!(lines.len(), 4);
        for line in &lines {
            assert_eq!(width_of(line), 30, "{line:?}");
        }
        assert!(lines[0].starts_with("[run] "), "{:?}", lines[0]);
        assert!(lines[0].trim_end().ends_with(" +4"), "{:?}", lines[0]);
        assert!(lines[1].starts_with("  alpha"), "{:?}", lines[1]);
        assert!(lines[2].starts_with("  beta"), "{:?}", lines[2]);
        assert!(lines[3].trim().is_empty(), "{:?}", lines[3]);

        // A multi-byte label cut short ends with the ellipsis and is not split
        let cut = ActivityPane::render_column(&item, 6, 2);
        assert_eq!(cut.len(), 2);
        for line in &cut {
            assert_eq!(width_of(line), 6, "{line:?}");
        }
        assert!(cut[0].ends_with(ELLIPSIS), "{:?}", cut[0]);

        // Zero rows returns empty; zero width still returns `rows` empty strings
        assert!(ActivityPane::render_column(&item, 10, 0).is_empty());
        let zero = ActivityPane::render_column(&item, 0, 3);
        assert_eq!(zero.len(), 3);
        assert!(zero.iter().all(String::is_empty));
    }

    #[test]
    fn render_joins_two_columns_with_divider() {
        let mut pane = ActivityPane::new();
        pane.touch("a", ActivityKind::Bash, "one", 0)
            .push_text("aa\n");
        pane.touch("b", ActivityKind::Job, "two", 0)
            .push_text("bb\n");

        let lines = pane.render(21, 3, 0);
        assert_eq!(lines.len(), 3);
        for line in &lines {
            assert_eq!(width_of(line), 21, "{line:?}");
            assert_eq!(line.matches(COLUMN_DIVIDER).count(), 1, "{line:?}");
        }
        let head = &lines[0];
        let (left, right) = head.split_once(COLUMN_DIVIDER).unwrap();
        assert!(left.trim_end().ends_with("[run] one"), "{head:?}");
        assert!(right.trim().starts_with("[run] two"), "{head:?}");
        assert!(lines[1].starts_with("  aa"), "{:?}", lines[1]);
        let (_, body_right) = lines[1].split_once(COLUMN_DIVIDER).unwrap();
        assert!(body_right.starts_with("  bb"), "{:?}", lines[1]);
    }

    #[test]
    fn render_boxed_wraps_the_pane_in_a_rounded_box() {
        let mut pane = ActivityPane::new();
        pane.touch("b1", ActivityKind::Bash, "bash: cargo test", 0)
            .push_text("compiling\nrunning 3 tests\n");

        let lines = pane.render_boxed(60, 5, 0);
        assert_eq!(lines.len(), 5);
        for line in &lines {
            assert_eq!(width_of(line), 60, "{line:?}");
        }
        assert!(lines[0].starts_with("╭─ activity"), "{:?}", lines[0]);
        assert!(lines[0].ends_with('╮'), "{:?}", lines[0]);
        assert!(lines[0].contains("ctrl+x expand"), "{:?}", lines[0]);
        for line in &lines[1..4] {
            assert!(line.starts_with('│') && line.ends_with('│'), "{line:?}");
        }
        assert!(lines[1].contains("bash: cargo test"), "{:?}", lines[1]);
        assert!(lines[2].contains("compiling"), "{:?}", lines[2]);
        assert!(
            lines[4].starts_with('╰') && lines[4].ends_with('╯'),
            "{:?}",
            lines[4]
        );

        // A narrow box clips the header instead of overflowing the border.
        let narrow = pane.render_boxed(30, 3, 0);
        assert_eq!(narrow.len(), 3);
        for line in &narrow {
            assert_eq!(width_of(line), 30, "{line:?}");
        }
        assert!(narrow[0].starts_with("╭─ activity"), "{:?}", narrow[0]);

        // Too few rows (or cells) for corners: the plain body comes back.
        assert_eq!(pane.render_boxed(60, 2, 0), pane.render(60, 2, 0));
        assert_eq!(pane.render_boxed(3, 5, 0), pane.render(3, 5, 0));
    }

    #[test]
    fn render_reports_no_active_tasks_and_empty_sizes() {
        let pane = ActivityPane::new();
        let lines = pane.render(20, 3, 0);
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0].trim_end(), "no active tasks");
        for line in &lines {
            assert_eq!(width_of(line), 20, "{line:?}");
        }
        assert!(lines[1..].iter().all(|line| line.trim().is_empty()));

        assert!(pane.render(0, 5, 0).is_empty());
        assert!(pane.render(20, 0, 0).is_empty());
    }

    #[test]
    fn header_summarizes_running_counts() {
        let mut pane = ActivityPane::new();
        assert_eq!(pane.header(false), "activity · idle · ctrl+x expand");
        for i in 0..5 {
            pane.touch(&format!("k{i}"), ActivityKind::Subagent, "s", 0);
        }
        assert_eq!(
            pane.header(false),
            "activity · 5 running · 4 shown · ctrl+x expand"
        );
        assert_eq!(
            pane.header(true),
            "activity · 5 running · 4 shown · ctrl+x collapse"
        );
    }

    #[test]
    fn text_is_sanitized_on_push() {
        let mut item = ActivityItem::default();
        item.push_text("a\tb\r\nc\u{1}d\n");
        assert_eq!(item.lines.len(), 2);
        assert_eq!(item.lines[0], "a    b");
        assert_eq!(item.lines[1], "cd");
    }
}
