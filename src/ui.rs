//! Shared UI pieces (taken from DevTerminal).

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, List, ListItem, ListState};

/// Grey for secondary text. Deliberately light: the terminal's DarkGray is
/// barely readable on a dark background.
pub const DIM: Color = Color::Rgb(0xa8, 0xb0, 0xc0);

/// Background of the selected row.
const SEL_BG: Color = Color::Rgb(0x2d, 0x37, 0x48);

pub fn block(title: &str) -> Block<'static> {
    block_c(title, DIM)
}

/// Bordered box with the title in the view's colour.
pub fn block_c(title: &str, color: Color) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(color))
        .title_style(Style::default().fg(color).add_modifier(Modifier::BOLD))
        .title(format!(" {title} "))
}

/// Rectangle centred at pct% of the area (floating windows).
pub fn centered(area: Rect, pct_x: u16, pct_y: u16) -> Rect {
    // In u32: with u16, width * 90 overflows on terminals wider than ~728 columns.
    let w = (area.width as u32 * pct_x as u32 / 100) as u16;
    let h = (area.height as u32 * pct_y as u32 / 100) as u16;
    Rect::new(
        area.x + (area.width - w) / 2,
        area.y + (area.height - h) / 2,
        w,
        h,
    )
}

/// The claudeterm logo (assets/logo.svg) as pixel art: each letter is a pixel
/// (d background, o orange, w cursor, space = transparent). Drawn with half
/// blocks, two pixel rows per line: 10×10 px = 10×5 characters.
const LOGO: [&str; 10] = [
    " dddddddd ",
    "dddddddddd",
    "dddddddddd",
    "ddoodddddd",
    "dddooddddd",
    "ddddoodddd",
    "dddooddddd",
    "ddoodwwwdd",
    "dddddddddd",
    " dddddddd ",
];

pub fn logo() -> Vec<Line<'static>> {
    let color = |c: char| match c {
        'd' => Some(Color::Rgb(0x1e, 0x24, 0x30)),
        'o' => Some(Color::Rgb(0xd9, 0x77, 0x57)),
        'w' => Some(Color::Rgb(0xe8, 0xe6, 0xe3)),
        _ => None,
    };
    LOGO.chunks(2)
        .map(|pair| {
            let spans = pair[0].chars().zip(pair[1].chars()).map(|(top, bottom)| {
                match (color(top), color(bottom)) {
                    (None, None) => Span::raw(" "),
                    (None, Some(b)) => Span::styled("▄", Style::default().fg(b)),
                    (Some(t), None) => Span::styled("▀", Style::default().fg(t)),
                    (Some(t), Some(b)) => Span::styled("▀", Style::default().fg(t).bg(b)),
                }
            });
            Line::from(spans.collect::<Vec<_>>())
        })
        .collect()
}

/// 1234 → "1.2k", 3400000 → "3.4M".
pub fn fmt_tokens(n: u64) -> String {
    match n {
        0..1_000 => n.to_string(),
        1_000..1_000_000 => format!("{:.1}k", n as f64 / 1e3),
        1_000_000..1_000_000_000 => format!("{:.1}M", n as f64 / 1e6),
        _ => format!("{:.2}G", n as f64 / 1e9),
    }
}

struct Row {
    line: Line<'static>,
    text: String,
    id: String,
}

/// List with selection (W/S or arrows) and filter (/ or F).
pub struct ListView {
    items: Vec<Row>,
    pub state: ListState,
    pub filter: String,
    pub filtering: bool,
    /// Where it was last drawn, to map a click to a row.
    area: Rect,
}

impl ListView {
    pub fn new() -> Self {
        Self {
            items: Vec::new(),
            state: ListState::default(),
            filter: String::new(),
            filtering: false,
            area: Rect::default(),
        }
    }

