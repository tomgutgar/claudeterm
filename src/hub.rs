//! Home screen: pick which CLI to open (Picker), or watch every agent at once
//! (Hub): this PC at the root, each open session below it, and under each one
//! its subagents and background shells.

use std::time::{Duration, Instant, SystemTime};

use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::data::{Cli, Store, clip};
use crate::i18n::tr;
use crate::ui::{DIM, block_c};
use crate::views::{Action, View, short_path};

// ───────────────────────── icons ─────────────────────────

/// Claude Code's mascot exactly as `claude` prints it on startup
/// (" ▐▛███▜▌ ", "▝▜█████▛▘", "  ▘▘ ▝▝  "), as pixels: each character of the
/// banner is 2×2 of these (quadrant blocks).
const CLAWD: [&str; 6] = [
    "...############...",
    "...##.######.##...",
    ".################.",
    "...############...",
    "....#.#....#.#....",
    "..................",
];

/// Kimi Code's logo exactly as `kimi` draws it in its welcome box
/// ("▐█▛█▛█▌", "▐█████▌"), centred in Claude's 9×3 characters.
const KIMI: [&str; 6] = [
    "...############...",
    "...####.###.###...",
    "...############...",
    "...############...",
    "..................",
    "..................",
];

/// Codex's logo: OpenAI's (ChatGPT's) blossom, rasterised to 24×24 from its
/// SVG (Simple Icons). Drawn as half blocks, so its pixels are square.
const OPENAI: [&str; 24] = [
    "........#####...........",
    ".......########.........",
    "......##.....######.....",
    ".....##.....####.####...",
    "....###...###......##...",
    "..####...###........##..",
    ".###.#..##....###....#..",
    ".##..#..#...###.###..##.",
    "##...#..#..###...######.",
    "##...#..###..##....###..",
    "##...#..##....###....##.",
    "##...#..#......####...#.",
    ".#...####......#..#...##",
    ".##....###....##..#...##",
    "..###....##..###..#...##",
    ".######...###..#..#...##",
    ".##..###.###...#..#..##.",
    "..#....###....##..#.###.",
    "..##........###...####..",
    "...##......###...###....",
    "...####.####.....##.....",
    ".....######.....##......",
    ".........########.......",
    "...........#####........",
];

/// Antigravity's rainbow logo exactly as `agy` 1.3.1 prints it on startup:
/// per character row, each half block as (column, top, bottom) xterm-256
/// colours, 0 = transparent. 12 columns × 5 rows, i.e. 12×10 pixels.
const AGY: [&[(usize, u8, u8)]; 5] = [
    &[(4, 0, 179), (5, 208, 208), (6, 203, 209), (7, 0, 203)],
    &[
        (3, 149, 113),
        (4, 143, 107),
        (5, 173, 173),
        (6, 209, 209),
        (7, 203, 167),
        (8, 203, 167),
    ],
    &[
        (2, 107, 113),
        (3, 71, 72),
        (4, 72, 68),
        (5, 67, 0),
        (6, 103, 0),
        (7, 98, 68),
        (8, 97, 62),
        (9, 132, 97),
    ],
    &[
        (1, 0, 78),
        (2, 78, 74),
        (3, 73, 74),
        (8, 68, 69),
        (9, 68, 69),
        (10, 0, 68),
    ],
    &[
        (0, 0, 75),
        (1, 79, 75),
        (2, 75, 0),
        (9, 69, 0),
        (10, 69, 69),
        (11, 0, 69),
    ],
];

type Rgb = [f32; 3];
type ColorPx = Vec<Vec<Option<Rgb>>>;

/// xterm-256 colour cube (16–231) to RGB.
fn xterm(n: u8) -> Rgb {
    const LV: [f32; 6] = [0.0, 95.0, 135.0, 175.0, 215.0, 255.0];
    let n = (n - 16) as usize;
    [LV[n / 36], LV[n / 6 % 6], LV[n % 6]]
}

fn agy_original() -> ColorPx {
    let mut p = vec![vec![None; 12]; 10];
    for (y, row) in AGY.iter().enumerate() {
        for &(x, top, bottom) in *row {
            p[y * 2][x] = (top > 0).then(|| xterm(top));
            p[y * 2 + 1][x] = (bottom > 0).then(|| xterm(bottom));
        }
    }
    p
}

/// Resized to w×h by area averaging; a pixel is drawn if it's at least half
/// covered.
fn resample(p: &ColorPx, w: usize, h: usize) -> ColorPx {
    let (sh, sw) = (p.len() as f32, p[0].len() as f32);
    (0..h)
        .map(|y| {
            (0..w)
                .map(|x| {
                    let (y0, y1) = (y as f32 * sh / h as f32, (y + 1) as f32 * sh / h as f32);
                    let (x0, x1) = (x as f32 * sw / w as f32, (x + 1) as f32 * sw / w as f32);
                    let (mut sum, mut cov, mut area) = ([0.0; 3], 0.0, 0.0);
                    for (sy, row) in p.iter().enumerate() {
                        for (sx, c) in row.iter().enumerate() {
                            let a = (y1.min(sy as f32 + 1.0) - y0.max(sy as f32)).max(0.0)
                                * (x1.min(sx as f32 + 1.0) - x0.max(sx as f32)).max(0.0);
                            area += a;
                            if let Some(c) = c {
                                cov += a;
                                (0..3).for_each(|i| sum[i] += a * c[i]);
                            }
                        }
                    }
                    (cov >= area / 2.0).then(|| sum.map(|s| s / cov))
                })
                .collect()
        })
        .collect()
}

