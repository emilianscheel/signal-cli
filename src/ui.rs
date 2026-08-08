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
    widgets::{Block, Borders, List, ListItem, Padding, Paragraph, Wrap},
    Terminal,
};

use crate::app::{App, LayoutMode, Screen};

const ACCENT: Color = Color::Rgb(70, 130, 255);
pub const SIDEBAR_WIDTH: u16 = 32;

fn primary() -> Style {
    Style::default().fg(Color::Reset)
}

fn muted() -> Style {
    primary().add_modifier(Modifier::DIM)
}

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
        self.terminal.draw(|frame| draw_app(frame, app))?;
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

fn draw_app(frame: &mut ratatui::Frame<'_>, app: &App) {
    if app.screen == Screen::DisconnectConfirm {
        draw_disconnect_confirm(frame);
        return;
    }
    match app.layout_mode() {
        LayoutMode::Narrow => match app.screen {
            Screen::Conversations => draw_narrow_conversations(frame, app),
            Screen::Chat => draw_narrow_chat(frame, app),
            Screen::DisconnectConfirm => unreachable!(),
        },
        LayoutMode::Wide => draw_wide(frame, app),
    }
}

fn shell(area: Rect) -> (Rect, Rect, Rect) {
    let [header, body, footer] = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(1),
            Constraint::Length(2),
        ])
        .areas(area);
    (header, body, footer)
}

fn wide_columns(area: Rect) -> (Rect, Rect) {
    let [sidebar, chat] = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(SIDEBAR_WIDTH), Constraint::Min(1)])
        .areas(area);
    (sidebar, chat)
}

fn logo<'a>() -> Line<'a> {
    Line::from(vec![
        Span::styled("● ", Style::default().fg(ACCENT)),
        Span::styled("signal", primary().add_modifier(Modifier::BOLD)),
    ])
}

fn status_spans(status: &str) -> Vec<Span<'_>> {
    let status = if status.eq_ignore_ascii_case("Connected to Signal CLI") {
        "Connected"
    } else {
        status
    };
    if status == "Connected" {
        vec![
            Span::styled("● ", Style::default().fg(Color::Green)),
            Span::styled(status, muted()),
        ]
    } else {
        vec![Span::styled(status, muted())]
    }
}

fn render_logo(frame: &mut ratatui::Frame<'_>, area: Rect) {
    frame.render_widget(
        Paragraph::new(logo()).block(Block::default().padding(Padding::horizontal(1))),
        area,
    );
}

fn render_chat_header(frame: &mut ratatui::Frame<'_>, app: &App, area: Rect, with_logo: bool) {
    let title = app
        .opened_conversation()
        .map(|conversation| conversation.title.as_str())
        .unwrap_or("Signal");
    let mut spans = if with_logo { logo().spans } else { Vec::new() };
    if with_logo {
        spans.push(Span::raw("  "));
    }
    spans.push(Span::styled(title, muted()));
    frame.render_widget(
        Paragraph::new(Line::from(spans)).block(Block::default().padding(Padding::horizontal(1))),
        area,
    );
}

fn conversation_list(app: &App, wide: bool, focused: bool) -> List<'static> {
    let content = if app.conversations.is_empty() {
        vec![ListItem::new(Text::from(vec![
            Line::from("No conversations synced yet"),
            Line::styled(
                "Keep this open while Signal syncs from your iPhone.",
                muted(),
            ),
        ]))]
    } else {
        app.conversations
            .iter()
            .enumerate()
            .map(|(index, chat)| {
                let selected = index == app.selected;
                ListItem::new(Text::from(vec![
                    Line::from(vec![
                        Span::styled(
                            if selected { "› " } else { "  " },
                            if selected {
                                Style::default().fg(ACCENT)
                            } else {
                                primary()
                            },
                        ),
                        Span::styled(
                            chat.title.clone(),
                            if selected {
                                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
                            } else {
                                primary().add_modifier(Modifier::BOLD)
                            },
                        ),
                    ]),
                    Line::from(vec![
                        Span::raw("  "),
                        Span::styled(chat.subtitle.clone(), muted()),
                    ]),
                    Line::raw(""),
                ]))
            })
            .collect()
    };
    let borders = if wide {
        Borders::TOP | Borders::RIGHT
    } else {
        Borders::TOP
    };
    let border_style = if focused {
        Style::default().fg(ACCENT)
    } else {
        muted()
    };
    List::new(content).block(
        Block::default()
            .title(" Chats ")
            .borders(borders)
            .border_style(border_style)
            .padding(Padding::horizontal(if wide { 1 } else { 2 })),
    )
}

