//! claudeterm (`cct`): a terminal dashboard for Claude Code.
//! The main loop and the floating windows come from DevTerminal.

mod cli;
mod data;
mod i18n;
mod platform;
mod ui;
mod views;

use std::collections::HashMap;
use std::io::IsTerminal;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use ratatui::crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
    MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::crossterm::execute;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph, Tabs, Wrap};
use ratatui::{DefaultTerminal, Frame};

use i18n::tr;
use ui::{block, block_c, centered};
use views::{Action, View};

/// How often Claude Code's files are reread (only what's new).
const AUTO_REFRESH: Duration = Duration::from_secs(2);
/// How often to check whether a session is now waiting for you.
const IDLE_CHECK: Duration = Duration::from_secs(1);
/// Two clicks on the same row within this = double click (Enter).
const DOUBLE_CLICK: Duration = Duration::from_millis(400);

/// Clickable area recorded while drawing.
enum Hit {
    Tab(usize),
    /// Does the same as that key.
    Key(KeyCode),
}

enum Overlay {
    Confirm {
        msg: String,
        cmd: Vec<String>,
        show: bool,
    },
    Prompt {
        label: String,
        template: Vec<String>,
        input: String,
    },
    /// `scroll` = u16::MAX means "at the end" (clamped while drawing).
    Pane {
        title: String,
        text: String,
        scroll: u16,
    },
}

struct App {
    views: Vec<Box<dyn View>>,
    active: usize,
    overlay: Option<Overlay>,
    store: data::Store,
    last_refresh: Instant,
    last_idle_check: Instant,
    /// Last seen status of each pid (busy/idle), to notify on change.
    statuses: HashMap<u32, String>,
    pending_refresh: bool,
    notice: Option<String>,
    /// Tabs and buttons from the last draw.
    hits: Vec<(Rect, Hit)>,
    /// Open floating window: a click outside closes it.
    overlay_area: Rect,
    /// Last click on a row (when, which line), for double clicks.
    last_click: Option<(Instant, u16)>,
}

fn main() -> Result<()> {
    match std::env::args().nth(1).as_deref() {
        Some("--statusline") => return cli::statusline(),
        Some("--waybar") => return cli::waybar(),
        Some("--mini") => return cli::mini(),
        Some("-V" | "--version") => {
            println!("cct {}", env!("CARGO_PKG_VERSION"));
            return Ok(());
        }
        Some("-h" | "--help") => {
            println!(
                "{}",
                tr(
                    "cct: panel de control de Claude Code en la terminal\n\n\
                     Uso: cct [opción]\n\n  \
                     (nada)         abre el panel\n  \
                     --mini         ventana pequeña con el % de los límites\n  \
                     --statusline   statusline de Claude Code (lee JSON por stdin)\n  \
                     --waybar       módulo JSON para Waybar\n  \
                     -V, --version  versión\n  \
                     -h, --help     esta ayuda",
                    "cct: terminal dashboard for Claude Code\n\n\
                     Usage: cct [option]\n\n  \
                     (none)         open the dashboard\n  \
                     --mini         small window with the limits' %\n  \
                     --statusline   Claude Code statusline (reads JSON on stdin)\n  \
                     --waybar       JSON module for Waybar\n  \
                     -V, --version  version\n  \
                     -h, --help     this help",
                )
            );
            return Ok(());
        }
        Some(other) => bail!(tr!(
            "opción desconocida: {other} (prueba --help)",
            "unknown option: {other} (try --help)"
        )),
        None => {}
    }
    // ratatui doesn't enable the mouse, nor disable it on a panic: without
    // this hook the terminal would keep spewing escape codes on mouse moves.
    // Installed before anything can panic.
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = execute!(std::io::stdout(), DisableMouseCapture);
        hook(info);
    }));
    let mut terminal = init_terminal()?;
    let mut app = App::new();
    let _ = execute!(std::io::stdout(), EnableMouseCapture);
    let res = app.run(&mut terminal);
    let _ = execute!(std::io::stdout(), DisableMouseCapture);
    ratatui::restore();
    res
}