/// The logo at the size of Claude's: 9×3 characters, or 36×12 when `big`.
/// `up`: floating half a character higher (its animation).
fn agy_pixels(big: bool, up: bool) -> ColorPx {
    let (w, h) = if big { (36, 24) } else { (9, 6) };
    let mut p = resample(&agy_original(), w, h);
    if up {
        p.remove(0);
        p.push(vec![None; w]);
    }
    p
}

/// Codex's logo: 24×24 (12 rows, Claude's big height) or, small, 8×8 (4 rows:
/// at Claude's 6 pixels it's just a blob). Each small pixel is lit if most
/// of its 3×3 block is. `up`: floating half a
/// character higher (its animation).
fn openai_pixels(big: bool, up: bool) -> Px {
    let full = px(&OPENAI);
    let mut p = if big {
        full
    } else {
        (0..8)
            .map(|y| {
                (0..8)
                    .map(|x| {
                        let lit = (0..9)
                            .filter(|i| full[y * 3 + i / 3][x * 3 + i % 3])
                            .count();
                        lit >= 5
                    })
                    .collect()
            })
            .collect()
    };
    if up {
        let w = p[0].len();
        p.remove(0);
        p.push(vec![false; w]);
    }
    p
}

/// Draws coloured pixels as half blocks; transparent ones leave what's behind.
fn paint(buf: &mut Buffer, area: Rect, x: i32, y: i32, p: &ColorPx) {
    let rgb = |c: Rgb| Color::Rgb(c[0] as u8, c[1] as u8, c[2] as u8);
    for (l, rows) in p.chunks(2).enumerate() {
        for (i, &top) in rows[0].iter().enumerate() {
            let bottom = rows.get(1).and_then(|r| r[i]);
            let (ch, mut style) = match (top, bottom) {
                (Some(t), _) => ("▀", Style::default().fg(rgb(t))),
                (None, Some(b)) => ("▄", Style::default().fg(rgb(b))),
                (None, None) => continue,
            };
            if let (Some(_), Some(b)) = (top, bottom) {
                style = style.bg(rgb(b));
            }
            put(buf, area, x + i as i32, y + l as i32, ch, style);
        }
    }
}

const SHELL_WHITE: Color = Color::Rgb(0xe8, 0xe6, 0xe3);

type Px = Vec<Vec<bool>>;

fn px(rows: &[&str]) -> Px {
    rows.iter()
        .map(|r| r.chars().map(|c| c == '#').collect())
        .collect()
}

/// Each pixel becomes k×k.
fn scale(p: &Px, k: usize) -> Px {
    p.iter()
        .flat_map(|row| {
            let wide: Vec<bool> = row
                .iter()
                .flat_map(|&b| std::iter::repeat_n(b, k))
                .collect();
            std::iter::repeat_n(wide, k)
        })
        .collect()
}

/// 2×2 pixels per character.
fn quad(p: &Px) -> Vec<String> {
    const Q: [char; 16] = [
        ' ', '▘', '▝', '▀', '▖', '▌', '▞', '▛', '▗', '▚', '▐', '▜', '▄', '▙', '▟', '█',
    ];
    let at = |y: usize, x: usize| p.get(y).and_then(|r| r.get(x)).copied().unwrap_or(false);
    let w = p.first().map_or(0, Vec::len);
    (0..p.len().div_ceil(2))
        .map(|l| {
            (0..w.div_ceil(2))
                .map(|c| {
                    let (y, x) = (l * 2, c * 2);
                    let bits = at(y, x) as usize
                        | (at(y, x + 1) as usize) << 1
                        | (at(y + 1, x) as usize) << 2
                        | (at(y + 1, x + 1) as usize) << 3;
                    Q[bits]
                })
                .collect()
        })
        .collect()
}

/// 1×2 pixels per character.
fn half(p: &Px) -> Vec<String> {
    let w = p.first().map_or(0, Vec::len);
    (0..p.len().div_ceil(2))
        .map(|l| {
            (0..w)
                .map(|x| {
                    let top = p[l * 2][x];
                    let bottom = p.get(l * 2 + 1).is_some_and(|r| r[x]);
                    match (top, bottom) {
                        (true, true) => '█',
                        (true, false) => '▀',
                        (false, true) => '▄',
                        (false, false) => ' ',
                    }
                })
                .collect()
        })
        .collect()
}

