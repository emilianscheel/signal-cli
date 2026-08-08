use std::io::{self, Stdout};

use anyhow::Result;
use chrono::{Local, TimeZone};
use crossterm::{
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, List, ListItem, ListState, Padding, Paragraph, Wrap},
    Terminal,
};

use crate::app::{App, Screen};

const ACCENT: Color = Color::Rgb(70, 130, 255);
const MUTED: Color = Color::DarkGray;

pub struct TerminalSession {
    terminal: Terminal<CrosstermBackend<Stdout>>,
}

impl TerminalSession {
    pub fn start() -> Result<Self> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen)?;
        let terminal = Terminal::new(CrosstermBackend::new(stdout))?;
        Ok(Self { terminal })
    }

    pub fn draw(&mut self, app: &App) -> Result<()> {
        self.terminal.draw(|frame| match app.screen {
            Screen::Conversations => draw_conversations(frame, app),
            Screen::Chat => draw_chat(frame, app),
        })?;
        Ok(())
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(self.terminal.backend_mut(), LeaveAlternateScreen);
        let _ = self.terminal.show_cursor();
    }
}

fn shell(frame: &mut ratatui::Frame<'_>) -> (Rect, Rect, Rect) {
    let [header, body, footer] = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(1),
            Constraint::Length(2),
        ])
        .areas(frame.area());
    (header, body, footer)
}

fn logo<'a>() -> Line<'a> {
    Line::from(vec![
        Span::styled("● ", Style::default().fg(ACCENT)),
        Span::styled(
            "signal",
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
    ])
}

fn status_spans(status: &str) -> Vec<Span<'_>> {
    if status == "Connected" {
        vec![
            Span::styled("● ", Style::default().fg(Color::Green)),
            Span::styled(status, Style::default().fg(MUTED)),
        ]
    } else {
        vec![Span::styled(status, Style::default().fg(MUTED))]
    }
}

fn draw_conversations(frame: &mut ratatui::Frame<'_>, app: &App) {
    let (header, body, footer) = shell(frame);
    frame.render_widget(
        Paragraph::new(logo()).block(Block::default().padding(Padding::horizontal(1))),
        header,
    );

    let content = if app.conversations.is_empty() {
        vec![ListItem::new(Text::from(vec![
            Line::from("No conversations synced yet"),
            Line::styled(
                "Keep this open while Signal syncs from your iPhone.",
                Style::default().fg(MUTED),
            ),
        ]))]
    } else {
        app.conversations
            .iter()
            .map(|chat| {
                ListItem::new(Text::from(vec![
                    Line::styled(
                        chat.title.clone(),
                        Style::default().add_modifier(Modifier::BOLD),
                    ),
                    Line::styled(chat.subtitle.clone(), Style::default().fg(MUTED)),
                ]))
            })
            .collect()
    };
    let list = List::new(content)
        .block(
            Block::default()
                .title(" Chats ")
                .borders(Borders::TOP)
                .padding(Padding::horizontal(2)),
        )
        .highlight_symbol("› ")
        .highlight_style(Style::default().fg(ACCENT).add_modifier(Modifier::BOLD));
    let mut state =
        ListState::default().with_selected((!app.conversations.is_empty()).then_some(app.selected));
    frame.render_stateful_widget(list, body, &mut state);
    let mut footer_spans = vec![
        Span::styled("↑↓", Style::default().fg(Color::White)),
        Span::styled(" move   ", Style::default().fg(MUTED)),
        Span::styled("enter", Style::default().fg(Color::White)),
        Span::styled(" open   ", Style::default().fg(MUTED)),
        Span::styled("r", Style::default().fg(Color::White)),
        Span::styled(" refresh   ", Style::default().fg(MUTED)),
    ];
    footer_spans.extend(status_spans(&app.status));
    frame.render_widget(
        Paragraph::new(Line::from(footer_spans))
            .block(Block::default().padding(Padding::horizontal(1))),
        footer,
    );
}

