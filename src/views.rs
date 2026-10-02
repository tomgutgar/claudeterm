//! The four tabs: Sessions, Agents, Worktrees and Usage.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::time::{Duration as StdDuration, Instant, SystemTime};

use chrono::{DateTime, Duration, Local, NaiveDate, Utc};
use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Row, Sparkline, Table};

use crate::data::{self, Store, Usage, clip, context_window};
use crate::i18n::tr;
use crate::platform;
use crate::ui::{self, DIM, ListView, block_c, fmt_tokens};

/// What a view asks the main loop to do after a key press.
pub enum Action {
    /// The key isn't the view's: try the global ones.
    Ignored,
    Handled,
    /// Recompute the view (something of its own changed, e.g. the period).
    Refresh,
    /// Run capturing the output; `confirm` asks first (destructive actions).
    Run {
        cmd: Vec<String>,
        confirm: Option<String>,
        show: bool,
    },
    /// Launch without waiting (new terminal windows).
    Spawn(Vec<String>),
    /// Ask for a branch name; in the template "{}" is replaced by what was
    /// typed and "{-}" by the same with `/` → `-` (see `valid_branch`).
    Prompt {
        label: String,
        template: Vec<String>,
    },
    /// Show text in a scrollable window (opens at the end).
    Show {
        title: String,
        text: String,
    },
}

pub trait View {
    fn title(&self) -> &'static str;
    fn accent(&self) -> Color;
    fn refresh(&mut self, st: &Store);
    fn draw(&mut self, f: &mut Frame, area: Rect);
    fn on_key(&mut self, key: KeyEvent) -> Action;
    fn footer(&self) -> &'static str;
    /// Its list, if any (for mouse selection).
    fn list(&mut self) -> Option<&mut ListView> {
        None
    }
    /// What the `y` key copies.
    fn clip(&self) -> Option<String> {
        None
    }
}

pub fn all() -> Vec<Box<dyn View>> {
    vec![
        Box::new(Sessions::default()),
        Box::new(Agents::default()),
        Box::new(Worktrees::default()),
        Box::new(UsageView::default()),
    ]
}

// ───────────────────────── helpers ─────────────────────────

fn s(v: impl Into<String>) -> String {
    v.into()
}

/// /home/me/x → ~/x
pub fn short_path(p: &str) -> String {
    let h = platform::home();
    let h = h.to_string_lossy();
    if !h.is_empty() && p.starts_with(&*h) {
        format!("~{}", &p[h.len()..])
    } else {
        p.to_string()
    }
}

fn short_model(m: &str) -> String {
    m.trim_start_matches("claude-").to_string()
}

/// "HEAD" shows up in directories that aren't a repo (or have no commits): show a dash.
fn branch(b: &str) -> &str {
    if b.is_empty() || b == "HEAD" {
        "—"
    } else {
        b
    }
}

fn fmt_wh(wh: f64) -> String {
    if wh >= 1000.0 {
        format!("{:.1} kWh", wh / 1000.0)
    } else {
        format!("{wh:.0} Wh")
    }
}

fn fmt_ml(ml: f64) -> String {
    if ml >= 1000.0 {
        format!("{:.1} L", ml / 1000.0)
    } else {
        format!("{ml:.0} mL")
    }
}

fn fmt_g(g: f64) -> String {
    if g >= 1000.0 {
        format!("{:.1} kg", g / 1000.0)
    } else {
        format!("{g:.0} g")
    }
}

/// For a sense of scale. Assumptions: charging a phone ≈ 15 Wh (~4000 mAh
/// battery at 3.85 V); average petrol car ≈ 150 g CO₂/km; glass = 250 mL.
fn equivalences(f: &data::Footprint) -> String {
    crate::tr!(
        "≈ {:.1} cargas de móvil · {:.1} km en coche · {:.1} vasos de agua",
        "≈ {:.1} phone charges · {:.1} km by car · {:.1} glasses of water",
        f.wh / 15.0,
        f.co2_g / 150.0,
        f.water_ml / 250.0
    )
}

/// "███░░░░░░░  34%" in green, yellow (≥ 50 %) or red (≥ 80 %).
pub fn limit_bar(p: f64, width: usize) -> Span<'static> {
    let color = match p {
        p if p >= 80.0 => Color::Red,
        p if p >= 50.0 => Color::Yellow,
        _ => Color::Green,
    };
    Span::styled(
        format!("{} {p:>3.0}%", bar(p, width)),
        Style::default().fg(color),
    )
}

/// Text bar: bar(34.0, 10) → "███░░░░░░░".
fn bar(pct: f64, width: usize) -> String {
    let full = ((pct.clamp(0.0, 100.0) / 100.0) * width as f64).round() as usize;
    format!("{}{}", "█".repeat(full), "░".repeat(width - full))
}

fn money(usd: f64) -> String {
    format!("${usd:.2}")
}