/// Whether the alternate animation frame is showing. Working: it flips every
/// 400 ms. Waiting: a short twitch every 5 s, so it's visibly alive but calm.
fn alt_frame(busy: bool, ms: u128) -> bool {
    if busy {
        (ms / 400) % 2 == 1
    } else {
        ms % 5000 < 350
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Pc,
    Cli(Cli),
    Shell,
}

/// The icon's text lines. `big` = the home screen's large version.
fn icon(kind: Kind, busy: bool, ms: u128, big: bool) -> Vec<String> {
    let alt = alt_frame(busy, ms);
    match kind {
        Kind::Cli(Cli::Claude) => {
            let mut p = px(&CLAWD);
            if alt && busy {
                // Arms down / arms up: busy typing.
                p.swap(2, 3);
            } else if alt {
                // Blink.
                p[1] = p[0].clone();
            }
            quad(&scale(&p, if big { 4 } else { 1 }))
        }
        Kind::Cli(Cli::Kimi) => {
            let mut p = px(&KIMI);
            if alt && busy {
                // Bobbing half a character down and back: busy.
                p.rotate_right(1);
            } else if alt {
                // Blink.
                p[1] = p[0].clone();
            }
            quad(&scale(&p, if big { 4 } else { 1 }))
        }
        Kind::Cli(Cli::Codex) => half(&openai_pixels(big, alt)),
        // Its shape, for the layout; `paint` draws it in colour on top.
        Kind::Cli(Cli::Agy) => half(
            &agy_pixels(big, alt)
                .iter()
                .map(|r| r.iter().map(Option::is_some).collect())
                .collect(),
        ),
        Kind::Shell => {
            let screen = if busy {
                [">_ ", ">._", ">.."][(ms / 300 % 3) as usize]
            } else if alt {
                ">  "
            } else {
                ">_ "
            };
            vec!["╭───╮".into(), format!("│{screen}│"), "╰───╯".into()]
        }
        Kind::Pc => {
            let cursor = if alt { "  " } else { ">_" };
            vec![
                "╭────────────╮".into(),
                format!("│ {cursor}         │"),
                "│            │".into(),
                "╰─────┬┬─────╯".into(),
                "   ▀▀▀▀▀▀▀▀   ".into(),
            ]
        }
    }
}

fn width(lines: &[String]) -> u16 {
    lines.iter().map(|l| l.chars().count()).max().unwrap_or(0) as u16
}

/// Writes clipped to `area`; (x, y) may be outside it. Without a background,
/// spaces are skipped so icons don't erase the branches behind them.
fn put(buf: &mut Buffer, area: Rect, x: i32, y: i32, s: &str, style: Style) {
    if y < area.y as i32 || y >= area.bottom() as i32 {
        return;
    }
    for (i, ch) in s.chars().enumerate() {
        let cx = x + i as i32;
        if cx >= area.x as i32 && cx < area.right() as i32 && (ch != ' ' || style.bg.is_some()) {
            buf[(cx as u16, y as u16)].set_char(ch).set_style(style);
        }
    }
}

fn busy_text(n: usize, busy: usize) -> String {
    crate::tr!(
        "● {busy} trabajando · ◌ {} esperando",
        "● {busy} working · ◌ {} waiting",
        n - busy
    )
}

// ───────────────────────── Picker ─────────────────────────

/// One big icon per CLI, in a 2×2 grid: Enter opens that CLI's dashboard.
pub struct Picker {
    sel: usize,
    /// (open sessions, working) per CLI.
    counts: [(usize, usize); 4],
    cards: [Rect; 4],
    start: Instant,
}

impl Default for Picker {
    fn default() -> Self {
        Self {
            sel: 0,
            counts: [(0, 0); 4],
            cards: [Rect::default(); 4],
            start: Instant::now(),
        }
    }
}

const CLIS: [Cli; 4] = Cli::ALL;

impl View for Picker {
    fn title(&self) -> &'static str {
        tr("Inicio", "Home")
    }
    fn accent(&self) -> Color {
        CLIS[self.sel].color()
    }
    fn refresh(&mut self, _st: &Store) {}
    fn refresh_all(&mut self, stores: &[Store]) {
        for (i, st) in stores.iter().enumerate() {
            let busy = st.live.iter().filter(|l| l.status == "busy").count();
            self.counts[i] = (st.live.len(), busy);
        }
    }

    fn draw(&mut self, f: &mut Frame, area: Rect) {
        let ms = self.start.elapsed().as_millis();
        // Big icons need 19 rows per card; small ones fit in 9.
        let big = area.height > 2 * 19 && area.width >= 92;
        let h = if big { 19 } else { 9 };
        let [_, top, _, bottom, _] = Layout::vertical([
            Constraint::Fill(1),
            Constraint::Length(h),
            Constraint::Length(1),
            Constraint::Length(h),
            Constraint::Fill(1),
        ])
        .areas(area);
        let cols = |row| {
            let [_, a, _, b, _] = Layout::horizontal([
                Constraint::Fill(1),
                Constraint::Length(44),
                Constraint::Length(4),
                Constraint::Length(44),
                Constraint::Fill(1),
            ])
            .areas(row);
            [a, b]
        };
        let ([a, b], [c, d]) = (cols(top), cols(bottom));
        self.cards = [a, b, c, d];
        for (i, (cli, card)) in CLIS.into_iter().zip(self.cards).enumerate() {
            let selected = i == self.sel;
            let color = if selected { cli.color() } else { DIM };
            let (n, busy) = self.counts[i];
            let mut block = block_c(cli.name(), color);
            if selected {
                block = block
                    .border_type(ratatui::widgets::BorderType::Thick)
                    .title_bottom(Line::from(tr(" Enter abrir ", " Enter open ")).centered());
            }
            let inner = block.inner(card);
            f.render_widget(block, card);
            let lines = icon(Kind::Cli(cli), busy > 0, ms, big);
            let (w, h) = (width(&lines), lines.len() as u16);
            let x = inner.x as i32 + (inner.width.saturating_sub(w) / 2) as i32;
            let y = inner.y as i32 + (inner.height.saturating_sub(h + 2) / 2) as i32;
            let style = Style::default().fg(cli.color());
            if cli == Cli::Agy {
                let up = alt_frame(busy > 0, ms);
                paint(f.buffer_mut(), inner, x, y, &agy_pixels(big, up));
            } else {
                for (j, l) in lines.iter().enumerate() {
                    put(f.buffer_mut(), inner, x, y + j as i32, l, style);
                }
            }
            let status = Paragraph::new(busy_text(n, busy))
                .style(Style::default().fg(if n > 0 { Color::Gray } else { DIM }))
                .centered();
            let sy = (y + h as i32 + 1)
                .min(inner.bottom() as i32 - 1)
                .max(inner.y as i32);
            f.render_widget(status, Rect::new(inner.x, sy as u16, inner.width, 1));
        }
    }

    fn on_key(&mut self, key: KeyEvent) -> Action {
        match key.code {
            // ←/→ go through all four; ↑/↓ change row.
            KeyCode::Right => {
                self.sel = (self.sel + 1) % CLIS.len();
                Action::Handled
            }
            KeyCode::Left => {
                self.sel = (self.sel + CLIS.len() - 1) % CLIS.len();
                Action::Handled
            }
            KeyCode::Up | KeyCode::Down => {
                self.sel ^= 2;
                Action::Handled
            }
            KeyCode::Enter => Action::Open(CLIS[self.sel]),
            _ => Action::Ignored,
        }
    }

    fn footer(&self) -> &'static str {
        tr(
            "←/→ ↑/↓ elegir · Enter abrir",
            "←/→ ↑/↓ choose · Enter open",
        )
    }

    fn click(&mut self, x: u16, y: u16) -> bool {
        let hit = self
            .cards
            .iter()
            .position(|r| x >= r.x && x < r.right() && y >= r.y && y < r.bottom());
        if let Some(i) = hit {
            self.sel = i;
        }
        hit.is_some()
    }

    fn animated(&self) -> bool {
        true
    }
}