fn render_messages(frame: &mut ratatui::Frame<'_>, app: &App, area: Rect) {
    if app.opened_conversation().is_none() {
        frame.render_widget(
            Paragraph::new(Line::styled(
                "Your conversations will appear here after Signal syncs.",
                muted(),
            ))
            .wrap(Wrap { trim: true })
            .block(
                Block::default()
                    .borders(Borders::TOP)
                    .padding(Padding::horizontal(2)),
            ),
            area,
        );
        return;
    }

    let title = app
        .opened_conversation()
        .map(|conversation| conversation.title.as_str())
        .unwrap_or("Signal");
    let width = area.width.saturating_sub(6).max(20) as usize;
    let mut lines = Vec::new();
    for message in &app.messages {
        let time = Local
            .timestamp_millis_opt(message.timestamp as i64)
            .single()
            .map(|date| date.format("%H:%M").to_string())
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
            Span::styled(format!("  {time}"), muted()),
        ]));
        for wrapped in wrap_text(&message.body, width) {
            lines.push(Line::raw(wrapped));
        }
        lines.push(Line::raw(""));
    }
    if lines.is_empty() {
        lines.push(Line::styled("No messages yet. Say hello.", muted()));
    }
    let total_lines = lines.len() as u16;
    let available = area.height.saturating_sub(2);
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
        area,
    );
}

fn render_composer(frame: &mut ratatui::Frame<'_>, app: &App, area: Rect, focused: bool) {
    let border_style = if focused {
        Style::default().fg(ACCENT)
    } else {
        muted()
    };
    frame.render_widget(
        Paragraph::new(app.input.as_str())
            .wrap(Wrap { trim: false })
            .block(
                Block::default()
                    .title(" Message ")
                    .borders(Borders::ALL)
                    .border_style(border_style)
                    .padding(Padding::horizontal(1)),
            ),
        area,
    );
    if focused && app.opened_conversation().is_some() {
        let cursor_prefix = &app.input[..app.cursor];
        let cursor_x = area.x + 2 + cursor_prefix.chars().count() as u16;
        frame.set_cursor_position((cursor_x.min(area.right().saturating_sub(2)), area.y + 1));
    }
}

fn conversation_footer(app: &App) -> Line<'_> {
    let mut spans = vec![
        Span::styled("↑↓", primary()),
        Span::styled(" move   ", muted()),
        Span::styled("enter", primary()),
        Span::styled(" open   ", muted()),
        Span::styled("r", primary()),
        Span::styled(" refresh   ", muted()),
        Span::styled("d", primary()),
        Span::styled(" Disconnect   ", muted()),
        Span::styled("esc", primary()),
        Span::styled(" quit   ", muted()),
    ];
    spans.extend(status_spans(&app.status));
    Line::from(spans)
}

fn chat_footer(app: &App, wide: bool) -> Line<'_> {
    let mut spans = vec![
        Span::styled("enter", primary()),
        Span::styled(" send   ", muted()),
        Span::styled("esc", primary()),
        Span::styled(if wide { " sidebar   " } else { " chats   " }, muted()),
        Span::styled("pgup/dn", primary()),
        Span::styled(" scroll   ", muted()),
        Span::styled("ctrl-c", primary()),
        Span::styled(" quit   ", muted()),
    ];
    spans.extend(status_spans(&app.status));
    Line::from(spans)
}

fn render_footer(frame: &mut ratatui::Frame<'_>, line: Line<'_>, area: Rect) {
    frame.render_widget(
        Paragraph::new(line).block(Block::default().padding(Padding::horizontal(1))),
        area,
    );
}