/// Tokens and cost summed per key.
fn tally<'a>(
    st: &'a Store,
    key: impl Fn(&'a data::Rec) -> Option<&'a str>,
) -> HashMap<&'a str, (Usage, f64)> {
    let mut m: HashMap<&str, (Usage, f64)> = HashMap::new();
    for r in &st.recs {
        if let Some(k) = key(r) {
            let e = m.entry(k).or_default();
            e.0.add(&r.usage);
            e.1 += r.usage.cost(&r.model).unwrap_or(0.0);
        }
    }
    m
}

fn ago(t: SystemTime) -> String {
    let secs = t.elapsed().unwrap_or_default().as_secs();
    match secs {
        0..60 => crate::tr!("hace {secs}s", "{secs}s ago"),
        60..3600 => crate::tr!("hace {}m", "{}m ago", secs / 60),
        3600..86400 => crate::tr!("hace {}h", "{}h ago", secs / 3600),
        _ => crate::tr!("hace {}d", "{}d ago", secs / 86400),
    }
}

/// Opens Claude Code in a new terminal, if the directory still exists and
/// the terminal is installed.
fn open_here(cwd: &str, claude_args: &[&str]) -> Action {
    if let Some(term) = platform::terminal_missing() {
        return Action::Show {
            title: s(tr("No se puede abrir", "Can't open")),
            text: crate::tr!(
                "No encuentro la terminal «{term}». En Linux elige otra con CCT_TERMINAL, p. ej. CCT_TERMINAL=\"alacritty -e\" o \"gnome-terminal --\".",
                "Terminal \"{term}\" not found. On Linux pick another one with CCT_TERMINAL, e.g. CCT_TERMINAL=\"alacritty -e\" or \"gnome-terminal --\"."
            ),
        };
    }
    if !std::path::Path::new(cwd).is_dir() {
        return Action::Show {
            title: s(tr("No se puede abrir", "Can't open")),
            text: crate::tr!(
                "La carpeta {} ya no existe.",
                "Folder {} no longer exists.",
                short_path(cwd)
            ),
        };
    }
    Action::Spawn(platform::open_claude(cwd, claude_args))
}

// ───────────────────────── Sessions ─────────────────────────

struct SessRef {
    path: PathBuf,
    cwd: String,
    pid: Option<u32>,
    /// pid of its terminal window (only live sessions with a known window).
    window: Option<u64>,
}

#[derive(Default)]
pub struct Sessions {
    list: Option<ListView>,
    refs: HashMap<String, SessRef>,
}