// ───────────────────────── Hub ─────────────────────────

/// Something running: this PC, a session, a subagent or a shell.
struct Node {
    id: String,
    kind: Kind,
    color: Color,
    busy: bool,
    /// Under the icon.
    label: String,
    /// The bottom line when selected.
    info: String,
    /// Third level down: a short tag ("A1", "S1") instead of an icon.
    tag: Option<String>,
    /// Enter opens this CLI's dashboard.
    cli: Option<Cli>,
    children: Vec<Node>,
}

/// A node laid out, in tree coordinates (DFS order).
struct Placed {
    parent: Option<usize>,
    depth: usize,
    /// Centre column and icon top row.
    cx: i32,
    y: i32,
    lines: Vec<String>,
    /// Width available for the label.
    slot: u16,
}

/// One parent→child line: its cells, from the parent down to the child.
struct Branch {
    cells: Vec<(i32, i32, char)>,
    color: Color,
    busy: bool,
}

const GAP: u16 = 2;

/// Minimum slot width per depth (the label must fit).
fn min_slot(depth: usize, tag: bool) -> u16 {
    match (depth, tag) {
        (_, true) => 3,
        (0, _) => 24,
        (1, _) => 22,
        _ => 14,
    }
}

pub struct Hub {
    root: Node,
    sel: usize,
    sel_id: String,
    start: Instant,
    /// From the last draw, in DFS order: where each node is on screen (for
    /// clicks), its id, parent and first child.
    hits: Vec<Rect>,
    ids: Vec<String>,
    parents: Vec<Option<usize>>,
    first_child: Vec<Option<usize>>,
}

impl Default for Hub {
    fn default() -> Self {
        Self {
            root: pc(0, 0),
            sel: 0,
            sel_id: String::new(),
            start: Instant::now(),
            hits: Vec::new(),
            ids: Vec::new(),
            parents: Vec::new(),
            first_child: Vec::new(),
        }
    }
}

fn pc(n: usize, busy: usize) -> Node {
    Node {
        id: "pc".into(),
        kind: Kind::Pc,
        color: Color::Gray,
        busy: busy > 0,
        label: busy_text(n, busy),
        info: tr(
            "Este PC: todos los agentes abiertos ahora mismo",
            "This PC: every agent open right now",
        )
        .into(),
        tag: None,
        cli: None,
        children: Vec::new(),
    }
}

fn recent(t: SystemTime, d: Duration) -> bool {
    t.elapsed().is_ok_and(|e| e < d)
}

/// A subagent counts as working if its transcript changed this recently.
const AGENT_BUSY: Duration = Duration::from_secs(30);
/// Unfinished subagents silent for longer are assumed dead (e.g. interrupted).
const AGENT_STALE: Duration = Duration::from_secs(30 * 60);
/// A shell whose output changed this recently is working.
const SHELL_BUSY: Duration = Duration::from_secs(5);

