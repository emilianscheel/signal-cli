use std::{
    collections::HashMap,
    io::{self, Stdout},
    sync::mpsc::{self, Receiver},
    thread,
};

use anyhow::Result;
use chrono::{Local, TimeZone};
use crossterm::{
    execute,
    terminal::{
        disable_raw_mode, enable_raw_mode, Clear, ClearType, EnterAlternateScreen,
        LeaveAlternateScreen,
    },
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, List, ListItem, ListState, Padding, Paragraph, Wrap},
    Terminal,
};
use ratatui_image::{
    errors::Errors as ImageError,
    picker::{Picker, ProtocolType},
    thread::{ResizeRequest, ResizeResponse, ThreadImage, ThreadProtocol},
    Resize,
};
use unicode_width::UnicodeWidthChar;

use crate::{
    app::{App, LayoutMode, Screen},
    attachments::AttachmentState,
    backend::{ChatAttachment, MessageKind},
};

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
    picker: Option<Picker>,
    images: HashMap<String, AsyncImage>,
    redraw_tx: tokio::sync::mpsc::UnboundedSender<()>,
    redraw_rx: tokio::sync::mpsc::UnboundedReceiver<()>,
}

struct AsyncImage {
    protocol: ThreadProtocol,
    completed: Receiver<Result<ResizeResponse, ImageError>>,
}

impl AsyncImage {
    fn new(
        picker: Picker,
        image: image::DynamicImage,
        redraw: tokio::sync::mpsc::UnboundedSender<()>,
    ) -> Self {
        let (work_tx, work_rx) = mpsc::channel::<ResizeRequest>();
        let (completed_tx, completed) = mpsc::channel();
        thread::spawn(move || {
            while let Ok(request) = work_rx.recv() {
                if completed_tx.send(request.resize_encode()).is_err() {
                    break;
                }
                let _ = redraw.send(());
            }
        });
        Self {
            protocol: ThreadProtocol::new(work_tx, Some(picker.new_resize_protocol(image))),
            completed,
        }
    }

    fn apply_completed(&mut self) {
        while let Ok(result) = self.completed.try_recv() {
            if let Ok(response) = result {
                self.protocol.update_resized_protocol(response);
            }
        }
    }
}

impl TerminalSession {
    pub fn start() -> Result<Self> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        // Purge the primary buffer before switching screens so terminal
        // emulators cannot reveal pre-TUI output when the user scrolls back.
        execute!(stdout, Clear(ClearType::Purge), EnterAlternateScreen)?;
        let picker = Picker::from_query_stdio()
            .ok()
            .filter(|picker| picker.protocol_type() != ProtocolType::Halfblocks);
        let mut terminal = Terminal::new(CrosstermBackend::new(stdout))?;
        terminal.clear()?;
        let (redraw_tx, redraw_rx) = tokio::sync::mpsc::unbounded_channel();
        Ok(Self {
            terminal,
            picker,
            images: HashMap::new(),
            redraw_tx,
            redraw_rx,
        })
    }

    pub fn supports_images(&self) -> bool {
        self.picker.is_some()
    }

    pub fn clear_images(&mut self) {
        self.images.clear();
    }

    pub async fn redraw_requested(&mut self) {
        let _ = self.redraw_rx.recv().await;
    }

    pub fn draw(&mut self, app: &mut App) -> Result<Vec<String>> {
        self.images.retain(|key, _| {
            app.messages.iter().any(|message| {
                message
                    .attachments
                    .iter()
                    .any(|attachment| &attachment.key == key)
            })
        });
        let picker = self.picker;
        let images = &mut self.images;
        let redraw = self.redraw_tx.clone();
        let mut visible_images = Vec::new();
        self.terminal
            .draw(|frame| draw_app(frame, app, picker, images, &redraw, &mut visible_images))?;
        Ok(visible_images)
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(self.terminal.backend_mut(), LeaveAlternateScreen);
        let _ = self.terminal.show_cursor();
    }
}