impl View for Sessions {
    fn title(&self) -> &'static str {
        tr("Sesiones", "Sessions")
    }
    fn accent(&self) -> Color {
        Color::Rgb(0xd9, 0x77, 0x57) // Claude orange
    }

    fn refresh(&mut self, st: &Store) {
        // The session's own tokens and its subagents', separately.
        let main = tally(st, |r| r.agent.is_none().then_some(r.session.as_str()));
        let agents = tally(st, |r| r.agent.is_some().then_some(r.session.as_str()));
        let tokens = |id: &str| {
            let (m, mc) = main.get(id).copied().unwrap_or_default();
            let (a, ac) = agents.get(id).copied().unwrap_or_default();
            let t = if a.total() == 0 {
                fmt_tokens(m.total())
            } else {
                format!("{} +{} ag", fmt_tokens(m.total()), fmt_tokens(a.total()))
            };
            format!("{t:>16} {:>8}", money(mc + ac))
        };
        let mut rows = Vec::new();
        self.refs.clear();
        let dim = Style::default().fg(DIM);
        let cyan = Style::default().fg(Color::Cyan);

        for l in &st.live {
            let sess = st.sessions.get(&l.session);
            let (icon, label, color) = if l.status == "busy" {
                ("●", tr("trabajando", "working"), Color::Green)
            } else {
                ("◌", tr("esperando ", "waiting"), Color::Yellow)
            };
            let title = data::title(&st.windows, l);
            let mut spans = vec![
                Span::styled(
                    format!("{icon} {label} "),
                    Style::default().fg(color).add_modifier(Modifier::BOLD),
                ),
                Span::raw(format!("{:<38} ", clip(title, 38))),
                Span::raw(format!("{:<26} ", clip(&short_path(&l.cwd), 26))),
            ];
            match sess.filter(|s| s.last.is_some()) {
                Some(s) => {
                    let pct = s.context * 100 / context_window(&s.model).max(1);
                    spans.push(Span::styled(
                        format!("{:<14} ", clip(branch(&s.branch), 14)),
                        cyan,
                    ));
                    spans.push(Span::raw(format!(
                        "{:<9} ctx {pct:>2}% ",
                        clip(&short_model(&s.model), 9)
                    )));
                    spans.push(Span::raw(tokens(&l.session)));
                }
                None => spans.push(Span::styled(
                    tr("nueva, aún sin mensajes", "new, no messages yet"),
                    dim,
                )),
            }
            rows.push((Line::from(spans), l.session.clone()));
            self.refs.insert(
                l.session.clone(),
                SessRef {
                    path: sess.map(|s| s.path.clone()).unwrap_or_default(),
                    cwd: l.cwd.clone(),
                    pid: Some(l.pid),
                    window: st.windows.get(&l.pid).map(|w| w.pid),
                },
            );
        }
        if st.live.is_empty() {
            rows.push((
                Line::styled(tr("  (ningún claude abierto)", "  (no claude open)"), dim),
                String::new(),
            ));
        }
        rows.push((
            Line::styled(tr("── Sesiones anteriores ──", "── Past sessions ──"), dim),
            String::new(),
        ));

        let mut past: Vec<_> = st
            .sessions
            .values()
            .filter(|s| s.last.is_some() && !self.refs.contains_key(&s.id))
            .collect();
        past.sort_by_key(|s| std::cmp::Reverse(s.last));
        for sess in past {
            let when = sess
                .last
                .unwrap()
                .with_timezone(&Local)
                .format(tr("%d/%m %H:%M", "%b %d %H:%M"));
            rows.push((
                Line::from(vec![
                    Span::styled(format!("  {when}  "), dim),
                    Span::raw(format!("{:<26} ", clip(&short_path(&sess.cwd), 26))),
                    Span::styled(format!("{:<14} ", clip(branch(&sess.branch), 14)), cyan),
                    Span::raw(tokens(&sess.id)),
                    Span::styled(format!("  {}", clip(&sess.title, 70)), dim),
                ]),
                sess.id.clone(),
            ));
            self.refs.insert(
                sess.id.clone(),
                SessRef {
                    path: sess.path.clone(),
                    cwd: sess.cwd.clone(),
                    pid: None,
                    window: None,
                },
            );
        }
        self.list.get_or_insert_with(ListView::new).set_items(rows);
    }

    fn draw(&mut self, f: &mut Frame, area: Rect) {
        let color = self.accent();
        self.list.get_or_insert_with(ListView::new).draw(
            f,
            area,
            tr("Sesiones de Claude Code", "Claude Code sessions"),
            color,
        );
    }

    fn on_key(&mut self, key: KeyEvent) -> Action {
        let list = self.list.get_or_insert_with(ListView::new);
        if list.key(key) {
            return Action::Handled;
        }
        let Some(id) = list.selected_id() else {
            return Action::Ignored;
        };
        let Some(r) = self.refs.get(&id) else {
            return Action::Ignored;
        };
        match key.code {
            KeyCode::Enter => Action::Show {
                title: format!("Transcript {id}"),
                text: data::transcript(&r.path, 200),
            },
            // If it's already open, resuming would duplicate it: go to its window.
            KeyCode::Char('g' | 'o') if r.pid.is_some() => match r.window {
                Some(w) => Action::Run {
                    cmd: platform::focus_cmd(w),
                    confirm: None,
                    show: false,
                },
                None => Action::Show {
                    title: s(tr("Sin ventana", "No window")),
                    text: s(tr(
                        "No encuentro su ventana (ir a la ventana necesita Hyprland).",
                        "Can't find its window (going to the window needs Hyprland).",
                    )),
                },
            },
            KeyCode::Char('o') => open_here(&r.cwd, &["--resume", &id]),
            KeyCode::Char('n') => open_here(&r.cwd, &[]),
            KeyCode::Char('k') => match r.pid {
                Some(pid) => Action::Run {
                    cmd: platform::kill(pid),
                    confirm: Some(crate::tr!(
                        "¿Cerrar la sesión de Claude en {} (pid {pid})?",
                        "Close the Claude session in {} (pid {pid})?",
                        short_path(&r.cwd)
                    )),
                    show: false,
                },
                None => Action::Handled,
            },
            _ => Action::Ignored,
        }
    }

    fn footer(&self) -> &'static str {
        tr(
            "Enter transcript · g ir a su ventana · o reanudar · n nueva aquí · k cerrar · / filtrar",
            "Enter transcript · g go to window · o resume · n new here · k close · / filter",
        )
    }

    fn clip(&self) -> Option<String> {
        self.list.as_ref()?.selected_id()
    }

    fn list(&mut self) -> Option<&mut ListView> {
        self.list.as_mut()
    }
}

// ───────────────────────── Agents ─────────────────────────

/// A subagent whose transcript was touched more recently than this counts as active.
const AGENT_ACTIVE: StdDuration = StdDuration::from_secs(30);

#[derive(Default)]
pub struct Agents {
    list: Option<ListView>,
    paths: HashMap<String, PathBuf>,
}