fn state(busy: bool) -> &'static str {
    if busy {
        tr("trabajando", "working")
    } else {
        tr("esperando", "waiting")
    }
}

fn shell_node(sh: &crate::data::Shell, cli: Cli, tag: Option<String>) -> Node {
    let busy = std::fs::metadata(&sh.output)
        .and_then(|m| m.modified())
        .is_ok_and(|t| recent(t, SHELL_BUSY));
    Node {
        id: format!("shell:{}:{}", sh.session, sh.id),
        kind: Kind::Shell,
        color: SHELL_WHITE,
        busy,
        label: if sh.desc.is_empty() {
            sh.id.clone()
        } else {
            sh.desc.clone()
        },
        info: format!("shell · {} · {} · {}", state(busy), sh.id, sh.desc),
        tag,
        cli: Some(cli),
        children: Vec::new(),
    }
}

/// A shell still running: not reported finished, and its output file is there.
fn open_shell(sh: &crate::data::Shell) -> bool {
    !sh.done && (sh.output.as_os_str().is_empty() || sh.output.exists())
}

/// Each live session of that CLI, with its subagents and shells.
fn sessions(st: &Store) -> Vec<Node> {
    let cli = st.cli;
    st.live
        .iter()
        .map(|l| {
            let busy = l.status == "busy";
            let title = match l.name.as_str() {
                "" => tr("(nueva)", "(new)"),
                t => t,
            };
            let model = st
                .sessions
                .get(&l.session)
                .map(|s| s.model.as_str())
                .unwrap_or("");
            let mut children = Vec::new();
            let mut tags = 0;
            let mut agents: Vec<_> = st
                .agents
                .values()
                .filter(|a| a.session == l.session && !a.done && recent(a.mtime, AGENT_STALE))
                .collect();
            agents.sort_by_key(|a| a.mtime);
            for a in agents {
                let busy = recent(a.mtime, AGENT_BUSY);
                let mut shells: Vec<_> = st
                    .shells
                    .values()
                    .filter(|s| s.agent.as_deref() == Some(a.id.as_str()) && open_shell(s))
                    .collect();
                shells.sort_by(|x, y| x.id.cmp(&y.id));
                // Third level: tags instead of icons.
                let kids = shells
                    .into_iter()
                    .map(|sh| {
                        tags += 1;
                        shell_node(sh, cli, Some(format!("S{tags}")))
                    })
                    .collect();
                children.push(Node {
                    id: format!("agent:{}", a.id),
                    kind: Kind::Cli(cli),
                    color: cli.color(),
                    busy,
                    label: if a.desc.is_empty() {
                        a.kind.clone()
                    } else {
                        a.desc.clone()
                    },
                    info: format!("{} · {} · {}", a.kind, state(busy), a.desc),
                    tag: None,
                    cli: Some(cli),
                    children: kids,
                });
            }
            let mut shells: Vec<_> = st
                .shells
                .values()
                .filter(|s| s.session == l.session && s.agent.is_none() && open_shell(s))
                .collect();
            shells.sort_by(|x, y| x.id.cmp(&y.id));
            children.extend(shells.into_iter().map(|sh| shell_node(sh, cli, None)));
            Node {
                id: format!("session:{}", l.session),
                kind: Kind::Cli(cli),
                color: cli.color(),
                busy,
                label: title.to_string(),
                info: format!(
                    "{} · {} · {} · {}{}",
                    cli.name(),
                    state(busy),
                    short_path(&l.cwd),
                    model,
                    if l.pid > 0 {
                        format!(" · pid {}", l.pid)
                    } else {
                        String::new()
                    }
                ),
                tag: None,
                cli: Some(cli),
                children,
            }
        })
        .collect()
}

/// The node's own lines: its icon, or its tag.
fn lines(n: &Node, ms: u128) -> Vec<String> {
    match &n.tag {
        Some(t) => vec![t.clone()],
        None => icon(n.kind, n.busy, ms, false),
    }
}

fn subtree_width(n: &Node, depth: usize, ms: u128) -> u16 {
    let own = width(&lines(n, ms)).max(min_slot(depth, n.tag.is_some()));
    own.max(children_width(n, depth, ms))
}

fn children_width(n: &Node, depth: usize, ms: u128) -> u16 {
    n.children
        .iter()
        .map(|c| subtree_width(c, depth + 1, ms))
        .sum::<u16>()
        + GAP * n.children.len().saturating_sub(1) as u16
}

