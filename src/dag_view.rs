//! DAG 分层居中渲染：开始/结束居中，层内居中，扇出/扇入用直角连线汇合。

// Layout math is integer/column arithmetic over u32 node ids and usize display
// columns; the pedantic cast/shape lints below are noise for this module. The
// crate runs pedantic+nursery under `-D warnings`, so they are allowed here in
// one documented place rather than sprinkled at each call site.
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap,
    clippy::many_single_char_names,
    clippy::similar_names,
    clippy::too_many_lines,
    clippy::needless_range_loop,
    clippy::struct_excessive_bools
)]

use std::collections::{HashMap, VecDeque};

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

pub const START_LABEL: &str = "开始";
pub const END_LABEL: &str = "结束";

const H_GAP: usize = 3;
const LAYER_PITCH: usize = 5; // 3 行方框 + 2 行连线带（合并 / 分叉）

/// 框宽：内容显示宽 + 左右各 1 内边距 + 左右框线 = 内容宽 + 4。
/// 不强制奇数——锚点取 `x + (w-1)/2` 已是唯一整数中心列，奇偶不影响对齐。
fn box_width(label: &str) -> usize {
    display_width(label) + 4
}
const MAX_VIEW_NODES: usize = 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DagViewState {
    Pending,
    Running,
    Succeeded,
    Failed,
    Skipped,
}

impl DagViewState {
    #[must_use]
    pub const fn rgb(self) -> (u8, u8, u8) {
        match self {
            Self::Pending => (130, 137, 151),
            Self::Running => (229, 192, 123),
            Self::Failed => (220, 80, 80),
            Self::Succeeded => (96, 196, 116),
            Self::Skipped => (110, 116, 128),
        }
    }

    /// 标记：标准 Unicode（对勾 / 叉号）；运行中取盲文转圈帧。
    #[must_use]
    pub fn marker(self, frame: usize) -> String {
        match self {
            Self::Pending => "[ ]".to_string(),
            Self::Running => format!("[{}]", SPINNER_FRAMES[frame % SPINNER_FRAMES.len()]),
            Self::Succeeded => "[✓]".to_string(),
            Self::Failed => "[✗]".to_string(),
            Self::Skipped => "[-]".to_string(),
        }
    }
}

/// Docker 风格盲文转圈（10 帧）；TUI 每帧递增 `frame` 即可动起来。
pub const SPINNER_FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