impl View for Agents {
    fn title(&self) -> &'static str {
        tr("Agentes", "Agents")
    }
    fn accent(&self) -> Color {
        Color::Rgb(0xb4, 0x8e, 0xad)
    }

    fn refresh(&mut self, st: &Store) {
        let per = tally(st, |r| r.agent.as_deref());
        let mut agents: Vec<_> = st.agents.values().collect();
        agents.sort_by_key(|a| std::cmp::Reverse(a.mtime));
        self.paths.clear();
        let accent = self.accent();
        let rows = agents
            .into_iter()
            .map(|a| {
                let active = a.mtime.elapsed().unwrap_or_default() < AGENT_ACTIVE;
                let (icon, color) = if active {
                    ("●", Color::Green)
                } else {
                    ("·", DIM)
                };
                let (u, cost) = per.get(a.id.as_str()).copied().unwrap_or_default();
                let cwd = st
                    .sessions
                    .get(&a.session)
                    .map(|s| short_path(&s.cwd))
                    .unwrap_or_default();
                self.paths.insert(a.id.clone(), a.path.clone());
                (
                    Line::from(vec![
                        Span::styled(format!("{icon} "), Style::default().fg(color)),
                        Span::styled(
                            format!("{:<18} ", clip(&a.kind, 18)),
                            Style::default().fg(accent),
                        ),
                        Span::raw(format!("{:<40} ", clip(&a.desc, 40))),
                        Span::raw(format!("{:<26} ", clip(&cwd, 26))),
                        Span::raw(format!("{:>7} {:>8}  ", fmt_tokens(u.total()), money(cost))),
                        Span::styled(ago(a.mtime), Style::default().fg(DIM)),
                    ]),
                    a.id.clone(),
                )
            })
            .collect();
        self.list.get_or_insert_with(ListView::new).set_items(rows);
    }

    fn draw(&mut self, f: &mut Frame, area: Rect) {
        let color = self.accent();
        self.list.get_or_insert_with(ListView::new).draw(
            f,
            area,
            tr("Subagentes (● activo)", "Subagents (● active)"),
            color,
        );
    }

    fn on_key(&mut self, key: KeyEvent) -> Action {
        let list = self.list.get_or_insert_with(ListView::new);
        if list.key(key) {
            return Action::Handled;
        }
        match (
            key.code,
            list.selected_id()
                .and_then(|id| self.paths.get(&id).map(|p| (id, p))),
        ) {
            (KeyCode::Enter, Some((id, path))) => Action::Show {
                title: crate::tr!("Agente {id}", "Agent {id}"),
                text: data::transcript(path, 200),
            },
            _ => Action::Ignored,
        }
    }

    fn footer(&self) -> &'static str {
        tr(
            "Enter transcript · / filtrar",
            "Enter transcript · / filter",
        )
    }

    fn clip(&self) -> Option<String> {
        self.list.as_ref()?.selected_id()
    }

    fn list(&mut self) -> Option<&mut ListView> {
        self.list.as_mut()
    }
}

// ───────────────────────── Worktrees ─────────────────────────

/// Repos with sessions in the last few days.
const WT_DAYS: i64 = 7;

#[derive(Default)]
pub struct Worktrees {
    list: Option<ListView>,
    /// worktree path → main repo path
    main_of: HashMap<String, String>,
    /// `git worktree list` output per directory: the view refreshes every 2 s
    /// and git doesn't need to be called that often.
    git_cache: HashMap<String, (Instant, Vec<(String, String)>)>,
}

const GIT_TTL: StdDuration = StdDuration::from_secs(30);

impl Worktrees {
    fn worktrees(&mut self, cwd: &str) -> Vec<(String, String)> {
        if let Some((t, wts)) = self.git_cache.get(cwd)
            && t.elapsed() < GIT_TTL
        {
            return wts.clone();
        }
        let wts = git_worktrees(cwd)
            .map(|out| data::parse_worktrees(&out))
            .unwrap_or_default();
        self.git_cache
            .insert(cwd.to_string(), (Instant::now(), wts.clone()));
        wts
    }
}