/// The tree's nodes in DFS order (with the `Node` each one is) and the
/// branches between them, plus its total width and height.
fn layout(root: &Node, ms: u128) -> (Vec<(Placed, &Node)>, Vec<Branch>, u16, i32) {
    fn place<'a>(
        n: &'a Node,
        depth: usize,
        left: i32,
        parent: Option<usize>,
        ms: u128,
        out: &mut Vec<(Placed, &'a Node)>,
    ) {
        let w = subtree_width(n, depth, ms);
        let me = out.len();
        out.push((
            Placed {
                parent,
                depth,
                cx: left + w as i32 / 2,
                y: 0,
                lines: lines(n, ms),
                slot: w,
            },
            n,
        ));
        let mut x = left + (w - children_width(n, depth, ms)) as i32 / 2;
        for c in &n.children {
            place(c, depth + 1, x, Some(me), ms, out);
            x += (subtree_width(c, depth + 1, ms) + GAP) as i32;
        }
    }
    let mut placed = Vec::new();
    place(root, 0, 0, None, ms, &mut placed);
    let depth = placed.iter().map(|(p, _)| p.depth).max().unwrap_or(0) + 1;
    let mut level_h = vec![0i32; depth];
    for (p, _) in &placed {
        level_h[p.depth] = level_h[p.depth].max(p.lines.len() as i32);
    }

    // Each child leaves its parent from its own column (2 apart, centred under
    // the parent) and turns at its own bus row: the outermost child on each
    // side turns highest, so no two branches cross or share a cell.
    let mut kids: Vec<Vec<usize>> = vec![Vec::new(); placed.len()];
    for (i, (p, _)) in placed.iter().enumerate() {
        if let Some(par) = p.parent {
            kids[par].push(i);
        }
    }
    let mut plan = Vec::new(); // (child, exit column, bus rank)
    let mut zone = vec![2i32; depth];
    for (par, ks) in kids.iter().enumerate() {
        let k = ks.len() as i32;
        let exits: Vec<i32> = (0..k).map(|j| placed[par].0.cx + 2 * j - (k - 1)).collect();
        let dx = |j: usize| placed[ks[j]].0.cx - exits[j];
        let left: Vec<usize> = (0..ks.len()).filter(|&j| dx(j) < 0).collect();
        let right: Vec<usize> = (0..ks.len()).rev().filter(|&j| dx(j) > 0).collect();
        for (j, &c) in ks.iter().enumerate() {
            let rank = left
                .iter()
                .chain(&right)
                .position(|&x| x == j)
                .map(|p| if p < left.len() { p } else { p - left.len() })
                .unwrap_or(0);
            plan.push((c, exits[j], rank as i32));
        }
        let d = placed[par].0.depth;
        zone[d] = zone[d].max(left.len().max(right.len()) as i32 + 2);
    }

    // Rows: icons bottom-aligned in their level, label right below, then the
    // branch zone.
    let mut top = vec![0i32; depth];
    for d in 1..depth {
        top[d] = top[d - 1] + level_h[d - 1] + 1 + zone[d - 1];
    }
    for (p, _) in placed.iter_mut() {
        p.y = top[p.depth] + level_h[p.depth] - p.lines.len() as i32;
    }

    // Branches in DFS order of their child, like `placed`.
    plan.sort_by_key(|&(c, _, _)| c);
    let mut branches = Vec::new();
    for (c, e, rank) in plan {
        let (child, node) = &placed[c];
        let par = &placed[child.parent.unwrap()].0;
        let ys = top[par.depth] + level_h[par.depth] + 1;
        let end = child.y - 1;
        let r = ys + 1 + rank;
        let cx = child.cx;
        let mut cells = Vec::new();
        if cx == e {
            cells.extend((ys..=end).map(|y| (e, y, '│')));
        } else {
            let dir = if cx < e { -1 } else { 1 };
            cells.extend((ys..r).map(|y| (e, y, '│')));
            cells.push((e, r, if dir < 0 { '╯' } else { '╰' }));
            let mut x = e + dir;
            while x != cx {
                cells.push((x, r, '─'));
                x += dir;
            }
            cells.push((cx, r, if dir < 0 { '╭' } else { '╮' }));
            cells.extend((r + 1..=end).map(|y| (cx, y, '│')));
        }
        branches.push(Branch {
            cells,
            color: node.color,
            busy: node.busy,
        });
    }
    let w = subtree_width(root, 0, ms);
    let h = top[depth - 1] + level_h[depth - 1] + 1;
    (placed, branches, w, h)
}