fn draw_narrow_conversations(frame: &mut ratatui::Frame<'_>, app: &App) {
    let (header, body, footer) = shell(frame.area());
    render_logo(frame, header);
    frame.render_widget(conversation_list(app, false, true), body);
    render_footer(frame, conversation_footer(app), footer);
}

fn draw_narrow_chat(frame: &mut ratatui::Frame<'_>, app: &App) {
    let [header, messages, input, footer] = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(3),
            Constraint::Length(4),
            Constraint::Length(2),
        ])
        .areas(frame.area());
    render_chat_header(frame, app, header, true);
    render_messages(frame, app, messages);
    render_composer(frame, app, input, true);
    render_footer(frame, chat_footer(app, false), footer);
}

fn draw_wide(frame: &mut ratatui::Frame<'_>, app: &App) {
    let (header, body, footer) = shell(frame.area());
    let (sidebar_header, chat_header) = wide_columns(header);
    let (sidebar, chat) = wide_columns(body);
    let [messages, input] = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(3), Constraint::Length(4)])
        .areas(chat);

    render_logo(frame, sidebar_header);
    render_chat_header(frame, app, chat_header, false);
    frame.render_widget(
        conversation_list(app, true, app.screen == Screen::Conversations),
        sidebar,
    );
    render_messages(frame, app, messages);
    render_composer(frame, app, input, app.screen == Screen::Chat);
    let footer_line = if app.screen == Screen::Conversations {
        conversation_footer(app)
    } else {
        chat_footer(app, true)
    };
    render_footer(frame, footer_line, footer);
}

fn draw_disconnect_confirm(frame: &mut ratatui::Frame<'_>) {
    let (header, body, footer) = shell(frame.area());
    render_logo(frame, header);

    let warning = Text::from(vec![
        Line::styled(
            "Disconnect Signal CLI?",
            primary().add_modifier(Modifier::BOLD),
        ),
        Line::raw(""),
        Line::styled(
            "This removes local credentials, sessions, contacts, groups, and messages.",
            muted(),
        ),
        Line::styled(
            "Afterward, also remove “Signal CLI” on your iPhone under Settings → Linked Devices.",
            muted(),
        ),
    ]);
    frame.render_widget(
        Paragraph::new(warning).wrap(Wrap { trim: false }).block(
            Block::default()
                .title(" Confirm ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Red))
                .padding(Padding::new(2, 2, 1, 1)),
        ),
        body,
    );
    render_footer(
        frame,
        Line::from(vec![
            Span::styled("y", Style::default().fg(Color::Red)),
            Span::styled(" Disconnect and erase local data   ", muted()),
            Span::styled("esc/n", primary()),
            Span::styled(" Cancel", muted()),
        ]),
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
    use super::{muted, primary, shell, status_spans, wide_columns, wrap_text, SIDEBAR_WIDTH};
    use ratatui::{
        layout::Rect,
        style::{Color, Modifier},
    };

    #[test]
    fn wraps_at_word_boundaries() {
        assert_eq!(wrap_text("one two three", 7), vec!["one two", "three"]);
    }

    #[test]
    fn preserves_explicit_newlines() {
        assert_eq!(wrap_text("one\ntwo", 20), vec!["one", "two"]);
    }

    #[test]
    fn neutral_styles_follow_the_terminal_theme() {
        assert_eq!(primary().fg, Some(Color::Reset));
        assert_eq!(muted().fg, Some(Color::Reset));
        assert!(muted().add_modifier.contains(Modifier::DIM));
    }

    #[test]
    fn wide_layout_has_fixed_32_column_sidebar() {
        let (_, body, _) = shell(Rect::new(0, 0, 120, 40));
        let (sidebar, chat) = wide_columns(body);
        assert_eq!(sidebar.width, SIDEBAR_WIDTH);
        assert_eq!(chat.width, 120 - SIDEBAR_WIDTH);
        assert_eq!(chat.x, SIDEBAR_WIDTH);
    }

    #[test]
    fn connected_status_omits_device_name() {
        let spans = status_spans("Connected to Signal CLI");
        let rendered = spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert_eq!(rendered, "● Connected");
    }
}