#[derive(Debug, Clone)]
pub struct DagViewNode {
    pub id: u32,
    /// AI 起的语义名（中文亦可），图里显示这个；空则退回工具名。
    pub name: String,
    pub tool_name: String,
    pub depends_on: Vec<u32>,
    pub state: DagViewState,
    pub output: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DagViewCellState {
    Neutral,
    Root,
    Node(DagViewState),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DagViewCell {
    pub text: String,
    pub state: DagViewCellState,
}

#[must_use]
pub fn render(nodes: &[DagViewNode]) -> Vec<Vec<DagViewCell>> {
    render_with_frame(nodes, 0)
}

/// 与 [`render`] 相同，但指定运行中节点的盲文转圈帧（TUI 每帧传入递增值）。
#[must_use]
pub fn render_with_frame(nodes: &[DagViewNode], frame: usize) -> Vec<Vec<DagViewCell>> {
    render_layout(nodes, frame, 10, H_GAP)
}

/// 宽度感知渲染：自然宽度超过 `max_width` 时，先缩节点间距、再逐级缩短框内
/// 名字（4 列下限），尽量塞进终端宽度；仍塞不下则按最紧配置输出，交给调用方裁剪。
#[must_use]
pub fn render_fitted(
    nodes: &[DagViewNode],
    frame: usize,
    max_width: usize,
) -> Vec<Vec<DagViewCell>> {
    for gap in [H_GAP, 1] {
        for cap in (4..=10usize).rev() {
            let rows = render_layout(nodes, frame, cap, gap);
            if rows.iter().map(|r| width_of(r)).max().unwrap_or(0) <= max_width {
                return rows;
            }
        }
    }
    render_layout(nodes, frame, 4, 1)
}

fn width_of(row: &[DagViewCell]) -> usize {
    row.iter().map(|c| display_width(&c.text)).sum()
}

fn width_of_rows(rows: &[Vec<DagViewCell>]) -> usize {
    rows.iter().map(|r| width_of(r)).max().unwrap_or(0)
}

#[must_use]
pub fn render_to_string(nodes: &[DagViewNode]) -> Vec<String> {
    render(nodes)
        .into_iter()
        .map(|row| row.into_iter().map(|c| c.text).collect())
        .collect()
}

// ---------------------------------------------------------------------------
// line drawing bits
// ---------------------------------------------------------------------------

const U: u8 = 1;
const D: u8 = 2;
const L: u8 = 4;
const R: u8 = 8;

#[derive(Clone, Copy)]
struct Cell {
    conn: u8,
    lit: char,
    has_lit: bool,
    skip: bool,
    state: DagViewCellState,
}

impl Cell {
    fn empty() -> Self {
        Self {
            conn: 0,
            lit: ' ',
            has_lit: false,
            skip: false,
            state: DagViewCellState::Neutral,
        }
    }
}

fn glyph(conn: u8) -> char {
    match conn {
        0 => ' ',
        1 | 2 | 3 => '│',
        4 | 8 | 12 => '─',
        5 => '┘',
        6 => '┐',
        7 => '┤',
        9 => '└',
        10 => '┌',
        11 => '├',
        13 => '┴',
        14 => '┬',
        15 => '┼',
        _ => '·',
    }
}

#[derive(Clone)]
struct LNode {
    label: String,
    width: usize,
    layer: usize,
    x: usize,
    state: DagViewCellState,
    is_dummy: bool,
    key: usize,
}

fn display_width(s: &str) -> usize {
    UnicodeWidthStr::width(s)
}

/// 图内名称：显示宽度 ≤ `cap` 列原样保留；更长则截到 `cap-1` 列补 `…`（全称见下方列表）。
fn short_name(tool: &str, cap: usize) -> String {
    let cap = cap.max(2);
    if UnicodeWidthStr::width(tool) <= cap {
        return tool.to_string();
    }
    let mut out = String::new();
    let mut w = 0usize;
    for ch in tool.chars() {
        let cw = UnicodeWidthChar::width(ch).unwrap_or(0);
        if w + cw > cap - 1 {
            break;
        }
        out.push(ch);
        w += cw;
    }
    out.push('…');
    out
}

struct Prepared {
    ln: Vec<LNode>,
    layers: Vec<Vec<usize>>,
    edges: Vec<(usize, usize)>,
    preds: Vec<Vec<usize>>,
}

fn render_layout(
    src: &[DagViewNode],
    frame: usize,
    cap: usize,
    gap: usize,
) -> Vec<Vec<DagViewCell>> {
    let truncated = src.len() > MAX_VIEW_NODES;
    let src = &src[..src.len().min(MAX_VIEW_NODES)];
    let Prepared {
        mut ln,
        layers,
        edges,
        preds,
    } = prepare(src, frame, cap);
    let max_layer = ln.iter().map(|x| x.layer).max().unwrap_or(0);

    // --- x positions: align each node under its parents so single-parent
    // chains become one straight vertical line instead of a 1-cell jog ---
    let anchors = place_anchors(
        &layers,
        &preds,
        &ln.iter().map(|x| x.width as i64).collect::<Vec<_>>(),
        gap,
    );
    let mut min_left = i64::MAX;
    for (i, node) in ln.iter().enumerate() {
        let half = (node.width as i64 - 1) / 2;
        min_left = min_left.min(anchors[i] - half);
    }
    for (i, node) in ln.iter_mut().enumerate() {
        let half = (node.width as i64 - 1) / 2;
        node.x = (anchors[i] - half - min_left) as usize;
    }
    let canvas_w = ln.iter().map(|n| n.x + n.width).max().unwrap_or(1).max(1);

    // --- grid ---
    let height = max_layer * LAYER_PITCH + 3;
    let mut grid: Vec<Vec<Cell>> = vec![vec![Cell::empty(); canvas_w]; height];

    for node in ln.iter().filter(|x| !x.is_dummy) {
        let y = node.layer * LAYER_PITCH;
        place_box(&mut grid, node.x, y, node.width, &node.label, node.state);
    }
    for node in ln.iter().filter(|x| x.is_dummy) {
        let y = node.layer * LAYER_PITCH;
        for dy in 0..3 {
            add_conn(&mut grid, node.x, y + dy, U | D, DagViewCellState::Neutral);
        }
    }
    // One shared central trunk ("竖向干线") per layer boundary: parents merge
    // into it, then it drops and splits to the children.
    let mut trunk = vec![0i64; layers.len()];
    for (l, t) in trunk.iter_mut().enumerate() {
        let mut parents: Vec<i64> = Vec::new();
        let mut children: Vec<i64> = Vec::new();
        for &(a, b) in &edges {
            if ln[a].layer == l && !parents.contains(&anchors[a]) {
                parents.push(anchors[a]);
            }
            if ln[b].layer == l + 1 && !children.contains(&anchors[b]) {
                children.push(anchors[b]);
            }
        }
        *t = if parents.len() == 1 {
            parents[0]
        } else if children.len() == 1 {
            children[0]
        } else if parents.is_empty() || children.is_empty() {
            0
        } else {
            let lo = parents
                .iter()
                .chain(children.iter())
                .copied()
                .min()
                .unwrap();
            let hi = parents
                .iter()
                .chain(children.iter())
                .copied()
                .max()
                .unwrap();
            (lo + hi).div_euclid(2)
        };
    }
    for &(a, b) in &edges {
        let xt = (trunk[ln[a].layer] - min_left) as usize;
        connect(&mut grid, &ln, a, b, xt);
    }

    let mut rows: Vec<Vec<DagViewCell>> = grid.iter().map(|row| rle(row)).collect();
    if !src.is_empty() {
        rows.push(Vec::new());
        for node in src {
            let text = if node.name.is_empty() {
                node.tool_name.clone()
            } else {
                format!("{} ({})", node.name, node.tool_name)
            };
            rows.push(vec![DagViewCell {
                text,
                state: DagViewCellState::Neutral,
            }]);
        }
    }
    if truncated {
        let omitted = src.len().saturating_sub(MAX_VIEW_NODES);
        rows.push(vec![DagViewCell {
            text: format!("… 其余 {omitted} 个节点已省略"),
            state: DagViewCellState::Neutral,
        }]);
    }
    rows
}

fn compute_layers(n: usize, deps: &[Vec<usize>], dependents: &[Vec<usize>]) -> Vec<usize> {
    let mut indeg: Vec<usize> = deps.iter().map(Vec::len).collect();
    let mut layer = vec![0usize; n];
    let mut settled = vec![false; n];
    let mut queue: VecDeque<usize> = (0..n).filter(|&i| indeg[i] == 0).collect();
    while let Some(u) = queue.pop_front() {
        settled[u] = true;
        for &v in &dependents[u] {
            layer[v] = layer[v].max(layer[u] + 1);
            indeg[v] -= 1;
            if indeg[v] == 0 {
                queue.push_back(v);
            }
        }
    }
    let mut maxl = layer.iter().copied().max().unwrap_or(0);
    for i in 0..n {
        if !settled[i] {
            maxl += 1;
            layer[i] = maxl;
        }
    }
    layer
}

fn barycenter(layers: &mut [Vec<usize>], preds: &[Vec<usize>], succs: &[Vec<usize>]) {
    for _ in 0..4 {
        for l in 1..layers.len() {
            let scores: Vec<f64> = layers[l]
                .iter()
                .enumerate()
                .map(|(pos, &id)| barycenter_of(id, preds, layers, l - 1).unwrap_or(pos as f64))
                .collect();
            reorder(&mut layers[l], &scores);
        }
        for l in (0..layers.len().saturating_sub(1)).rev() {
            let scores: Vec<f64> = layers[l]
                .iter()
                .enumerate()
                .map(|(pos, &id)| barycenter_of(id, succs, layers, l + 1).unwrap_or(pos as f64))
                .collect();
            reorder(&mut layers[l], &scores);
        }
    }
}

fn reorder(layer: &mut [usize], scores: &[f64]) {
    let orig: Vec<usize> = layer.to_vec();
    let mut keyed: Vec<(f64, usize)> = scores.iter().copied().zip(0..).collect();
    keyed.sort_by(|a, b| {
        a.0.partial_cmp(&b.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.1.cmp(&b.1))
    });
    for (slot, &(_, pos)) in keyed.iter().enumerate() {
        layer[slot] = orig[pos];
    }
}

fn barycenter_of(
    id: usize,
    adj: &[Vec<usize>],
    layers: &[Vec<usize>],
    other: usize,
) -> Option<f64> {
    if layers[other].is_empty() {
        return None;
    }
    let neighbors = &adj[id];
    if neighbors.is_empty() {
        return None;
    }
    let mut sum = 0.0f64;
    let mut count = 0.0f64;
    for &nb in neighbors {
        if let Some(pos) = layers[other].iter().position(|&x| x == nb) {
            sum += pos as f64;
            count += 1.0;
        }
    }
    if count == 0.0 {
        None
    } else {
        Some(sum / count)
    }
}

/// Anchor column (box center) per node, chosen to straighten edges: a node with
/// a single parent (or single child) lands exactly on that neighbor's column.
fn place_anchors(
    layers: &[Vec<usize>],
    preds: &[Vec<usize>],
    sizes: &[i64],
    gap: usize,
) -> Vec<i64> {
    let mut anchor = vec![0i64; sizes.len()];
    for l in 1..layers.len() {
        for &id in &layers[l] {
            let ps = &preds[id];
            if !ps.is_empty() {
                let sum: i64 = ps.iter().map(|&p| anchor[p]).sum();
                anchor[id] = sum.div_euclid(ps.len() as i64);
            }
        }
        resolve(&layers[l], &mut anchor, sizes, gap);
    }
    // Center every layer's bounding box on the widest layer's center. Because
    // the anchor is the box's left-middle, a single-node layer lands exactly on
    // the shared center — so `开始`/`结束` are strictly aligned and single-parent
    // chains stay straight.
    let mut center = vec![0i64; layers.len()];
    let mut width = vec![0i64; layers.len()];
    for (l, ids) in layers.iter().enumerate() {
        let left = ids
            .iter()
            .map(|&i| anchor[i] - (sizes[i] - 1) / 2)
            .min()
            .unwrap_or(0);
        let right = ids
            .iter()
            .map(|&i| anchor[i] + (sizes[i] - 1) / 2)
            .max()
            .unwrap_or(0);
        center[l] = (left + right).div_euclid(2);
        width[l] = right - left;
    }
    let widest = width
        .iter()
        .enumerate()
        .max_by_key(|&(_, &w)| w)
        .map_or(0, |(l, _)| l);
    let target = center[widest];
    for (l, ids) in layers.iter().enumerate() {
        let shift = target - center[l];
        if shift != 0 {
            for &i in ids {
                anchor[i] += shift;
            }
        }
    }
    anchor
}

fn resolve(order: &[usize], anchor: &mut [i64], sizes: &[i64], gap: usize) {
    let mut prev_right: Option<i64> = None;
    for &id in order {
        let half = (sizes[id] - 1) / 2;
        if let Some(pr) = prev_right {
            let min_left = pr + gap as i64 + 1;
            if anchor[id] - half < min_left {
                anchor[id] += min_left - (anchor[id] - half);
            }
        }
        let half = (sizes[id] - 1) / 2;
        prev_right = Some(anchor[id] + half);
    }
}

fn place_box(
    grid: &mut [Vec<Cell>],
    x: usize,
    y: usize,
    w: usize,
    label: &str,
    state: DagViewCellState,
) {
    for i in 0..w {
        let conn = if i == 0 {
            D | R
        } else if i + 1 == w {
            D | L
        } else {
            L | R
        };
        add_conn(grid, x + i, y, conn, state);
    }
    add_conn(grid, x, y + 1, U | D, state);
    add_conn(grid, x + w - 1, y + 1, U | D, state);
    set_lit(grid, x + 1, y + 1, ' ', state);
    let mut col = 2usize;
    for ch in label.chars() {
        let cw = UnicodeWidthChar::width(ch).unwrap_or(0).max(1);
        set_lit(grid, x + col, y + 1, ch, state);
        if cw >= 2 {
            mark_skip(grid, x + col + 1, y + 1, state);
        }
        col += cw;
    }
    while col < w - 1 {
        set_lit(grid, x + col, y + 1, ' ', state);
        col += 1;
    }
    for i in 0..w {
        let conn = if i == 0 {
            U | R
        } else if i + 1 == w {
            U | L
        } else {
            L | R
        };
        add_conn(grid, x + i, y + 2, conn, state);
    }
}

fn connect(grid: &mut [Vec<Cell>], ln: &[LNode], a: usize, b: usize, xt: usize) {
    let na = &ln[a];
    let nb = &ln[b];
    let ya = na.layer * LAYER_PITCH + 2;
    let merge = ya + 1;
    let split = ya + 2;
    let yb = nb.layer * LAYER_PITCH;
    if split >= grid.len() {
        return;
    }
    let xa = na.x + (na.width - 1) / 2;
    let xb = nb.x + (nb.width - 1) / 2;
    // parents merge into the central trunk
    add_conn(grid, xa, ya, D, na.state);
    add_conn(grid, xa, merge, U, DagViewCellState::Neutral);
    hline(grid, xa, xt, merge);
    add_conn(grid, xt, merge, D, DagViewCellState::Neutral);
    // trunk drops into the split row, which fans out to the children
    add_conn(grid, xt, split, U, DagViewCellState::Neutral);
    hline(grid, xt, xb, split);
    add_conn(grid, xb, split, D, DagViewCellState::Neutral);
    if yb < grid.len() {
        add_conn(grid, xb, yb, U, nb.state);
    }
}

fn hline(grid: &mut [Vec<Cell>], x0: usize, x1: usize, y: usize) {
    let (lo, hi) = if x0 <= x1 { (x0, x1) } else { (x1, x0) };
    for x in lo..=hi {
        let mut bits = 0u8;
        if x > lo {
            bits |= L;
        }
        if x < hi {
            bits |= R;
        }
        add_conn(grid, x, y, bits, DagViewCellState::Neutral);
    }
}

fn add_conn(grid: &mut [Vec<Cell>], x: usize, y: usize, bits: u8, state: DagViewCellState) {
    if y >= grid.len() || x >= grid[y].len() {
        return;
    }
    let cell = &mut grid[y][x];
    cell.conn |= bits;
    if !cell.has_lit && cell.state == DagViewCellState::Neutral {
        cell.state = state;
    }
}

fn set_lit(grid: &mut [Vec<Cell>], x: usize, y: usize, ch: char, state: DagViewCellState) {
    if y >= grid.len() || x >= grid[y].len() {
        return;
    }
    let cell = &mut grid[y][x];
    cell.lit = ch;
    cell.has_lit = true;
    cell.state = state;
}

fn mark_skip(grid: &mut [Vec<Cell>], x: usize, y: usize, state: DagViewCellState) {
    if y >= grid.len() || x >= grid[y].len() {
        return;
    }
    let cell = &mut grid[y][x];
    cell.skip = true;
    cell.state = state;
}

fn rle(row: &[Cell]) -> Vec<DagViewCell> {
    let mut out: Vec<DagViewCell> = Vec::new();
    let mut cur_state: Option<DagViewCellState> = None;
    let mut buf = String::new();
    for cell in row {
        let state = cell.state;
        if Some(state) != cur_state && !buf.is_empty() {
            out.push(DagViewCell {
                text: std::mem::take(&mut buf),
                state: cur_state.unwrap(),
            });
        }
        cur_state = Some(state);
        if !cell.skip {
            buf.push(if cell.has_lit {
                cell.lit
            } else {
                glyph(cell.conn)
            });
        }
    }
    if !buf.is_empty() {
        out.push(DagViewCell {
            text: buf,
            state: cur_state.unwrap(),
        });
    }
    loop {
        match out.last() {
            Some(last) if last.state == DagViewCellState::Neutral => {
                let trimmed = last.text.trim_end().to_string();
                if trimmed.is_empty() {
                    out.pop();
                } else {
                    out.last_mut().unwrap().text = trimmed;
                    break;
                }
            }
            _ => break,
        }
    }
    out
}

const COL_GAP: usize = 2; // 横排：层间一列垂直总线 + 一列留白
const V_GAP: usize = 1; // 横排：同层节点上下间距

/// 横排（左→右）：层变列，同层节点竖着堆、垂直居中，开始/结束左右居中。
#[must_use]
pub fn render_horizontal(nodes: &[DagViewNode], frame: usize, cap: usize) -> Vec<Vec<DagViewCell>> {
    render_horizontal_cfg(nodes, frame, cap, COL_GAP)
}

fn render_horizontal_cfg(
    nodes: &[DagViewNode],
    frame: usize,
    cap: usize,
    col_gap: usize,
) -> Vec<Vec<DagViewCell>> {
    let truncated = nodes.len() > MAX_VIEW_NODES;
    let src = &nodes[..nodes.len().min(MAX_VIEW_NODES)];
    let Prepared {
        mut ln,
        layers,
        edges,
        preds,
    } = prepare(src, frame, cap);
    let nlayers = layers.len();

    // cross-axis (row) anchors: every box is 3 rows tall.
    let sizes = vec![3i64; ln.len()];
    let anchors = place_anchors(&layers, &preds, &sizes, V_GAP);
    let min_a = anchors.iter().copied().min().unwrap_or(0);
    let row_of = |i: usize| (anchors[i] - min_a + 1) as usize;

    // column widths / x offsets
    let mut col_w = vec![0usize; nlayers];
    for (l, ids) in layers.iter().enumerate() {
        col_w[l] = ids.iter().map(|&i| ln[i].width).max().unwrap_or(1);
    }
    let mut x_left = vec![0usize; nlayers];
    let mut cursor = 0usize;
    for l in 0..nlayers {
        x_left[l] = cursor;
        cursor += col_w[l] + col_gap;
    }
    let canvas_w = cursor.saturating_sub(col_gap).max(1);
    let max_row = (0..ln.len()).map(&row_of).max().unwrap_or(0);
    let height = max_row + 2;
    let mut grid: Vec<Vec<Cell>> = vec![vec![Cell::empty(); canvas_w]; height];

    for node in &mut ln {
        node.x = if node.is_dummy {
            x_left[node.layer] + col_w[node.layer] / 2
        } else {
            x_left[node.layer] + (col_w[node.layer] - node.width) / 2
        };
    }
    for (i, node) in ln.iter().enumerate() {
        if !node.is_dummy {
            place_box(
                &mut grid,
                node.x,
                row_of(i).saturating_sub(1),
                node.width,
                &node.label,
                node.state,
            );
        }
    }

    // connectors per boundary: one vertical bus column between the layers.
    // The bus only spans the rows an edge actually needs, so a straight edge
    // stays a single horizontal line (no stray `┼`).
    let row_to_y = |v: i64| (v - min_a + 1) as usize;
    for l in 0..nlayers.saturating_sub(1) {
        let x_band = x_left[l] + col_w[l];
        for &(a, b) in &edges {
            if ln[a].layer != l {
                continue;
            }
            let ya = row_to_y(anchors[a]);
            let yb = row_to_y(anchors[b]);
            let xa = ln[a].x + ln[a].width - 1;
            let xb = ln[b].x;
            add_conn(&mut grid, xa, ya, R, ln[a].state);
            hline(&mut grid, xa, x_band, ya);
            if ya < yb {
                add_conn(&mut grid, x_band, ya, D, DagViewCellState::Neutral);
                for y in (ya + 1)..yb {
                    add_conn(&mut grid, x_band, y, U | D, DagViewCellState::Neutral);
                }
                add_conn(&mut grid, x_band, yb, U, DagViewCellState::Neutral);
            } else if ya > yb {
                add_conn(&mut grid, x_band, ya, U, DagViewCellState::Neutral);
                for y in (yb + 1)..ya {
                    add_conn(&mut grid, x_band, y, U | D, DagViewCellState::Neutral);
                }
                add_conn(&mut grid, x_band, yb, D, DagViewCellState::Neutral);
            }
            hline(&mut grid, x_band, xb, yb);
            add_conn(&mut grid, xb, yb, L, ln[b].state);
        }
    }

    let mut out: Vec<Vec<DagViewCell>> = grid.iter().map(|row| rle(row)).collect();
    if !src.is_empty() {
        out.push(Vec::new());
        for node in src {
            let text = if node.name.is_empty() {
                node.tool_name.clone()
            } else {
                format!("{} ({})", node.name, node.tool_name)
            };
            out.push(vec![DagViewCell {
                text,
                state: DagViewCellState::Neutral,
            }]);
        }
    }
    if truncated {
        let omitted = nodes.len().saturating_sub(MAX_VIEW_NODES);
        out.push(vec![DagViewCell {
            text: format!("… 其余 {omitted} 个节点已省略"),
            state: DagViewCellState::Neutral,
        }]);
    }
    out
}

/// The layout decision [`render_auto`] reaches for a given width.
///
/// Independent of the spinner `frame` — every braille glyph is one display
/// column, so the fit probe cannot change with it. That makes this cheap to
/// cache in a TUI: re-decide only when the node set or the width changes, and
/// re-render every frame with the cached decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Orientation {
    /// Vertical tree via [`render_layout`] with an explicit name cap / gap.
    Vertical { cap: usize, gap: usize },
    /// Left-to-right layers via [`render_horizontal_cfg`].
    Horizontal { cap: usize, col_gap: usize },
}

/// Decide the orientation for `nodes` within `max_width` columns, without
/// rendering every candidate for real.
///
/// Probes use `frame = 0` and mirror [`render_auto`]'s search order exactly
/// (horizontal first, widest labels first), so the decision and the eventual
/// render always agree.
#[must_use]
pub fn choose_orientation(nodes: &[DagViewNode], max_width: usize) -> Orientation {
    if max_width == 0 {
        return Orientation::Vertical {
            cap: 10,
            gap: H_GAP,
        };
    }
    for col_gap in [COL_GAP, 1] {
        for cap in (4..=10usize).rev() {
            if width_of_rows(&render_horizontal_cfg(nodes, 0, cap, col_gap)) <= max_width {
                return Orientation::Horizontal { cap, col_gap };
            }
        }
    }
    for gap in [H_GAP, 1] {
        for cap in (4..=10usize).rev() {
            if width_of_rows(&render_layout(nodes, 0, cap, gap)) <= max_width {
                return Orientation::Vertical { cap, gap };
            }
        }
    }
    Orientation::Vertical { cap: 4, gap: 1 }
}

/// Render with a pre-decided [`Orientation`] and the current spinner `frame`.
#[must_use]
pub fn render_with(
    nodes: &[DagViewNode],
    frame: usize,
    orientation: Orientation,
) -> Vec<Vec<DagViewCell>> {
    match orientation {
        Orientation::Vertical { cap, gap } => render_layout(nodes, frame, cap, gap),
        Orientation::Horizontal { cap, col_gap } => {
            render_horizontal_cfg(nodes, frame, cap, col_gap)
        }
    }
}

/// 自动取向，按可用宽度判断：
/// 1. `max_width == 0`（未知宽度）→ 竖排自然宽；
/// 2. 横排逐级缩间距、缩名字，只要能塞进 `max_width` 就**优先横排**；
/// 3. 横排怎么缩都塞不下 → 竖排，并同样缩到 `max_width` 内（缩不动才自然宽）。
///
/// 判据只看宽度，且两种取向都尝试收缩，所以不会出现"明明缩一下就能放下却
/// 直接退回另一种"的误判。
#[must_use]
pub fn render_auto(nodes: &[DagViewNode], frame: usize, max_width: usize) -> Vec<Vec<DagViewCell>> {
    render_with(nodes, frame, choose_orientation(nodes, max_width))
}

/// Shared graph preparation for both orientations: layout nodes, layers, edges.
fn prepare(src: &[DagViewNode], frame: usize, cap: usize) -> Prepared {
    let n = src.len();
    let mut by_id: HashMap<u32, usize> = HashMap::with_capacity(n);
    for (i, node) in src.iter().enumerate() {
        by_id.insert(node.id, i);
    }
    let mut deps: Vec<Vec<usize>> = vec![Vec::new(); n];
    for (i, node) in src.iter().enumerate() {
        for d in &node.depends_on {
            if let Some(&j) = by_id.get(d) {
                deps[i].push(j);
            }
        }
    }
    let mut dependents: Vec<Vec<usize>> = vec![Vec::new(); n];
    for (i, ds) in deps.iter().enumerate() {
        for &d in ds {
            dependents[d].push(i);
        }
    }
    let real_layer = compute_layers(n, &deps, &dependents);
    let max_real = real_layer.iter().copied().max().map_or(0, |m| m + 1);

    let mut ln: Vec<LNode> = Vec::new();
    let start_idx = 0usize;
    ln.push(LNode {
        label: START_LABEL.to_string(),
        width: box_width(START_LABEL),
        layer: 0,
        x: 0,
        state: DagViewCellState::Root,
        is_dummy: false,
        key: usize::MAX,
    });
    for (i, node) in src.iter().enumerate() {
        let display = if node.name.is_empty() {
            node.tool_name.as_str()
        } else {
            node.name.as_str()
        };
        let label = format!("{} {}", node.state.marker(frame), short_name(display, cap));
        let width = box_width(&label);
        ln.push(LNode {
            label,
            width,
            layer: real_layer[i] + 1,
            x: 0,
            state: DagViewCellState::Node(node.state),
            is_dummy: false,
            key: i,
        });
    }
    let end_idx = ln.len();
    ln.push(LNode {
        label: END_LABEL.to_string(),
        width: box_width(END_LABEL),
        layer: max_real + 1,
        x: 0,
        state: DagViewCellState::Root,
        is_dummy: false,
        key: usize::MAX,
    });

    let mut edges: Vec<(usize, usize)> = Vec::new();
    for i in 0..n {
        for &d in &deps[i] {
            if real_layer[d] < real_layer[i] {
                edges.push((1 + d, 1 + i));
            }
        }
    }
    let mut has_in = vec![false; ln.len()];
    let mut has_out = vec![false; ln.len()];
    for &(a, b) in &edges {
        has_out[a] = true;
        has_in[b] = true;
    }
    for i in 0..n {
        let idx = 1 + i;
        if !has_in[idx] {
            edges.push((start_idx, idx));
        }
        if !has_out[idx] {
            edges.push((idx, end_idx));
        }
    }
    if n == 0 {
        edges.push((start_idx, end_idx));
    }

    let mut next_key = n + 10;
    let mut chain: Vec<(usize, usize)> = Vec::new();
    for &(a, b) in &edges {
        let la = ln[a].layer;
        let lb = ln[b].layer;
        if lb > la + 1 {
            let mut prev = a;
            for l in (la + 1)..lb {
                ln.push(LNode {
                    label: String::new(),
                    width: 1,
                    layer: l,
                    x: 0,
                    state: DagViewCellState::Neutral,
                    is_dummy: true,
                    key: next_key,
                });
                next_key += 1;
                let cur = ln.len() - 1;
                chain.push((prev, cur));
                prev = cur;
            }
            chain.push((prev, b));
        } else {
            chain.push((a, b));
        }
    }
    let edges = chain;

    let max_layer = ln.iter().map(|x| x.layer).max().unwrap_or(0);
    let mut layers: Vec<Vec<usize>> = vec![Vec::new(); max_layer + 1];
    for (i, node) in ln.iter().enumerate() {
        layers[node.layer].push(i);
    }
    for l in &mut layers {
        l.sort_by_key(|&i| (ln[i].is_dummy, ln[i].key));
    }
    let mut preds: Vec<Vec<usize>> = vec![Vec::new(); ln.len()];
    let mut succs: Vec<Vec<usize>> = vec![Vec::new(); ln.len()];
    for &(a, b) in &edges {
        succs[a].push(b);
        preds[b].push(a);
    }
    barycenter(&mut layers, &preds, &succs);

    Prepared {
        ln,
        layers,
        edges,
        preds,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn n(id: u32, tool: &str, deps: &[u32], state: DagViewState) -> DagViewNode {
        DagViewNode {
            id,
            name: String::new(),
            tool_name: tool.to_string(),
            depends_on: deps.to_vec(),
            state,
            output: String::new(),
        }
    }

    fn leading_spaces(line: &str) -> usize {
        line.chars().take_while(|c| *c == ' ').count()
    }

    #[test]
    fn linear_chain_has_no_jogs() {
        let out = render_to_string(&[
            n(1, "read", &[], DagViewState::Succeeded),
            n(2, "write", &[1], DagViewState::Pending),
        ]);
        let joined = out.join("\n");
        for bad in ["└┐", "┌┘", "┴┬", "┬┴", "└┬", "┴┐"] {
            assert!(!joined.contains(bad), "jog {bad:?} in:\n{joined}");
        }
        assert!(joined.contains("[✓] read"), "{joined}");
        assert!(joined.contains("[ ] write"), "{joined}");
    }

    #[test]
    fn start_and_end_are_vertically_aligned() {
        let out = render_to_string(&[
            n(1, "read", &[], DagViewState::Succeeded),
            n(2, "parse", &[1], DagViewState::Running),
            n(3, "write", &[1], DagViewState::Pending),
        ]);
        let start = out.iter().find(|l| l.contains("开始")).unwrap();
        let end = out.iter().find(|l| l.contains("结束")).unwrap();
        assert_eq!(leading_spaces(start), leading_spaces(end), "{out:?}");
    }

    #[test]
    fn markers_are_standard_unicode() {
        let joined = render_to_string(&[
            n(1, "a", &[], DagViewState::Succeeded),
            n(2, "b", &[1], DagViewState::Failed),
            n(3, "c", &[1], DagViewState::Skipped),
        ])
        .join("\n");
        assert!(joined.contains("[✓]"), "{joined}");
        assert!(joined.contains("[✗]"), "{joined}");
        assert!(joined.contains("[-]"), "{joined}");
    }

    #[test]
    fn running_spinner_cycles_through_all_frames() {
        let nodes = vec![n(1, "build", &[], DagViewState::Running)];
        let mut seen = String::new();
        for f in 0..SPINNER_FRAMES.len() {
            let row = render_with_frame(&nodes, f)
                .into_iter()
                .map(|r| r.into_iter().map(|c| c.text).collect::<String>())
                .find(|l| l.contains("build"))
                .unwrap();
            seen.push_str(&row);
        }
        for glyph in SPINNER_FRAMES {
            assert!(seen.contains(glyph), "missing spinner frame {glyph}");
        }
    }

    #[test]
    fn diamond_rejoins_after_both_parents() {
        let joined = render_to_string(&[
            n(1, "read", &[], DagViewState::Succeeded),
            n(2, "parse", &[1], DagViewState::Running),
            n(3, "fetch", &[1], DagViewState::Pending),
            n(4, "write", &[2, 3], DagViewState::Pending),
        ])
        .join("\n");
        // 只数图本体（全称列表在空行之后，名字会再出现一次）。
        let diagram = joined.split("\n\n").next().unwrap();
        for name in ["read", "parse", "fetch", "write"] {
            assert_eq!(
                diagram.matches(name).count(),
                1,
                "node {name} not exactly once in diagram:\n{joined}"
            );
        }
        let p2 = diagram.find("parse").unwrap();
        let p3 = diagram.find("fetch").unwrap();
        let c4 = diagram.find("write").unwrap();
        assert!(p2 < c4 && p3 < c4, "{joined}");
    }

    #[test]
    fn dangling_and_cycle_nodes_do_not_disappear() {
        let joined = render_to_string(&[
            n(1, "orphan", &[9], DagViewState::Pending),
            n(2, "a", &[3], DagViewState::Pending),
            n(3, "b", &[2], DagViewState::Pending),
        ])
        .join("\n");
        for needle in ["[ ] orphan", "[ ] a", "[ ] b"] {
            assert!(joined.contains(needle), "missing {needle}:\n{joined}");
        }
    }

    #[test]
    fn empty_graph_renders_start_and_end() {
        let joined = render_to_string(&[]).join("\n");
        assert!(
            joined.contains("开始") && joined.contains("结束"),
            "{joined}"
        );
    }

    #[test]
    fn width_fitted_shrinks_labels() {
        let mut nodes = vec![n(0, "root", &[], DagViewState::Succeeded)];
        for i in 1..=12u32 {
            nodes.push(n(i, "web_search", &[0], DagViewState::Pending));
        }
        let natural: Vec<String> = render_to_string(&nodes);
        let natural_w = natural
            .iter()
            .map(|l| UnicodeWidthStr::width(l.as_str()))
            .max()
            .unwrap();
        let fitted: Vec<String> = render_fitted(&nodes, 0, natural_w / 2)
            .into_iter()
            .map(|r| r.into_iter().map(|c| c.text).collect::<String>())
            .collect();
        let fitted_w = fitted
            .iter()
            .map(|l| UnicodeWidthStr::width(l.as_str()))
            .max()
            .unwrap();
        assert!(
            fitted_w < natural_w,
            "fitted {fitted_w} !< natural {natural_w}"
        );
    }

    fn rows_to_string(rows: &[Vec<DagViewCell>]) -> Vec<String> {
        rows.iter()
            .map(|r| r.iter().map(|c| c.text.clone()).collect())
            .collect()
    }

    #[test]
    fn render_auto_picks_orientation_by_width() {
        // Wide fan-out: horizontal fits a 100-column pane, so 开始/结束 share a row.
        let mut fan = vec![n(0, "root", &[], DagViewState::Succeeded)];
        for i in 1..=12u32 {
            fan.push(n(i, "web_search", &[0], DagViewState::Pending));
        }
        let auto = render_auto(&fan, 0, 100);
        let lines = rows_to_string(&auto);
        let start = lines.iter().position(|l| l.contains("开始")).unwrap();
        let end = lines.iter().position(|l| l.contains("结束")).unwrap();
        assert_eq!(
            start,
            end,
            "expected horizontal at width 100:\n{}",
            lines.join("\n")
        );
        assert!(
            width_of_rows(&auto) <= 100,
            "chosen layout must fit:\n{}",
            lines.join("\n")
        );

        // Long chain: horizontal is far too wide, so vertical (开始 above 结束).
        let chain: Vec<DagViewNode> = (1..=12u32)
            .map(|i| {
                let deps = if i == 1 { Vec::new() } else { vec![i - 1] };
                n(i, "step", &deps, DagViewState::Pending)
            })
            .collect();
        let auto = render_auto(&chain, 0, 100);
        let lines = rows_to_string(&auto);
        let start = lines.iter().position(|l| l.contains("开始")).unwrap();
        let end = lines.iter().position(|l| l.contains("结束")).unwrap();
        assert!(
            end > start,
            "expected vertical for a long chain:\n{}",
            lines.join("\n")
        );

        // Unknown width → vertical, never panics.
        let auto = render_auto(&chain, 0, 0);
        let lines = rows_to_string(&auto);
        let start = lines.iter().position(|l| l.contains("开始")).unwrap();
        let end = lines.iter().position(|l| l.contains("结束")).unwrap();
        assert!(end > start, "width 0 must fall back to vertical");
    }

    /// The TUI caches [`choose_orientation`] and calls [`render_with`] per
    /// frame. That is only sound if it reproduces [`render_auto`] exactly —
    /// for every spinner frame, not just `frame = 0` (the frame the decision
    /// probes with).
    #[test]
    fn cached_orientation_matches_render_auto_at_every_frame() {
        let mut fan = vec![n(0, "root", &[], DagViewState::Running)];
        for i in 1..=12u32 {
            fan.push(n(i, "web_search", &[0], DagViewState::Pending));
        }
        let chain: Vec<DagViewNode> = (1..=8u32)
            .map(|i| {
                let deps = if i == 1 { Vec::new() } else { vec![i - 1] };
                n(i, "step", &deps, DagViewState::Running)
            })
            .collect();

        for nodes in [&fan, &chain] {
            for width in [0usize, 20, 60, 100, 400] {
                let orientation = choose_orientation(nodes, width);
                for frame in 0..10 {
                    let cached = render_with(nodes, frame, orientation);
                    let direct = render_auto(nodes, frame, width);
                    assert_eq!(
                        rows_to_string(&cached),
                        rows_to_string(&direct),
                        "cached orientation diverged at width={width} frame={frame}"
                    );
                }
            }
        }
    }

    #[test]
    fn cjk_content_keeps_box_rows_aligned() {
        let out = render_to_string(&[n(1, "查询用户资料", &[], DagViewState::Running)]);
        let idx = out.iter().position(|l| l.contains("查询")).unwrap();
        let w = |i: usize| UnicodeWidthStr::width(out[i].as_str());
        assert!(idx >= 1 && idx + 1 < out.len());
        assert_eq!(w(idx - 1), w(idx), "{out:?}");
        assert_eq!(w(idx), w(idx + 1), "{out:?}");
    }

    #[test]
    fn horizontal_mode_renders_and_auto_prefers_it_when_wide() {
        let nodes = vec![
            n(1, "read", &[], DagViewState::Succeeded),
            n(2, "write", &[1], DagViewState::Pending),
        ];
        let joined = render_horizontal(&nodes, 0, 10)
            .into_iter()
            .map(|r| r.into_iter().map(|c| c.text).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            joined.contains("开始") && joined.contains("结束"),
            "{joined}"
        );
        assert!(
            joined.contains("[✓] read") && joined.contains("[ ] write"),
            "{joined}"
        );
        // 宽裕时 render_auto 选横排（开始/结束与节点同一行）。
        let auto = render_auto(&nodes, 0, 200)
            .into_iter()
            .map(|r| r.into_iter().map(|c| c.text).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(auto.contains("开始") && auto.contains("结束"), "{auto}");
    }
}