impl View for Hub {
    fn title(&self) -> &'static str {
        "Hub"
    }
    fn accent(&self) -> Color {
        Color::Rgb(0xb4, 0x8e, 0xad)
    }
    fn refresh(&mut self, _st: &Store) {}
    fn refresh_all(&mut self, stores: &[Store]) {
        let kids: Vec<Node> = stores.iter().flat_map(sessions).collect();
        let busy = kids.iter().filter(|k| k.busy).count();
        self.root = pc(kids.len(), busy);
        self.root.children = kids;
    }

    fn draw(&mut self, f: &mut Frame, area: Rect) {
        let ms = self.start.elapsed().as_millis();
        let accent = self.accent();
        let block = block_c(
            tr(
                "Hub de agentes · naranja Claude Code · azul Antigravity · verde Codex · celeste Kimi · blanco shells · ··· trabajando",
                "Agent hub · orange Claude Code · blue Antigravity · green Codex · light blue Kimi · white shells · ··· working",
            ),
            accent,
        );
        let inner = block.inner(area);
        f.render_widget(block, area);
        let [tree, info] =
            Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).areas(inner);

        let (placed, branches, w, h) = layout(&self.root, ms);
        // Keep the selection on the same node across refreshes.
        if let Some(i) = placed.iter().position(|(_, n)| n.id == self.sel_id) {
            self.sel = i;
        }
        self.sel = self.sel.min(placed.len() - 1);
        self.sel_id = placed[self.sel].1.id.clone();

        // Centre the tree; if it doesn't fit, scroll so the selection shows.
        let sel = &placed[self.sel].0;
        let ox = if w <= tree.width {
            tree.x as i32 + (tree.width - w) as i32 / 2
        } else {
            let want = sel.cx - tree.width as i32 / 2;
            tree.x as i32 - want.clamp(0, (w - tree.width) as i32)
        };
        let oy = if h <= tree.height as i32 {
            tree.y as i32
        } else {
            let want = sel.y - tree.height as i32 / 2;
            tree.y as i32 - want.clamp(0, h - tree.height as i32)
        };

        let buf = f.buffer_mut();
        let tick = (ms / 150) as usize;
        for b in &branches {
            for (i, &(x, y, ch)) in b.cells.iter().enumerate() {
                let (ch, style) = if !b.busy {
                    (ch, Style::default().fg(b.color))
                } else if i % 3 == tick % 3 {
                    // Marching dots, parent → child.
                    (
                        '•',
                        Style::default().fg(b.color).add_modifier(Modifier::BOLD),
                    )
                } else {
                    ('·', Style::default().fg(b.color))
                };
                put(
                    buf,
                    tree,
                    ox + x,
                    oy + y,
                    ch.encode_utf8(&mut [0; 4]),
                    style,
                );
            }
        }

        self.hits.clear();
        self.ids.clear();
        self.parents.clear();
        self.first_child = vec![None; placed.len()];
        for (i, (p, node)) in placed.iter().enumerate() {
            if let Some(par) = p.parent
                && self.first_child[par].is_none()
            {
                self.first_child[par] = Some(i);
            }
            let iw = width(&p.lines) as i32;
            let x = ox + p.cx - iw / 2;
            let mut style = Style::default().fg(node.color);
            if node.tag.is_some() {
                // Tags: bold, flashing while working, once every 5 s while waiting.
                style = style.add_modifier(Modifier::BOLD);
                if alt_frame(node.busy, ms) {
                    style = style.add_modifier(Modifier::REVERSED);
                }
            }
            if node.kind == Kind::Cli(Cli::Agy) && node.tag.is_none() {
                paint(
                    buf,
                    tree,
                    x,
                    oy + p.y,
                    &agy_pixels(false, alt_frame(node.busy, ms)),
                );
            } else {
                for (j, l) in p.lines.iter().enumerate() {
                    put(buf, tree, x, oy + p.y + j as i32, l, style);
                }
            }
            let selected = i == self.sel;
            if node.tag.is_none() {
                let text = clip(&node.label, p.slot.saturating_sub(2).max(3) as usize);
                let lw = text.chars().count() as i32;
                let lstyle = if selected {
                    Style::default().fg(Color::Black).bg(node.color)
                } else {
                    Style::default().fg(DIM)
                };
                let ly = oy + p.y + p.lines.len() as i32;
                put(buf, tree, ox + p.cx - lw / 2, ly, &text, lstyle);
            } else if selected {
                put(
                    buf,
                    tree,
                    x - 1,
                    oy + p.y,
                    "▸",
                    Style::default().fg(Color::White),
                );
            }
            let rx = x.max(tree.x as i32);
            let ry = (oy + p.y).max(tree.y as i32);
            self.hits.push(
                Rect::new(rx as u16, ry as u16, iw as u16, p.lines.len() as u16 + 1)
                    .intersection(tree),
            );
            self.ids.push(node.id.clone());
            self.parents.push(p.parent);
        }
        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("▸ ", Style::default().fg(accent)),
                Span::raw(placed[self.sel].1.info.clone()),
            ])),
            info,
        );
    }

    fn on_key(&mut self, key: KeyEvent) -> Action {
        let n = self.ids.len();
        if n == 0 {
            return Action::Ignored;
        }
        let next = match key.code {
            KeyCode::Right => (self.sel + 1) % n,
            KeyCode::Left => (self.sel + n - 1) % n,
            KeyCode::Up => self.parents[self.sel].unwrap_or(self.sel),
            KeyCode::Down => self.first_child[self.sel].unwrap_or(self.sel),
            KeyCode::Enter => {
                let id = &self.ids[self.sel];
                return match id.split_once(':') {
                    Some(_) => self.cli_of(id).map_or(Action::Handled, Action::Open),
                    None => Action::Handled,
                };
            }
            _ => return Action::Ignored,
        };
        self.sel = next;
        self.sel_id = self.ids[next].clone();
        Action::Handled
    }

    fn footer(&self) -> &'static str {
        tr(
            "←/→ recorrer · ↑ padre · ↓ hijo · Enter abrir su panel",
            "←/→ walk · ↑ parent · ↓ child · Enter open its dashboard",
        )
    }

    fn click(&mut self, x: u16, y: u16) -> bool {
        let hit = self
            .hits
            .iter()
            .position(|r| x >= r.x && x < r.right() && y >= r.y && y < r.bottom());
        if let Some(i) = hit {
            self.sel = i;
            self.sel_id = self.ids[i].clone();
        }
        hit.is_some()
    }

    fn clip(&self) -> Option<String> {
        self.ids.get(self.sel).cloned()
    }

    fn animated(&self) -> bool {
        true
    }
}