impl App {
    fn new() -> Self {
        Self {
            views: views::all(),
            active: 0,
            overlay: None,
            store: data::Store::new(),
            last_refresh: Instant::now(),
            last_idle_check: Instant::now(),
            statuses: HashMap::new(),
            pending_refresh: true,
            notice: None,
            hits: Vec::new(),
            overlay_area: Rect::default(),
            last_click: None,
        }
    }

    fn refresh_active(&mut self) {
        self.store.refresh();
        self.views[self.active].refresh(&self.store);
        self.last_refresh = Instant::now();
    }

    /// busy → idle = Claude finished and is waiting for you: desktop notification.
    fn check_idle(&mut self) {
        self.last_idle_check = Instant::now();
        let live = data::live_sessions(&self.store.root);
        let mut next = HashMap::new();
        for l in &live {
            if self.statuses.get(&l.pid).is_some_and(|s| s == "busy") && l.status == "idle" {
                let body = format!(
                    "{} · {}",
                    data::title(&self.store.windows, l),
                    views::short_path(&l.cwd)
                );
                platform::notify_idle(body, self.store.windows.get(&l.pid).map(|w| w.pid));
            }
            next.insert(l.pid, l.status.clone());
        }
        self.statuses = next;
    }

    fn run(&mut self, terminal: &mut DefaultTerminal) -> Result<()> {
        self.check_idle();
        loop {
            terminal.draw(|f| self.draw(f))?;
            if self.pending_refresh {
                self.pending_refresh = false;
                self.refresh_active();
                continue;
            }
            if !event::poll(Duration::from_millis(250))? {
                if self.last_idle_check.elapsed() >= IDLE_CHECK {
                    self.check_idle();
                }
                if self.overlay.is_none() && self.last_refresh.elapsed() >= AUTO_REFRESH {
                    self.refresh_active();
                }
                continue;
            }
            let quit = match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => self.handle_key(key),
                Event::Mouse(m) => self.mouse(m),
                _ => false,
            };
            if quit {
                return Ok(());
            }
        }
    }

    /// true = quit.
    fn handle_key(&mut self, key: KeyEvent) -> bool {
        self.notice = None;
        if self.overlay.is_some() {
            self.overlay_key(key);
            return false;
        }
        match self.views[self.active].on_key(key) {
            Action::Ignored => return self.global_key(key),
            Action::Handled => {}
            Action::Refresh => self.pending_refresh = true,
            Action::Run {
                cmd,
                confirm: Some(msg),
                show,
            } => self.overlay = Some(Overlay::Confirm { msg, cmd, show }),
            Action::Run {
                cmd,
                confirm: None,
                show,
            } => self.exec(&cmd, show),
            Action::Spawn(cmd) => {
                self.notice = Some(match spawn(&cmd) {
                    Ok(()) => tr("Abriendo ventana nueva…", "Opening a new window…").into(),
                    Err(e) => format!("Error: {e}"),
                })
            }
            Action::Prompt { label, template } => {
                self.overlay = Some(Overlay::Prompt {
                    label,
                    template,
                    input: String::new(),
                })
            }
            Action::Show { title, text } => {
                self.overlay = Some(Overlay::Pane {
                    title,
                    text,
                    scroll: u16::MAX,
                })
            }
        }
        false
    }

    /// The mouse does what the keys do: each gesture is mapped to a key.
    /// true = quit.
    fn mouse(&mut self, m: MouseEvent) -> bool {
        let key = KeyEvent::from;
        let (x, y) = (m.column, m.row);
        let inside = |r: &Rect| x >= r.x && x < r.x + r.width && y >= r.y && y < r.y + r.height;
        match m.kind {
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                let code = if m.kind == MouseEventKind::ScrollUp {
                    KeyCode::Up
                } else {
                    KeyCode::Down
                };
                // In the text window, 3 lines per wheel step.
                let n = if matches!(self.overlay, Some(Overlay::Pane { .. })) {
                    3
                } else {
                    1
                };
                (0..n).any(|_| self.handle_key(key(code)))
            }
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some(i) = self.hits.iter().position(|(r, _)| inside(r)) {
                    return match self.hits[i].1 {
                        Hit::Tab(t) => {
                            if t != self.active {
                                self.active = t;
                                self.pending_refresh = true;
                            }
                            false
                        }
                        Hit::Key(code) => self.handle_key(key(code)),
                    };
                }
                if self.overlay.is_some() {
                    if !inside(&self.overlay_area) {
                        self.handle_key(key(KeyCode::Esc));
                    }
                    return false;
                }
                let Some(list) = self.views[self.active].list() else {
                    return false;
                };
                if !list.click(x, y) {
                    return false;
                }
                let double = self
                    .last_click
                    .is_some_and(|(t, row)| row == y && t.elapsed() < DOUBLE_CLICK);
                self.last_click = (!double).then(|| (Instant::now(), y));
                double && self.handle_key(key(KeyCode::Enter))
            }
            _ => false,
        }
    }

    /// true = quit.
    fn global_key(&mut self, key: KeyEvent) -> bool {
        let n = self.views.len();
        let prev = self.active;
        match key.code {
            KeyCode::Char('q' | 'Q') => return true,
            KeyCode::Tab | KeyCode::Char('d' | 'D') => self.active = (self.active + 1) % n,
            KeyCode::BackTab | KeyCode::Char('a' | 'A') => self.active = (self.active + n - 1) % n,
            KeyCode::Char('r' | 'R') => self.pending_refresh = true,
            KeyCode::Char('y' | 'Y') => {
                if let Some(text) = self.views[self.active].clip() {
                    self.notice = Some(match platform::copy(&text) {
                        Ok(()) => tr!("Copiado: {text}", "Copied: {text}"),
                        Err(e) => tr!("Portapapeles: {e}", "Clipboard: {e}"),
                    });
                }
            }
            KeyCode::Char(c @ '1'..='9') => {
                let i = c as usize - '1' as usize;
                if i < n {
                    self.active = i;
                }
            }
            _ => {}
        }
        if self.active != prev {
            self.pending_refresh = true;
        }
        false
    }

    fn overlay_key(&mut self, key: KeyEvent) {
        let Some(ov) = self.overlay.take() else {
            return;
        };
        match ov {
            Overlay::Pane {
                title,
                text,
                scroll,
            } => {
                let s = match key.code {
                    KeyCode::Esc | KeyCode::Char('q') | KeyCode::Enter => return,
                    KeyCode::Char('w') | KeyCode::Up => scroll.saturating_sub(1),
                    KeyCode::Char('s') | KeyCode::Down => scroll.saturating_add(1),
                    KeyCode::PageUp => scroll.saturating_sub(20),
                    KeyCode::PageDown => scroll.saturating_add(20),
                    KeyCode::Home | KeyCode::Char('g') => 0,
                    KeyCode::End | KeyCode::Char('G') => u16::MAX,
                    _ => scroll,
                };
                self.overlay = Some(Overlay::Pane {
                    title,
                    text,
                    scroll: s,
                });
            }
            Overlay::Confirm { msg, cmd, show } => match key.code {
                KeyCode::Enter | KeyCode::Char('y' | 'Y' | 's' | 'S') => self.exec(&cmd, show),
                KeyCode::Esc | KeyCode::Char('n' | 'N') => {}
                _ => self.overlay = Some(Overlay::Confirm { msg, cmd, show }),
            },
            Overlay::Prompt {
                label,
                template,
                mut input,
            } => match key.code {
                KeyCode::Esc => {}
                KeyCode::Enter if !views::valid_branch(&input) => {
                    self.notice = Some(s(tr(
                        "Nombre de rama no válido: letras, números y . _ - / (sin espacios ni ..)",
                        "Invalid branch name: letters, digits and . _ - / (no spaces or ..)",
                    )));
                    self.overlay = Some(Overlay::Prompt {
                        label,
                        template,
                        input,
                    });
                }
                KeyCode::Enter => {
                    // "{}" = what was typed; "{-}" = the same with `/` → `-`
                    // (for paths: feat/x must not create a subdirectory).
                    let flat = input.replace('/', "-");
                    let cmd: Vec<String> = template
                        .iter()
                        .map(|a| a.replace("{-}", &flat).replace("{}", &input))
                        .collect();
                    self.exec(&cmd, true);
                }
                KeyCode::Backspace => {
                    input.pop();
                    self.overlay = Some(Overlay::Prompt {
                        label,
                        template,
                        input,
                    });
                }
                KeyCode::Char(c) => {
                    input.push(c);
                    self.overlay = Some(Overlay::Prompt {
                        label,
                        template,
                        input,
                    });
                }
                _ => {
                    self.overlay = Some(Overlay::Prompt {
                        label,
                        template,
                        input,
                    })
                }
            },
        }
    }

    /// Runs capturing the output; shows it if `show` or on failure.
    fn exec(&mut self, cmd: &[String], show: bool) {
        match run_cmd(cmd) {
            Ok(out) if show => {
                let text = if out.trim().is_empty() {
                    "OK ✓".into()
                } else {
                    out
                };
                self.overlay = Some(Overlay::Pane {
                    title: cmd.join(" "),
                    text,
                    scroll: 0,
                });
            }
            Ok(_) => {}
            Err(e) => {
                self.overlay = Some(Overlay::Pane {
                    title: format!("Error · {}", cmd.join(" ")),
                    text: e.to_string(),
                    scroll: 0,
                })
            }
        }
        self.refresh_active();
    }

    fn draw(&mut self, f: &mut Frame) {
        let [header, body, footer] = Layout::vertical([
            Constraint::Length(3),
            Constraint::Min(0),
            Constraint::Length(1),
        ])
        .areas(f.area());

        let accent = self.views[self.active].accent();
        let titles: Vec<Line> = self
            .views
            .iter()
            .enumerate()
            .map(|(i, v)| {
                let mut style = Style::default().fg(v.accent());
                if i == self.active {
                    style = style.add_modifier(Modifier::BOLD);
                }
                Line::from(Span::styled(format!("{} {}", i + 1, v.title()), style))
            })
            .collect();
        let busy = self.statuses.values().filter(|s| *s == "busy").count();
        let idle = self.statuses.len() - busy;
        let spin = if self.pending_refresh { " ⟳" } else { "" };
        let brand = tr!(
            "claudeterm{spin} · ● {busy} trabajando · ◌ {idle} esperando",
            "claudeterm{spin} · ● {busy} working · ◌ {idle} waiting"
        );
        self.hits = tab_rects(header, &titles)
            .into_iter()
            .enumerate()
            .map(|(i, r)| (r, Hit::Tab(i)))
            .collect();
        f.render_widget(
            Tabs::new(titles)
                .select(self.active)
                .block(block_c(&brand, accent)),
            header,
        );

        self.views[self.active].draw(f, body);

        let text = match &self.notice {
            Some(n) => format!(" {n}"),
            None => tr!(
                " {} · y copiar · Tab/A/D pestaña · 1-4 saltar · R refrescar · Q salir",
                " {} · y copy · Tab/A/D tab · 1-4 jump · R refresh · Q quit",
                self.views[self.active].footer()
            ),
        };
        f.render_widget(
            Paragraph::new(text).style(Style::default().fg(accent)),
            footer,
        );

        self.overlay_area = match self.overlay {
            Some(Overlay::Pane { .. }) => centered(f.area(), 90, 85),
            Some(_) => centered(f.area(), 60, 25),
            None => Rect::default(),
        };
        match &mut self.overlay {
            Some(Overlay::Pane {
                title,
                text,
                scroll,
            }) => {
                let area = centered(f.area(), 90, 85);
                let p = Paragraph::new(text.as_str())
                    .block(block(&tr!(
                        "{title} — w/s PgUp/PgDn g/G · Esc cierra",
                        "{title} — w/s PgUp/PgDn g/G · Esc closes"
                    )))
                    .wrap(Wrap { trim: false });
                let max = p
                    .line_count(area.width.saturating_sub(2))
                    .saturating_sub(area.height.saturating_sub(2) as usize);
                *scroll = (*scroll).min(max.min(u16::MAX as usize) as u16);
                f.render_widget(Clear, area);
                f.render_widget(p.scroll((*scroll, 0)), area);
            }
            Some(Overlay::Confirm { msg, .. }) => {
                let area = centered(f.area(), 60, 25);
                // The last line is two clickable buttons.
                let (yes, no) = (
                    tr("[ Enter/s confirmar ]", "[ Enter/y confirm ]"),
                    tr("[ Esc/n cancelar ]", "[ Esc/n cancel ]"),
                );
                let p = Paragraph::new(format!("{msg}\n\n{yes}  {no}"))
                    .block(block_c(tr("Confirmar", "Confirm"), Color::Yellow))
                    .wrap(Wrap { trim: false });
                let y =
                    area.y + p.line_count(area.width.saturating_sub(2)).saturating_sub(2) as u16;
                if y + 1 < area.y + area.height {
                    let (w_yes, w_no) = (yes.chars().count() as u16, no.chars().count() as u16);
                    self.hits
                        .push((Rect::new(area.x + 1, y, w_yes, 1), Hit::Key(KeyCode::Enter)));
                    self.hits.push((
                        Rect::new(area.x + 1 + w_yes + 2, y, w_no, 1),
                        Hit::Key(KeyCode::Esc),
                    ));
                }
                f.render_widget(Clear, area);
                f.render_widget(p, area);
            }
            Some(Overlay::Prompt { label, input, .. }) => {
                let area = centered(f.area(), 60, 25);
                f.render_widget(Clear, area);
                f.render_widget(
                    Paragraph::new(tr!(
                        "{label}\n\n> {input}▏\n\nEnter aceptar · Esc cancelar",
                        "{label}\n\n> {input}▏\n\nEnter accept · Esc cancel"
                    ))
                    .block(block(tr("Escribe", "Type")))
                    .wrap(Wrap { trim: false }),
                    area,
                );
            }
            None => {}
        }
    }
}