fn draw_app(
    frame: &mut ratatui::Frame<'_>,
    app: &mut App,
    picker: Option<Picker>,
    images: &mut HashMap<String, AsyncImage>,
    redraw: &tokio::sync::mpsc::UnboundedSender<()>,
    visible_images: &mut Vec<String>,
) {
    if app.screen == Screen::DisconnectConfirm {
        draw_disconnect_confirm(frame);
        return;
    }
    match app.layout_mode() {
        LayoutMode::Narrow => match app.screen {
            Screen::Conversations => draw_narrow_conversations(frame, app),
            Screen::Chat => draw_narrow_chat(frame, app, picker, images, redraw, visible_images),
            Screen::DisconnectConfirm => unreachable!(),
        },
        LayoutMode::Wide => draw_wide(frame, app, picker, images, redraw, visible_images),
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
        let indices = app.filtered_conversation_indices();
        if indices.is_empty() {
            vec![ListItem::new(Text::from(vec![
                Line::from(format!("No chats match {:?}", app.search)),
                Line::styled("Press Esc to clear the search.", muted()),
            ]))]
        } else {
            indices
                .into_iter()
                .map(|index| {
                    let chat = &app.conversations[index];
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
        }
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

fn render_search(frame: &mut ratatui::Frame<'_>, app: &App, area: Rect, focused: bool) {
    let border_style = if focused {
        Style::default().fg(ACCENT)
    } else {
        muted()
    };
    let available = usize::from(area.width.saturating_sub(4));
    let before_cursor = &app.search[..app.search_cursor];
    let mut start = app.search_cursor;
    let mut width = 0;
    for (index, character) in before_cursor.char_indices().rev() {
        let character_width = character.width().unwrap_or(0);
        if width + character_width > available {
            break;
        }
        width += character_width;
        start = index;
    }
    let line = if app.search.is_empty() {
        Line::styled("Search chats…", muted())
    } else {
        Line::raw(app.search[start..].to_string())
    };
    frame.render_widget(
        Paragraph::new(line).block(
            Block::default()
                .title(" Search ")
                .borders(Borders::ALL)
                .border_style(border_style)
                .padding(Padding::horizontal(1)),
        ),
        area,
    );
    if focused {
        frame.set_cursor_position((
            (area.x + 2 + width as u16).min(area.right().saturating_sub(2)),
            area.y + 1,
        ));
    }
}

fn render_conversation_list(
    frame: &mut ratatui::Frame<'_>,
    app: &mut App,
    area: Rect,
    wide: bool,
    focused: bool,
) {
    let list = conversation_list(app, wide, focused);
    let selected = app.filtered_selection();
    let mut state = ListState::default()
        .with_offset(app.sidebar_offset)
        .with_selected(selected);
    frame.render_stateful_widget(list, area, &mut state);
    app.sidebar_offset = state.offset();
}

enum MessageElement {
    Text(Line<'static>),
    Image {
        key: String,
        name: String,
        height: u16,
    },
}

impl MessageElement {
    const fn height(&self) -> u16 {
        match self {
            Self::Text(_) => 1,
            Self::Image { height, .. } => *height,
        }
    }
}

fn human_size(size: Option<u32>) -> String {
    let Some(size) = size else {
        return "unknown size".into();
    };
    let size = f64::from(size);
    const UNITS: [&str; 4] = ["B", "KiB", "MiB", "GiB"];
    let mut value = size;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{} {}", value as u64, UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

fn friendly_type(content_type: Option<&str>) -> String {
    let value = content_type
        .and_then(|value| value.split(';').next())
        .map(str::trim)
        .unwrap_or("unknown type");
    match value {
        "application/pdf" => "PDF".into(),
        "image/jpeg" | "image/jpg" => "JPEG".into(),
        "image/png" => "PNG".into(),
        "image/webp" => "WebP".into(),
        "image/gif" => "GIF".into(),
        "video/mp4" => "MP4 video".into(),
        "video/quicktime" => "QuickTime video".into(),
        "unknown type" => value.into(),
        _ => value.to_string(),
    }
}

fn attachment_note(attachment: &ChatAttachment) -> Line<'static> {
    Line::styled(
        format!(
            "📎 {} · {} · {}",
            attachment.display_name(),
            friendly_type(attachment.content_type.as_deref()),
            human_size(attachment.size)
        ),
        muted(),
    )
}

fn image_failure(error: &str) -> &str {
    if error.contains("50 MiB") {
        "image is larger than 50 MiB"
    } else if error.contains("40 megapixel") {
        "image is larger than 40 megapixels"
    } else if error.contains("decode image") || error.contains("detect image format") {
        "unsupported or malformed image"
    } else {
        "could not load preview"
    }
}

fn image_height(attachment: &ChatAttachment, width: u16, picker: Picker) -> u16 {
    let max_width = (width.saturating_mul(2) / 5).max(1);
    let max_height = 12;
    let (Some(pixel_width), Some(pixel_height)) = (attachment.width, attachment.height) else {
        return max_height.min(8);
    };
    if pixel_width == 0 || pixel_height == 0 {
        return max_height.min(8);
    }
    let (cell_width, cell_height) = picker.font_size();
    let numerator = u64::from(max_width) * u64::from(pixel_height) * u64::from(cell_width);
    let denominator = u64::from(pixel_width) * u64::from(cell_height).max(1);
    u16::try_from(numerator.div_ceil(denominator))
        .unwrap_or(max_height)
        .clamp(1, max_height)
}

fn message_elements(
    app: &App,
    title: &str,
    width: u16,
    picker: Option<Picker>,
) -> Vec<MessageElement> {
    let mut elements = Vec::new();
    let wrap_width = usize::from(width.max(1));
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
        elements.push(MessageElement::Text(Line::from(vec![
            Span::styled(
                author,
                Style::default()
                    .fg(if message.mine { ACCENT } else { Color::Green })
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!("  {time}"), muted()),
        ])));
        let generic_attachment_body =
            message.kind == MessageKind::Attachment && message.body.starts_with("[attachment");
        if !message.body.is_empty() && !generic_attachment_body {
            elements.extend(
                wrap_text(&message.body, wrap_width)
                    .into_iter()
                    .map(|line| MessageElement::Text(Line::raw(line))),
            );
        }

        let mut inline_images = 0;
        for attachment in &message.attachments {
            let inline = picker.is_some() && attachment.can_preview() && inline_images < 3;
            if inline {
                inline_images += 1;
                match app.attachment_state(&attachment.key) {
                    AttachmentState::Failed(error) => {
                        elements.push(MessageElement::Text(Line::styled(
                            format!(
                                "Image unavailable: {} · {}",
                                attachment.display_name(),
                                image_failure(&error)
                            ),
                            muted(),
                        )))
                    }
                    AttachmentState::NotRequested
                    | AttachmentState::Loading
                    | AttachmentState::Ready(_) => elements.push(MessageElement::Image {
                        key: attachment.key.clone(),
                        name: attachment.display_name().to_string(),
                        height: image_height(attachment, width, picker.expect("picker exists")),
                    }),
                }
                elements.push(MessageElement::Text(attachment_note(attachment)));
            } else {
                elements.push(MessageElement::Text(attachment_note(attachment)));
            }
        }
        elements.push(MessageElement::Text(Line::raw("")));
    }
    elements
}

fn render_messages(
    frame: &mut ratatui::Frame<'_>,
    app: &App,
    area: Rect,
    picker: Option<Picker>,
    images: &mut HashMap<String, AsyncImage>,
    redraw: &tokio::sync::mpsc::UnboundedSender<()>,
    visible_images: &mut Vec<String>,
) {
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
    let block = Block::default()
        .borders(Borders::TOP)
        .padding(Padding::horizontal(2));
    let content = block.inner(area);
    frame.render_widget(block, area);
    if app.messages.is_empty() {
        frame.render_widget(
            Paragraph::new(Line::styled("No messages yet. Say hello.", muted())),
            content,
        );
        return;
    }

    let elements = message_elements(app, title, content.width, picker);
    let total_height = elements.iter().fold(0_u16, |height, element| {
        height.saturating_add(element.height())
    });
    let bottom = total_height.saturating_sub(content.height);
    let start = bottom.saturating_sub(app.scroll.min(bottom));
    let prefetch_start = start.saturating_sub(content.height);
    let prefetch_end = start.saturating_add(content.height.saturating_mul(2));
    let mut virtual_y = 0_u16;
    for element in elements {
        let height = element.height();
        let end = virtual_y.saturating_add(height);
        if let MessageElement::Image { key, .. } = &element {
            if end > prefetch_start && virtual_y < prefetch_end {
                visible_images.push(key.clone());
            }
        }
        if end > start && virtual_y < start.saturating_add(content.height) {
            let clipped_top = start.saturating_sub(virtual_y);
            let clipped_bottom = end.saturating_sub(start.saturating_add(content.height));
            let visible_height = height
                .saturating_sub(clipped_top)
                .saturating_sub(clipped_bottom);
            let y = content.y.saturating_add(virtual_y.saturating_sub(start));
            let row = Rect::new(content.x, y, content.width, visible_height);
            match element {
                MessageElement::Text(line) => frame.render_widget(Paragraph::new(line), row),
                MessageElement::Image { key, name, height } => {
                    if clipped_top > 0 || clipped_bottom > 0 {
                        frame.render_widget(
                            Paragraph::new(Line::styled("[image continues…]", muted())),
                            row,
                        );
                    } else {
                        match app.attachment_state(&key) {
                            AttachmentState::Ready(image) => {
                                let state = images.entry(key).or_insert_with(|| {
                                    AsyncImage::new(
                                        picker.expect("picker exists"),
                                        (*image).clone(),
                                        redraw.clone(),
                                    )
                                });
                                state.apply_completed();
                                frame.render_stateful_widget(
                                    ThreadImage::default().resize(Resize::Fit(None)),
                                    Rect::new(
                                        row.x,
                                        row.y,
                                        row.width.saturating_mul(2) / 5,
                                        height,
                                    ),
                                    &mut state.protocol,
                                );
                            }
                            AttachmentState::NotRequested | AttachmentState::Loading => {
                                frame.render_widget(
                                    Paragraph::new(Line::styled(
                                        format!("Loading {name}…"),
                                        muted(),
                                    )),
                                    row,
                                );
                            }
                            AttachmentState::Failed(_) => {}
                        }
                    }
                }
            }
        }
        virtual_y = end;
    }
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
        Span::styled("type", primary()),
        Span::styled(" search   ", muted()),
        Span::styled("↑↓", primary()),
        Span::styled(" move   ", muted()),
        Span::styled("enter", primary()),
        Span::styled(" open   ", muted()),
        Span::styled("ctrl-s", primary()),
        Span::styled(" sync   ", muted()),
        Span::styled("ctrl-r", primary()),
        Span::styled(" refresh   ", muted()),
        Span::styled("ctrl-d", primary()),
        Span::styled(" Disconnect   ", muted()),
        Span::styled("esc", primary()),
        Span::styled(
            if app.search.is_empty() {
                " quit   "
            } else {
                " clear   "
            },
            muted(),
        ),
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
        Span::styled("ctrl-s", primary()),
        Span::styled(" sync   ", muted()),
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

fn draw_narrow_conversations(frame: &mut ratatui::Frame<'_>, app: &mut App) {
    let (header, body, footer) = shell(frame.area());
    let [search, chats] = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(1)])
        .areas(body);
    render_logo(frame, header);
    render_search(frame, app, search, true);
    render_conversation_list(frame, app, chats, false, true);
    render_footer(frame, conversation_footer(app), footer);
}

fn draw_narrow_chat(
    frame: &mut ratatui::Frame<'_>,
    app: &App,
    picker: Option<Picker>,
    images: &mut HashMap<String, AsyncImage>,
    redraw: &tokio::sync::mpsc::UnboundedSender<()>,
    visible_images: &mut Vec<String>,
) {
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
    render_messages(frame, app, messages, picker, images, redraw, visible_images);
    render_composer(frame, app, input, true);
    render_footer(frame, chat_footer(app, false), footer);
}

fn draw_wide(
    frame: &mut ratatui::Frame<'_>,
    app: &mut App,
    picker: Option<Picker>,
    images: &mut HashMap<String, AsyncImage>,
    redraw: &tokio::sync::mpsc::UnboundedSender<()>,
    visible_images: &mut Vec<String>,
) {
    let (header, body, footer) = shell(frame.area());
    let (sidebar_header, chat_header) = wide_columns(header);
    let (sidebar, chat) = wide_columns(body);
    let [search, chats] = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(1)])
        .areas(sidebar);
    let [messages, input] = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(3), Constraint::Length(4)])
        .areas(chat);

    render_logo(frame, sidebar_header);
    render_chat_header(frame, app, chat_header, false);
    let sidebar_focused = app.screen == Screen::Conversations;
    render_search(frame, app, search, sidebar_focused);
    render_conversation_list(frame, app, chats, true, sidebar_focused);
    render_messages(frame, app, messages, picker, images, redraw, visible_images);
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
            Span::styled("ctrl-y", Style::default().fg(Color::Red)),
            Span::styled(" Disconnect and erase local data   ", muted()),
            Span::styled("esc/ctrl-n", primary()),
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
    use super::{
        friendly_type, human_size, image_failure, image_height, muted, primary, shell,
        status_spans, wide_columns, wrap_text, SIDEBAR_WIDTH,
    };
    use crate::backend::ChatAttachment;
    use presage::libsignal_service::proto::AttachmentPointer;
    use ratatui::{
        buffer::Buffer,
        layout::Rect,
        style::{Color, Modifier},
        text::{Line, Text},
        widgets::{List, ListItem, ListState, StatefulWidget},
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

    #[test]
    fn attachment_metadata_is_human_readable() {
        assert_eq!(human_size(Some(2_516_582)), "2.4 MiB");
        assert_eq!(human_size(None), "unknown size");
        assert_eq!(friendly_type(Some("application/pdf")), "PDF");
        assert_eq!(friendly_type(Some("video/mp4")), "MP4 video");
        assert_eq!(
            image_failure("decode image: bad data"),
            "unsupported or malformed image"
        );
    }

    #[test]
    fn inline_images_fit_the_balanced_height_limit() {
        let pointer = AttachmentPointer {
            digest: Some(vec![1; 32]),
            content_type: Some("image/png".into()),
            width: Some(800),
            height: Some(400),
            ..Default::default()
        };
        let attachment = ChatAttachment {
            key: "image".into(),
            file_name: Some("image.png".into()),
            content_type: pointer.content_type.clone(),
            size: Some(1_024),
            width: pointer.width,
            height: pointer.height,
            pointer,
        };
        let picker = ratatui_image::picker::Picker::from_fontsize((10, 20));
        assert_eq!(image_height(&attachment, 80, picker), 8);
        assert!(image_height(&attachment, 20, picker) <= 12);
    }

    #[test]
    fn stateful_chat_list_scrolls_to_the_last_selected_item() {
        let items = (0..10)
            .map(|index| {
                ListItem::new(Text::from(vec![
                    Line::raw(format!("Chat {index}")),
                    Line::raw("subtitle"),
                    Line::raw(""),
                ]))
            })
            .collect::<Vec<_>>();
        let list = List::new(items);
        let area = Rect::new(0, 0, 24, 6);
        let mut buffer = Buffer::empty(area);
        let mut state = ListState::default().with_selected(Some(9));

        StatefulWidget::render(list, area, &mut buffer, &mut state);

        assert_eq!(state.selected(), Some(9));
        assert_eq!(state.offset(), 8);
    }
}