fn draw_chat(frame: &mut ratatui::Frame<'_>, app: &App) {
    let [header, messages, input, footer] = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(3),
            Constraint::Length(4),
            Constraint::Length(2),
        ])
        .areas(frame.area());
    let title = app.active().map(|c| c.title.as_str()).unwrap_or("Signal");
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            logo().spans[0].clone(),
            logo().spans[1].clone(),
            Span::raw("  "),
            Span::styled(title, Style::default().fg(MUTED)),
        ]))
        .block(Block::default().padding(Padding::horizontal(1))),
        header,
    );

    let width = messages.width.saturating_sub(6).max(20) as usize;
    let mut lines = Vec::new();
    for message in &app.messages {
        let time = Local
            .timestamp_millis_opt(message.timestamp as i64)
            .single()
            .map(|d| d.format("%H:%M").to_string())
            .unwrap_or_default();
        let author = if message.mine {
            "you".to_string()
        } else {
            message.sender.clone().unwrap_or_else(|| title.to_string())
        };
        lines.push(Line::from(vec![
            Span::styled(
                author,
                Style::default()
                    .fg(if message.mine { ACCENT } else { Color::Green })
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!("  {time}"), Style::default().fg(MUTED)),
        ]));
        for wrapped in wrap_text(&message.body, width) {
            lines.push(Line::raw(wrapped));
        }
        lines.push(Line::raw(""));
    }
    if lines.is_empty() {
        lines.push(Line::styled(
            "No messages yet. Say hello.",
            Style::default().fg(MUTED),
        ));
    }
    let total_lines = lines.len() as u16;
    let available = messages.height.saturating_sub(2);
    let bottom = total_lines.saturating_sub(available);
    let scroll = bottom.saturating_sub(app.scroll.min(bottom));
    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .scroll((scroll, 0))
            .block(
                Block::default()
                    .borders(Borders::TOP)
                    .padding(Padding::horizontal(2)),
            ),
        messages,
    );

    frame.render_widget(
        Paragraph::new(app.input.as_str())
            .wrap(Wrap { trim: false })
            .block(
                Block::default()
                    .title(" Message ")
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(ACCENT))
                    .padding(Padding::horizontal(1)),
            ),
        input,
    );
    let cursor_prefix = &app.input[..app.cursor];
    let cursor_x = input.x + 2 + cursor_prefix.chars().count() as u16;
    frame.set_cursor_position((cursor_x.min(input.right().saturating_sub(2)), input.y + 1));

    let mut footer_spans = vec![
        Span::styled("enter", Style::default().fg(Color::White)),
        Span::styled(" send   ", Style::default().fg(MUTED)),
        Span::styled("esc", Style::default().fg(Color::White)),
        Span::styled(" chats   ", Style::default().fg(MUTED)),
        Span::styled("pgup/dn", Style::default().fg(Color::White)),
        Span::styled(" scroll   ", Style::default().fg(MUTED)),
        Span::styled("ctrl-c", Style::default().fg(Color::White)),
        Span::styled(" quit   ", Style::default().fg(MUTED)),
    ];
    footer_spans.extend(status_spans(&app.status));
    frame.render_widget(
        Paragraph::new(Line::from(footer_spans))
            .block(Block::default().padding(Padding::horizontal(1))),
        footer,
    );
}

fn wrap_text(input: &str, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    for source_line in input.lines() {
        let mut current = String::new();
        for word in source_line.split_whitespace() {
            if !current.is_empty() && current.chars().count() + 1 + word.chars().count() > width {
                lines.push(std::mem::take(&mut current));
            }
            if !current.is_empty() {
                current.push(' ');
            }
            current.push_str(word);
        }
        lines.push(current);
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::wrap_text;

    #[test]
    fn wraps_at_word_boundaries() {
        assert_eq!(wrap_text("one two three", 7), vec!["one two", "three"]);
    }

    #[test]
    fn preserves_explicit_newlines() {
        assert_eq!(wrap_text("one\ntwo", 20), vec!["one", "two"]);
    }
}