impl View for Worktrees {
    fn title(&self) -> &'static str {
        "Worktrees"
    }
    fn accent(&self) -> Color {
        Color::Rgb(0x88, 0xc0, 0xd0)
    }

    fn refresh(&mut self, st: &Store) {
        let since = Utc::now() - Duration::days(WT_DAYS);
        let mut cwds: Vec<&str> = st.live.iter().map(|l| l.cwd.as_str()).collect();
        cwds.extend(
            st.sessions
                .values()
                .filter(|s| s.last.is_some_and(|t| t > since))
                .map(|s| s.cwd.as_str()),
        );
        cwds.sort();
        cwds.dedup();

        // main repo → [(worktree, branch)]
        let mut repos: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();
        for cwd in cwds {
            if cwd.is_empty() || repos.values().flatten().any(|(p, _)| p == cwd) {
                continue;
            }
            let wts = self.worktrees(cwd);
            if let Some((main, _)) = wts.first() {
                repos.insert(main.clone(), wts);
            }
        }

        // Each live session goes to the longest worktree containing its cwd.
        let owner = |cwd: &str| {
            repos
                .values()
                .flatten()
                .map(|(p, _)| p)
                .filter(|p| {
                    cwd == p.as_str()
                        || cwd.starts_with(&format!("{p}{}", std::path::MAIN_SEPARATOR))
                })
                .max_by_key(|p| p.len())
                .cloned()
        };
        let mut here: HashMap<String, Vec<&data::Live>> = HashMap::new();
        for l in &st.live {
            if let Some(p) = owner(&l.cwd) {
                here.entry(p).or_default().push(l);
            }
        }

        self.main_of.clear();
        let accent = self.accent();
        let mut rows = Vec::new();
        for (main, wts) in &repos {
            // The first one is the main repo, in bold; the rest indented below.
            for (i, (path, branch)) in wts.iter().enumerate() {
                let name = if i == 0 {
                    Span::styled(
                        format!("▸ {:<52} ", clip(&short_path(path), 52)),
                        Style::default().fg(accent).add_modifier(Modifier::BOLD),
                    )
                } else {
                    Span::raw(format!("    └ {:<48} ", clip(&short_path(path), 48)))
                };
                let mut spans = vec![
                    name,
                    Span::styled(
                        format!("{:<24} ", clip(branch, 24)),
                        Style::default().fg(Color::Cyan),
                    ),
                ];
                for l in here.get(path).into_iter().flatten() {
                    let (txt, c) = if l.status == "busy" {
                        (tr("● trabajando", "● working"), Color::Green)
                    } else {
                        (tr("◌ esperando", "◌ waiting"), Color::Yellow)
                    };
                    spans.push(Span::styled(
                        format!("{txt} ({}) ", l.name),
                        Style::default().fg(c),
                    ));
                }
                rows.push((Line::from(spans), path.clone()));
                self.main_of.insert(path.clone(), main.clone());
            }
        }
        if rows.is_empty() {
            rows.push((
                Line::raw(tr(
                    "  (ninguna sesión reciente dentro de un repo git)",
                    "  (no recent session inside a git repo)",
                )),
                String::new(),
            ));
        }
        self.list.get_or_insert_with(ListView::new).set_items(rows);
    }

    fn draw(&mut self, f: &mut Frame, area: Rect) {
        let color = self.accent();
        self.list.get_or_insert_with(ListView::new).draw(
            f,
            area,
            &crate::tr!(
                "Worktrees de repos con sesiones (últimos {WT_DAYS} días)",
                "Worktrees of repos with sessions (last {WT_DAYS} days)"
            ),
            color,
        );
    }

    fn on_key(&mut self, key: KeyEvent) -> Action {
        let list = self.list.get_or_insert_with(ListView::new);
        if list.key(key) {
            return Action::Handled;
        }
        let Some(path) = list.selected_id() else {
            return Action::Ignored;
        };
        let main = self.main_of.get(&path).cloned().unwrap_or_default();
        if matches!(key.code, KeyCode::Char('c' | 'x')) {
            // The worktrees are about to change: make the next refresh ask git.
            self.git_cache.clear();
        }
        match key.code {
            KeyCode::Char('n') => open_here(&path, &[]),
            // New worktree next to the repo: <repo>-<branch>.
            KeyCode::Char('c') => Action::Prompt {
                label: crate::tr!(
                    "Rama del nuevo worktree (se crea en {}-<rama>)",
                    "Branch for the new worktree (created at {}-<branch>)",
                    short_path(&main)
                ),
                template: vec![
                    s("git"),
                    s("-C"),
                    main.clone(),
                    s("worktree"),
                    s("add"),
                    s("-b"),
                    s("{}"),
                    format!("{main}-{{-}}"),
                ],
            },
            KeyCode::Char('x') if path != main => Action::Run {
                cmd: vec![
                    s("git"),
                    s("-C"),
                    main,
                    s("worktree"),
                    s("remove"),
                    path.clone(),
                ],
                confirm: Some(crate::tr!(
                    "¿Borrar el worktree {}? (git se niega si tiene cambios sin guardar)",
                    "Delete worktree {}? (git refuses if it has uncommitted changes)",
                    short_path(&path)
                )),
                show: false,
            },
            _ => Action::Ignored,
        }
    }

    fn footer(&self) -> &'static str {
        tr(
            "n claude aquí · c nuevo worktree · x borrar worktree · / filtrar",
            "n claude here · c new worktree · x delete worktree · / filter",
        )
    }

    fn clip(&self) -> Option<String> {
        self.list.as_ref()?.selected_id()
    }

    fn list(&mut self) -> Option<&mut ListView> {
        self.list.as_mut()
    }
}

// ───────────────────────── Usage ─────────────────────────

/// Table periods: (Spanish name, English name, days back besides today).
const PERIODS: [(&str, &str, Option<i64>); 4] = [
    ("hoy", "today", Some(0)),
    ("7 días", "7 days", Some(6)),
    ("30 días", "30 days", Some(29)),
    ("todo", "all", None),
];

#[derive(Default)]
pub struct UsageView {
    summary: Vec<Line<'static>>,
    footprint: Vec<Line<'static>>,
    /// Daily cost of the last 30 days, in cents (the sparkline wants integers).
    daily: Vec<u64>,
    max_day: f64,
    by_model: Vec<(String, Usage, f64)>,
    by_project: Vec<(String, Usage, f64)>,
    period: usize,
}

fn sum(recs: impl Iterator<Item = (Usage, f64)>) -> (Usage, f64) {
    recs.fold((Usage::default(), 0.0), |(mut u, c), (ru, rc)| {
        u.add(&ru);
        (u, c + rc)
    })
}