    /// Multi-coloured rows; `id` is what actions use.
    pub fn set_items(&mut self, items: Vec<(Line<'static>, String)>) {
        self.items = items
            .into_iter()
            .map(|(line, id)| {
                let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
                Row { line, text, id }
            })
            .collect();
        self.clamp();
    }

    fn visible(&self) -> Vec<&Row> {
        let f = self.filter.to_lowercase();
        self.items
            .iter()
            .filter(|r| f.is_empty() || r.text.to_lowercase().contains(&f))
            .collect()
    }

    pub fn selected_id(&self) -> Option<String> {
        let vis = self.visible();
        self.state
            .selected()
            .and_then(|i| vis.get(i))
            .map(|r| r.id.clone())
            .filter(|id| !id.is_empty())
    }

    fn clamp(&mut self) {
        let len = self.visible().len();
        if len == 0 {
            self.state.select(None);
        } else {
            let i = self.state.selected().unwrap_or(0).min(len - 1);
            self.state.select(Some(i));
        }
    }

    /// true if the key belonged to the list.
    pub fn key(&mut self, key: KeyEvent) -> bool {
        if self.filtering {
            match key.code {
                KeyCode::Esc => {
                    self.filtering = false;
                    self.filter.clear();
                }
                KeyCode::Enter => self.filtering = false,
                KeyCode::Backspace => {
                    self.filter.pop();
                }
                KeyCode::Char(c) => self.filter.push(c),
                _ => {}
            }
            self.clamp();
            return true;
        }
        let len = self.visible().len();
        let i = self.state.selected().unwrap_or(0);
        match key.code {
            KeyCode::Char('w' | 'W') | KeyCode::Up => self.state.select(Some(i.saturating_sub(1))),
            KeyCode::Char('s' | 'S') | KeyCode::Down if len > 0 => {
                self.state.select(Some((i + 1).min(len - 1)))
            }
            KeyCode::PageUp => self.state.select(Some(i.saturating_sub(15))),
            KeyCode::PageDown if len > 0 => self.state.select(Some((i + 15).min(len - 1))),
            KeyCode::Char('/' | 'f' | 'F') => self.filtering = true,
            _ => return false,
        }
        self.clamp();
        true
    }

    /// Click at (x, y): selects that row. true if there was a row there.
    pub fn click(&mut self, x: u16, y: u16) -> bool {
        let a = self.area;
        if x <= a.x || x + 1 >= a.x + a.width {
            return false;
        }
        match row_at(a, self.state.offset(), self.visible().len(), y) {
            Some(i) => {
                self.state.select(Some(i));
                true
            }
            None => false,
        }
    }

    pub fn draw(&mut self, f: &mut Frame, area: Rect, title: &str, color: Color) {
        self.area = area;
        let title = if self.filter.is_empty() && !self.filtering {
            title.to_string()
        } else {
            crate::tr!(
                "{title} — filtro: {}▏",
                "{title} — filter: {}▏",
                self.filter
            )
        };
        let items: Vec<ListItem> = self
            .visible()
            .into_iter()
            .map(|r| ListItem::new(r.line.clone()))
            .collect();
        let list = List::new(items)
            .block(block_c(&title, color))
            .highlight_style(Style::default().bg(SEL_BG).add_modifier(Modifier::BOLD))
            .highlight_symbol("▶ ");
        f.render_stateful_widget(list, area, &mut self.state);
    }
}

/// List row at screen line `y`: the list sits inside a border (1 line above
/// and below) and is scrolled by `offset` rows.
fn row_at(area: Rect, offset: usize, len: usize, y: u16) -> Option<usize> {
    if y <= area.y || y + 1 >= area.y + area.height {
        return None;
    }
    let i = offset + (y - area.y - 1) as usize;
    (i < len).then_some(i)
}

#[cfg(test)]
mod tests {
    use ratatui::layout::Rect;

    #[test]
    fn logo_size() {
        let l = super::logo();
        assert_eq!(l.len(), 5);
        assert!(l.iter().all(|line| line.width() == 10));
    }

    #[test]
    fn click_to_row() {
        let a = Rect::new(0, 3, 80, 10); // visible rows at y = 4..=11
        assert_eq!(super::row_at(a, 0, 50, 3), None); // top border
        assert_eq!(super::row_at(a, 0, 50, 4), Some(0));
        assert_eq!(super::row_at(a, 0, 50, 11), Some(7));
        assert_eq!(super::row_at(a, 0, 50, 12), None); // bottom border
        assert_eq!(super::row_at(a, 20, 50, 5), Some(21)); // scrolled
        assert_eq!(super::row_at(a, 0, 2, 6), None); // below the last row
    }

    #[test]
    fn tokens() {
        assert_eq!(super::fmt_tokens(999), "999");
        assert_eq!(super::fmt_tokens(1_250), "1.2k");
        assert_eq!(super::fmt_tokens(3_400_000), "3.4M");
    }
}
