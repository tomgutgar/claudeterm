//! Modes other than the main TUI:
//! - `cct --mini`: small window with the logo and the limits' %.
//! - `cct --statusline`: Claude Code's statusline. It is the only official way
//!   to know the % used of the 5-hour and weekly limits.
//! - `cct --waybar`: bar module (JSON with text, tooltip and class).

use std::fs;
use std::io::Read;
use std::time::Duration;

use anyhow::Result;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use serde_json::{Value, json};

use crate::i18n::tr;
use crate::views::{limit_bar, short_path};
use crate::{data, platform, ui};

/// Compact mode: logo + 5 h and weekly with their reset time. Meant for a
/// small window that stays on top of the rest (see README). Rereads the limits
/// every 2 s (the file is tiny). `q` or `Esc` to quit.
pub fn mini() -> Result<()> {
    let mut terminal = crate::init_terminal()?;
    let res = (|| -> Result<()> {
        loop {
            // On every pass: in Windows Terminal the window takes a moment to
            // be ready and the first call may have no effect.
            platform::stay_on_top();
            let limits = data::read_limits().unwrap_or_default();
            terminal.draw(|f| {
                let a = f.area();
                // No room for the logo (10 columns + margin): numbers only.
                let x = if a.width >= 44 {
                    f.render_widget(
                        Paragraph::new(ui::logo()),
                        Rect {
                            width: 10,
                            height: 5.min(a.height),
                            ..a
                        },
                    );
                    12
                } else {
                    0
                };
                let mut lines = Vec::new();
                for (name, lim) in [("5 h ", &limits.five_hour), ("7 d ", &limits.seven_day)] {
                    let bold = Style::default().add_modifier(Modifier::BOLD);
                    match lim {
                        Some(l) => {
                            lines.push(Line::from(vec![
                                Span::styled(name, bold),
                                limit_bar(l.pct(), 10),
                            ]));
                            lines.push(Line::styled(
                                format!("    {}", l.reset_text()),
                                Style::default().fg(ui::DIM),
                            ));
                        }
                        None => lines.push(Line::from(vec![
                            Span::styled(name, bold),
                            Span::styled(tr("sin datos", "no data"), Style::default().fg(ui::DIM)),
                        ])),
                    }
                    lines.push(Line::raw(""));
                }
                f.render_widget(
                    Paragraph::new(lines),
                    Rect {
                        x: a.x + x,
                        width: a.width.saturating_sub(x),
                        ..a
                    },
                );
            })?;
            if event::poll(Duration::from_secs(2))?
                && let Event::Key(k) = event::read()?
                && k.kind == KeyEventKind::Press
                && matches!(k.code, KeyCode::Char('q') | KeyCode::Esc)
            {
                return Ok(());
            }
        }
    })();
    ratatui::restore();
    res
}

/// Claude Code runs this after every reply, passing a JSON on stdin. Its
/// `rate_limits` are saved and the line shown under the prompt is printed.
pub fn statusline() -> Result<()> {
    let mut input = Vec::new();
    std::io::stdin().read_to_end(&mut input)?;
    // Lossy: a stray invalid byte must not leave the status line empty.
    let v: Value = serde_json::from_str(&String::from_utf8_lossy(&input)).unwrap_or_default();
    let limits = &v["rate_limits"];
    if limits.is_object() {
        let path = data::limits_path();
        if let Some(dir) = path.parent() {
            let _ = fs::create_dir_all(dir);
        }
        // Several sessions may write at once: each to its own temp file, then rename.
        let tmp = path.with_extension(format!("{}.tmp", std::process::id()));
        if fs::write(&tmp, limits.to_string()).is_ok() && fs::rename(&tmp, &path).is_err() {
            // On Windows the rename fails if another process has the file open.
            let _ = fs::remove_file(&tmp);
        }
    }
    let mut parts: Vec<String> = Vec::new();
    if let Some(m) = v["model"]["display_name"].as_str() {
        parts.push(m.to_string());
    }
    if let Some(c) = v["context_window"]["used_percentage"].as_f64() {
        parts.push(format!("ctx {c:.0}%"));
    }
    for (key, name) in [("five_hour", "5h"), ("seven_day", "7d")] {
        if let Some(p) = limits[key]["used_percentage"].as_f64() {
            parts.push(format!("{name} {p:.0}%"));
        }
    }
    println!("{}", parts.join(" · "));
    Ok(())
}

/// One JSON line for a Waybar `custom/claude` module (`return-type: json`).
pub fn waybar() -> Result<()> {
    let live = data::live_sessions(&data::claude_root());
    let wins = data::windows(&live);
    let busy = live.iter().filter(|l| l.status == "busy").count();
    let idle = live.len() - busy;
    let limits = data::read_limits().unwrap_or_default();

    let mut text = if live.is_empty() {
        "✳".to_string()
    } else {
        format!("✳ {busy}● {idle}◌")
    };
    let mut tooltip: Vec<String> = live
        .iter()
        .map(|l| {
            let st = if l.status == "busy" {
                tr("● trabajando", "● working")
            } else {
                tr("◌ esperando", "◌ waiting")
            };
            format!("{st}  {} — {}", data::title(&wins, l), short_path(&l.cwd))
        })
        .collect();
    if live.is_empty() {
        tooltip.push(tr("Ningún Claude abierto", "No Claude open").into());
    }
    tooltip.push(String::new());
    for (lim, name) in [
        (&limits.five_hour, tr("Límite 5 h", "5 h limit")),
        (&limits.seven_day, tr("Semanal", "Weekly")),
    ] {
        match lim {
            Some(l) => tooltip.push(format!("{name}: {:.0}% · {}", l.pct(), l.reset_text())),
            None => tooltip.push(crate::tr!(
                "{name}: sin datos (hace falta la statusline de cct)",
                "{name}: no data (needs the cct statusline)"
            )),
        }
    }
    tooltip.push(tr("Clic: abrir claudeterm", "Click: open claudeterm").into());

    let five = limits.five_hour.as_ref().map(|l| l.pct());
    if let Some(p) = five {
        text.push_str(&format!(" · 5h {p:.0}%"));
    }
    let class = match (five, busy, idle) {
        (Some(p), ..) if p >= 80.0 => "warn",
        (_, _, i) if i > 0 => "idle",
        (_, b, _) if b > 0 => "busy",
        _ => "none",
    };
    println!(
        "{}",
        json!({ "text": text, "tooltip": tooltip.join("\n"), "class": class })
    );
    Ok(())
}