impl View for UsageView {
    fn title(&self) -> &'static str {
        tr("Uso", "Usage")
    }
    fn accent(&self) -> Color {
        Color::Rgb(0xa3, 0xbe, 0x8c)
    }

    fn refresh(&mut self, st: &Store) {
        let now = Utc::now();
        let today = Local::now().date_naive();
        let day = |t: &DateTime<Utc>| t.with_timezone(&Local).date_naive();
        let priced = |r: &data::Rec| (r.usage, r.usage.cost(&r.model).unwrap_or(0.0));

        let mut ts: Vec<DateTime<Utc>> = st.recs.iter().map(|r| r.ts).collect();
        ts.sort();
        let limits = data::read_limits().unwrap_or_default();
        // With the official figure (statusline) the block starts 5 h before its
        // reset; without it, it's estimated from the messages.
        let official = limits
            .five_hour
            .as_ref()
            .and_then(|l| l.resets())
            .filter(|r| *r > Local::now())
            .map(|r| r.with_timezone(&Utc) - Duration::hours(5));
        let block_start = official
            .or_else(|| data::last_block_start(&ts))
            .filter(|s| now < *s + Duration::hours(5));
        let block_usage =
            block_start.map(|start| sum(st.recs.iter().filter(|r| r.ts >= start).map(priced)));
        let block = block_start.zip(block_usage).map(|(start, (u, c))| {
            let left = start + Duration::hours(5) - now;
            let local = start.with_timezone(&Local).format("%H:%M");
            crate::tr!(
                "{} tokens · {} · empezó {local} · quedan {}h {:02}m",
                "{} tokens · {} · started {local} · {}h {:02}m left",
                fmt_tokens(u.total()),
                money(c),
                left.num_hours(),
                left.num_minutes() % 60
            )
        });

        // Transcripts + what Claude Code already deleted but our history kept.
        let all = || st.recs.iter().chain(&st.archived);
        let first = all()
            .map(|r| day(&r.ts))
            .min()
            .map(|d| {
                d.format(tr(" (desde %d/%m/%y)", " (since %b %d %Y)"))
                    .to_string()
            })
            .unwrap_or_default();
        let since = |d: i64| today - Duration::days(d);
        let (tu, tc) = sum(all().filter(|r| day(&r.ts) == today).map(priced));
        let (mu, mc) = sum(all().filter(|r| day(&r.ts) >= since(29)).map(priced));
        let (au, ac) = sum(all().map(priced));

        // Estimated footprint. Week: the official window (reset − 7 days) or,
        // without statusline data, the last 7 days.
        let week_start = limits
            .seven_day
            .as_ref()
            .and_then(|l| l.resets())
            .filter(|r| *r > Local::now())
            .map(|r| r.with_timezone(&Utc) - Duration::days(7))
            .unwrap_or(now - Duration::days(7));
        let (wu, _) = sum(all().filter(|r| r.ts >= week_start).map(priced));
        let bold = Style::default().add_modifier(Modifier::BOLD);
        let fp_line = |name: &'static str, u: Option<Usage>| {
            let Some(u) = u else {
                return Line::from(vec![
                    Span::styled(name, bold),
                    Span::styled(
                        tr("sin bloque activo", "no active block"),
                        Style::default().fg(DIM),
                    ),
                ]);
            };
            let f = u.footprint();
            let (lo, hi) = (f.scaled(data::RANGE_LOW), f.scaled(data::RANGE_HIGH));
            // Central figure and, in parentheses, the range from the sources.
            let val = |fmt: fn(f64) -> String, c: f64, l: f64, h: f64| {
                format!("≈ {} ({}–{})", fmt(c), fmt(l), fmt(h))
            };
            Line::from(vec![
                Span::styled(name, bold),
                Span::styled(
                    format!("{:<31}", val(fmt_wh, f.wh, lo.wh, hi.wh)),
                    Style::default().fg(Color::Yellow),
                ),
                Span::styled(
                    format!(
                        "{:<34}",
                        crate::tr!(
                            "{} agua",
                            "{} water",
                            val(fmt_ml, f.water_ml, lo.water_ml, hi.water_ml)
                        )
                    ),
                    Style::default().fg(Color::Cyan),
                ),
                Span::styled(
                    format!(
                        "{:<34}",
                        format!("{} CO₂e", val(fmt_g, f.co2_g, lo.co2_g, hi.co2_g))
                    ),
                    Style::default().fg(Color::Green),
                ),
                Span::styled(format!("   {}", equivalences(&f)), Style::default().fg(DIM)),
            ])
        };
        self.footprint = vec![
            fp_line("5 h          ", block_usage.map(|(u, _)| u)),
            fp_line(tr("Semana       ", "Week         "), Some(wu)),
            fp_line("Total        ", Some(au)),
            Line::styled(
                tr(
                    "Cifra central y (mín–máx) según estudios de 2025–2026. Anthropic no publica estos datos: es una aproximación. Fuentes en el README.",
                    "Central figure and (min–max) from 2025–2026 studies. Anthropic doesn't publish this data: it's an approximation. Sources in the README.",
                ),
                Style::default().fg(DIM),
            ),
        ];
        let bold = Style::default().add_modifier(Modifier::BOLD);
        let limit_line = |name: &'static str, lim: &Option<data::Limit>| match lim {
            Some(l) => Line::from(vec![
                Span::styled(name, bold),
                limit_bar(l.pct(), 20),
                Span::raw(format!("  · {}", l.reset_text())),
            ]),
            None => Line::from(vec![
                Span::styled(name, bold),
                Span::styled(
                    tr(
                        "sin datos todavía: sale tras la próxima respuesta de Claude (statusline de cct)",
                        "no data yet: shows up after Claude's next reply (cct statusline)",
                    ),
                    Style::default().fg(DIM),
                ),
            ]),
        };
        let updated = limits
            .updated
            .map(|t| crate::tr!("  (dato de {})", "  (as of {})", ago(t)))
            .unwrap_or_default();
        let mut five = limit_line(tr("Límite 5 h   ", "5 h limit    "), &limits.five_hour);
        five.spans
            .push(Span::styled(updated, Style::default().fg(DIM)));
        self.summary = vec![
            five,
            limit_line(tr("Semanal      ", "Weekly       "), &limits.seven_day),
            Line::from(vec![
                Span::styled(tr("Tokens 5 h   ", "5 h tokens   "), bold),
                Span::raw(block.unwrap_or_else(|| s(tr("ninguno activo", "none active")))),
            ]),
            Line::from(vec![
                Span::styled(tr("Hoy          ", "Today        "), bold),
                Span::raw(format!("{} tokens · {}", fmt_tokens(tu.total()), money(tc))),
                Span::styled(tr("    30 días ", "    30 days "), bold),
                Span::raw(format!("{} · {}", fmt_tokens(mu.total()), money(mc))),
                Span::styled(format!("    Total{first} "), bold),
                Span::raw(format!("{} · {}", fmt_tokens(au.total()), money(ac))),
            ]),
            Line::styled(
                tr(
                    "Coste = equivalente a precio de API. Con suscripción no pagas esto, pero sirve para comparar.",
                    "Cost = API-price equivalent. With a subscription you don't pay this, but it's useful to compare.",
                ),
                Style::default().fg(DIM),
            ),
        ];

        let mut per_day: BTreeMap<NaiveDate, f64> = (0..30).map(|d| (since(d), 0.0)).collect();
        for r in all() {
            if let Some(c) = per_day.get_mut(&day(&r.ts)) {
                *c += r.usage.cost(&r.model).unwrap_or(0.0);
            }
        }
        self.max_day = per_day.values().copied().fold(0.0, f64::max);
        self.daily = per_day
            .values()
            .map(|c| (c * 100.0).round() as u64)
            .collect();

        let from = PERIODS[self.period].2.map(since);
        let group = |key: &dyn Fn(&data::Rec) -> String| {
            let mut m: HashMap<String, (Usage, f64)> = HashMap::new();
            for r in all().filter(|r| from.is_none_or(|f| day(&r.ts) >= f)) {
                let e = m.entry(key(r)).or_default();
                e.0.add(&r.usage);
                e.1 += r.usage.cost(&r.model).unwrap_or(0.0);
            }
            let mut v: Vec<_> = m.into_iter().map(|(k, (u, c))| (k, u, c)).collect();
            v.sort_by(|a, b| b.2.total_cmp(&a.2).then(b.1.total().cmp(&a.1.total())));
            v
        };
        self.by_model = group(&|r| short_model(&r.model));
        self.by_project = group(&|r| short_path(&r.cwd));
    }

    fn draw(&mut self, f: &mut Frame, area: Rect) {
        let color = self.accent();
        let [top, foot, spark, tables] = Layout::vertical([
            Constraint::Length(7),
            Constraint::Length(6),
            Constraint::Length(7),
            Constraint::Min(0),
        ])
        .areas(area);
        // The logo left of the summary, if it fits uncropped.
        let top = if top.width >= 120 {
            let [logo, rest] =
                Layout::horizontal([Constraint::Length(12), Constraint::Min(0)]).areas(top);
            f.render_widget(
                Paragraph::new(ui::logo()),
                Rect {
                    x: logo.x + 1,
                    y: logo.y + 1,
                    width: 10,
                    height: 5,
                },
            );
            rest
        } else {
            top
        };
        f.render_widget(
            Paragraph::new(self.summary.clone()).block(block_c(tr("Resumen", "Summary"), color)),
            top,
        );
        f.render_widget(
            Paragraph::new(self.footprint.clone()).block(block_c(
                tr(
                    "Huella estimada (≈): electricidad · agua · CO₂",
                    "Estimated footprint (≈): electricity · water · CO₂",
                ),
                color,
            )),
            foot,
        );
        // Each day takes several columns to fill the width (the sparkline draws 1 per value).
        let per = (spark.width.saturating_sub(2) as usize / self.daily.len().max(1)).max(1);
        let wide: Vec<u64> = self
            .daily
            .iter()
            .flat_map(|&d| std::iter::repeat_n(d, per))
            .collect();
        f.render_widget(
            Sparkline::default()
                .data(&wide)
                .style(Style::default().fg(color))
                .block(block_c(
                    &crate::tr!(
                        "Coste diario, últimos 30 días (máx {})",
                        "Daily cost, last 30 days (max {})",
                        money(self.max_day)
                    ),
                    color,
                )),
            spark,
        );

        let [left, right] =
            Layout::horizontal([Constraint::Percentage(55), Constraint::Percentage(45)])
                .areas(tables);
        let period = tr(PERIODS[self.period].0, PERIODS[self.period].1);
        let head = Style::default().fg(color).add_modifier(Modifier::BOLD);
        let n = fmt_tokens;
        let model_rows = self.by_model.iter().map(|(m, u, c)| {
            Row::new(vec![
                m.clone(),
                n(u.input),
                n(u.output),
                n(u.cache_read),
                n(u.cache_5m + u.cache_1h),
                money(*c),
            ])
        });
        f.render_widget(
            Table::new(
                model_rows,
                [
                    Constraint::Fill(2),
                    Constraint::Fill(1),
                    Constraint::Fill(1),
                    Constraint::Fill(1),
                    Constraint::Fill(1),
                    Constraint::Fill(1),
                ],
            )
            .header(
                Row::new(if crate::i18n::es() {
                    vec![
                        "Modelo",
                        "Entrada",
                        "Salida",
                        "Caché lee",
                        "Caché escr",
                        "Coste",
                    ]
                } else {
                    vec![
                        "Model",
                        "Input",
                        "Output",
                        "Cache read",
                        "Cache write",
                        "Cost",
                    ]
                })
                .style(head),
            )
            .block(block_c(
                &crate::tr!("Por modelo · {period}", "By model · {period}"),
                color,
            )),
            left,
        );
        let proj_rows = self
            .by_project
            .iter()
            .map(|(p, u, c)| Row::new(vec![clip(p, 40), n(u.total()), money(*c)]));
        f.render_widget(
            Table::new(
                proj_rows,
                [
                    Constraint::Fill(3),
                    Constraint::Fill(1),
                    Constraint::Fill(1),
                ],
            )
            .header(
                Row::new(vec![
                    tr("Proyecto", "Project"),
                    "Tokens",
                    tr("Coste", "Cost"),
                ])
                .style(head),
            )
            .block(block_c(
                &crate::tr!("Por proyecto · {period}", "By project · {period}"),
                color,
            )),
            right,
        );
    }

    fn on_key(&mut self, key: KeyEvent) -> Action {
        match key.code {
            KeyCode::Char('v') => {
                self.period = (self.period + 1) % PERIODS.len();
                Action::Refresh
            }
            _ => Action::Ignored,
        }
    }

    fn footer(&self) -> &'static str {
        tr(
            "v periodo de las tablas (hoy / 7 días / 30 días / todo)",
            "v table period (today / 7 days / 30 days / all)",
        )
    }
}