/// Where each tab sits inside its box: Tabs draws " title " separated
/// by "│".
fn tab_rects(area: Rect, titles: &[Line]) -> Vec<Rect> {
    let mut x = area.x + 1;
    titles
        .iter()
        .map(|t| {
            let w = t.width() as u16 + 2;
            x += w + 1;
            Rect::new(x - w - 1, area.y + 1, w, 1)
        })
        .collect()
}

// ───────────── external commands (like DevTerminal's runner.rs) ─────────────

/// Runs and returns the output; error if the exit code isn't 0.
fn run_cmd(cmd: &[String]) -> Result<String> {
    let Some((prog, args)) = cmd.split_first() else {
        bail!(tr("comando vacío", "empty command"))
    };
    let out = Command::new(prog).args(args).output()?;
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    if out.status.success() {
        Ok(stdout)
    } else {
        bail!(
            "{}\n{}",
            stdout.trim_end(),
            String::from_utf8_lossy(&out.stderr).trim_end()
        )
    }
}

/// Launches without inheriting the terminal (otherwise kitty would mess up
/// the TUI). With `setsid -f` in front, this returns at once.
fn spawn(cmd: &[String]) -> Result<()> {
    let Some((prog, args)) = cmd.split_first() else {
        bail!(tr("comando vacío", "empty command"))
    };
    Command::new(prog)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;
    Ok(())
}