impl Hub {
    fn cli_of(&self, id: &str) -> Option<Cli> {
        fn go(n: &Node, id: &str) -> Option<Cli> {
            if n.id == id {
                return n.cli;
            }
            n.children.iter().find_map(|c| go(c, id))
        }
        go(&self.root, id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The pixel versions draw exactly the official banners.
    #[test]
    fn official_logos() {
        assert_eq!(quad(&px(&CLAWD)), [" ▐▛███▜▌ ", "▝▜█████▛▘", "  ▘▘ ▝▝  "]);
        let shape = |p: &ColorPx| -> Vec<String> {
            half(
                &p.iter()
                    .map(|r| r.iter().map(Option::is_some).collect())
                    .collect(),
            )
        };
        assert_eq!(
            shape(&agy_original()),
            [
                "    ▄██▄    ",
                "   ██████   ",
                "  ███▀▀███  ",
                " ▄██    ██▄ ",
                "▄█▀      ▀█▄",
            ]
        );
        assert_eq!(quad(&px(&KIMI)), [" ▐█▛█▛█▌ ", " ▐█████▌ ", "         "]);
        // Codex's: as tall as Claude's big one, and recognisable small.
        assert_eq!(
            half(&openai_pixels(true, false)).len(),
            quad(&scale(&px(&CLAWD), 4)).len()
        );
        assert_eq!(
            half(&openai_pixels(false, false)),
            [" ▄▀█▀ ▄ ", "█   ▀█ ▀", "▄ █▄   █", " ▀ ▄█▄▀ "]
        );
        // Antigravity's is shown at the size of Claude's, small and big.
        let claude = |k| quad(&scale(&px(&CLAWD), k));
        for (big, k) in [(false, 1), (true, 4)] {
            let agy = shape(&agy_pixels(big, false));
            assert_eq!(
                (width(&agy), agy.len()),
                (width(&claude(k)), claude(k).len())
            );
        }
    }

    /// The home screen fits its four cards small, big, and in a narrow terminal.
    #[test]
    fn picker_draws_four_cards() {
        for (w, h) in [(100, 24), (100, 45), (60, 20)] {
            let backend = ratatui::backend::TestBackend::new(w, h);
            let mut t = ratatui::Terminal::new(backend).unwrap();
            let mut p = Picker::default();
            t.draw(|f| p.draw(f, f.area())).unwrap();
            let text: String = t
                .backend()
                .buffer()
                .content()
                .iter()
                .map(|c| c.symbol())
                .collect();
            if w >= 92 {
                for cli in CLIS {
                    assert!(
                        text.contains(cli.name()),
                        "{} missing at {w}×{h}",
                        cli.name()
                    );
                }
            }
        }
    }

    fn leaf(id: &str, kind: Kind, busy: bool) -> Node {
        Node {
            id: id.into(),
            kind,
            color: SHELL_WHITE,
            busy,
            label: id.into(),
            info: String::new(),
            tag: None,
            cli: None,
            children: Vec::new(),
        }
    }

    /// Every branch runs from under its parent to right above its child, and
    /// no two branches share a cell.
    #[test]
    fn branches_dont_cross() {
        let mut root = pc(0, 0);
        let mut s1 = leaf("s1", Kind::Cli(Cli::Claude), true);
        let mut a = leaf("a", Kind::Cli(Cli::Claude), true);
        a.children = (1..=3)
            .map(|i| {
                let mut t = leaf(&format!("t{i}"), Kind::Shell, false);
                t.tag = Some(format!("S{i}"));
                t
            })
            .collect();
        s1.children = vec![
            a,
            leaf("b", Kind::Shell, true),
            leaf("c", Kind::Shell, false),
        ];
        root.children = vec![
            s1,
            leaf("s2", Kind::Cli(Cli::Agy), false),
            leaf("s3", Kind::Cli(Cli::Agy), true),
            leaf("s4", Kind::Cli(Cli::Claude), false),
            leaf("s5", Kind::Cli(Cli::Claude), false),
        ];
        let (placed, branches, _, _) = layout(&root, 0);
        assert_eq!(branches.len(), placed.len() - 1);
        let mut seen = std::collections::HashSet::new();
        for b in &branches {
            for &(x, y, _) in &b.cells {
                assert!(seen.insert((x, y)), "two branches meet at {x},{y}");
            }
        }
        let children = placed.iter().filter(|(p, _)| p.parent.is_some());
        for (b, (p, n)) in branches.iter().zip(children) {
            let &(x, y, _) = b.cells.last().unwrap();
            assert_eq!((x, y + 1), (p.cx, p.y), "branch must end above {}", n.id);
            let &(_, y0, _) = b.cells.first().unwrap();
            let par = &placed[p.parent.unwrap()].0;
            assert_eq!(
                y0,
                par.y + par.lines.len() as i32 + 1,
                "starts under the label"
            );
        }
    }
}