/// Branch names accepted for a new worktree. Stricter than git on purpose:
/// the name also becomes part of a directory name on Windows and Linux.
pub fn valid_branch(b: &str) -> bool {
    !b.is_empty()
        && !b.starts_with(['-', '.', '/'])
        && !b.ends_with(['/', '.'])
        && !b.contains("..")
        && !b.contains("//")
        && !b.ends_with(".lock")
        && b.chars()
            .all(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | '.' | '/'))
}

/// `git worktree list --porcelain`, with a timeout: a repo on a dead network
/// drive must not freeze the UI.
fn git_worktrees(cwd: &str) -> Option<String> {
    use std::io::Read;
    use std::process::{Command, Stdio};
    let mut child = Command::new("git")
        .args(["-C", cwd, "worktree", "list", "--porcelain"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    // Read in a thread: a big output would fill the pipe and block git.
    let mut stdout = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut out = String::new();
        stdout.read_to_string(&mut out).map(|_| out)
    });
    let start = Instant::now();
    // ponytail: polls every 10 ms, plenty for a 2 s timeout.
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if start.elapsed() < GIT_TIMEOUT => {
                std::thread::sleep(StdDuration::from_millis(10))
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
    reader.join().ok()?.ok()
}

const GIT_TIMEOUT: StdDuration = StdDuration::from_secs(2);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn branch_names() {
        for ok in ["feat/x", "fix-1", "v1.2", "ñandú"] {
            assert!(valid_branch(ok), "{ok}");
        }
        for bad in [
            "", "-b", "a b", "a..b", "a/", ".x", "x.lock", "a\\b", "c:x", "a//b",
        ] {
            assert!(!valid_branch(bad), "{bad}");
        }
    }

    #[test]
    fn formatting() {
        assert_eq!(short_model("claude-opus-5-5"), "opus-5-5");
        assert_eq!(money(3.456), "$3.46");
        assert_eq!(bar(34.0, 10), "███░░░░░░░");
        assert_eq!(bar(150.0, 4), "████");
    }
}