fn s(v: &str) -> String {
    v.to_string()
}

/// Enters the TUI, or explains why it can't (e.g. `cct | less`) instead of
/// panicking. ratatui's own panic hook restores the terminal.
pub fn init_terminal() -> Result<DefaultTerminal> {
    if !std::io::stdout().is_terminal() {
        bail!(tr(
            "cct necesita una terminal interactiva (la salida no es una TTY)",
            "cct needs an interactive terminal (stdout is not a TTY)"
        ));
    }
    Ok(ratatui::try_init()?)
}

#[cfg(test)]
mod tests {
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;
    use ratatui::text::Line;
    use ratatui::widgets::{Block, Borders, Tabs, Widget};

    /// The clickable areas land exactly on what Tabs draws.
    #[test]
    fn tab_hits_match_render() {
        let titles: Vec<Line> = ["1 Sessions", "2 Agents", "3 Worktrees", "4 Usage"]
            .map(Line::from)
            .to_vec();
        let area = Rect::new(0, 0, 80, 3);
        let mut buf = Buffer::empty(area);
        Tabs::new(titles.clone())
            .block(Block::default().borders(Borders::ALL))
            .render(area, &mut buf);
        for (r, t) in super::tab_rects(area, &titles).iter().zip(&titles) {
            let shown: String = (r.x..r.x + r.width)
                .map(|x| buf[(x, r.y)].symbol().to_string())
                .collect();
            assert_eq!(shown, format!(" {} ", t));
        }
    }
}
