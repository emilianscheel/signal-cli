use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::Arc,
};

use anyhow::Result;
use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyModifiers};
use futures::StreamExt;
use presage::{
    manager::Registered,
    store::{ContentsStore, StateStore, Store},
    Manager,
};
use presage_store_sqlite::SqliteStore;
use tokio::sync::mpsc;

use crate::{
    attachments::{self, AttachmentCache, AttachmentEvent, AttachmentState},
    backend::{self, ChatMessage, Conversation, NetworkEvent},
    preferences::PreferencesStore,
    ui::TerminalSession,
};

pub const WIDE_BREAKPOINT: u16 = 120;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayoutMode {
    Narrow,
    Wide,
}

impl LayoutMode {
    pub const fn for_width(width: u16) -> Self {
        if width >= WIDE_BREAKPOINT {
            Self::Wide
        } else {
            Self::Narrow
        }
    }
}

fn remembered_selection(
    conversations: &[Conversation],
    remembered: Option<&backend::ConversationId>,
) -> usize {
    remembered
        .and_then(|id| {
            conversations
                .iter()
                .position(|conversation| &conversation.id == id)
        })
        .unwrap_or(0)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    Conversations,
    Chat,
    DisconnectConfirm,
}

pub struct App {
    manager: Manager<SqliteStore, Registered>,
    pub conversations: Vec<Conversation>,
    pub selected: usize,
    opened: Option<backend::ConversationId>,
    pub messages: Vec<ChatMessage>,
    pub input: String,
    pub cursor: usize,
    pub scroll: u16,
    pub screen: Screen,
    pub status: String,
    pub sending: bool,
    pub terminal_width: u16,
    preferences: PreferencesStore,
    receiver: Option<tokio::task::JoinHandle<()>>,
    disconnected: bool,
    attachment_cache: AttachmentCache,
    pub attachment_states: HashMap<String, AttachmentState>,
    attachment_slots: Arc<tokio::sync::Semaphore>,
}

impl App {
    pub fn new(
        manager: Manager<SqliteStore, Registered>,
        preferences: PreferencesStore,
        attachment_cache_path: PathBuf,
    ) -> Self {
        Self {
            manager,
            conversations: Vec::new(),
            selected: 0,
            opened: None,
            messages: Vec::new(),
            input: String::new(),
            cursor: 0,
            scroll: 0,
            screen: Screen::Conversations,
            status: "Connecting…".into(),
            sending: false,
            terminal_width: 0,
            preferences,
            receiver: None,
            disconnected: false,
            attachment_cache: AttachmentCache::new(attachment_cache_path),
            attachment_states: HashMap::new(),
            attachment_slots: Arc::new(tokio::sync::Semaphore::new(2)),
        }
    }

    pub fn attachment_state(&self, key: &str) -> AttachmentState {
        self.attachment_states
            .get(key)
            .cloned()
            .unwrap_or(AttachmentState::NotRequested)
    }

    fn retain_current_attachment_states(&mut self) {
        let keys = self
            .messages
            .iter()
            .flat_map(|message| message.attachments.iter().map(|attachment| &attachment.key))
            .collect::<HashSet<_>>();
        self.attachment_states.retain(|key, _| keys.contains(key));
    }

    pub fn selected_conversation(&self) -> Option<&Conversation> {
        self.conversations.get(self.selected)
    }

    pub fn opened_conversation(&self) -> Option<&Conversation> {
        let opened = self.opened.as_ref()?;
        self.conversations
            .iter()
            .find(|conversation| &conversation.id == opened)
    }

    pub const fn layout_mode(&self) -> LayoutMode {
        LayoutMode::for_width(self.terminal_width)
    }

    async fn refresh_conversations(&mut self) -> Result<()> {
        let selected = self.selected_conversation().map(|c| c.id.clone());
        let opened = self.opened.clone();
        self.conversations = backend::conversations(&self.manager).await?;
        if let Some(id) = selected {
            self.selected = self
                .conversations
                .iter()
                .position(|c| c.id == id)
                .unwrap_or(0);
        } else {
            self.selected = self
                .selected
                .min(self.conversations.len().saturating_sub(1));
        }
        self.opened = opened.filter(|id| {
            self.conversations
                .iter()
                .any(|conversation| &conversation.id == id)
        });
        if self.opened.is_none() {
            self.messages.clear();
        }
        Ok(())
    }

    async fn load_selected(&mut self, focus_chat: bool) -> Result<()> {
        let Some(conversation) = self.selected_conversation().cloned() else {
            self.status =
                "No chats yet — leave Signal open on your phone while contacts sync".into();
            return Ok(());
        };
        self.messages = backend::history(&self.manager, &conversation).await?;
        self.retain_current_attachment_states();
        self.opened = Some(conversation.id.clone());
        self.scroll = 0;
        if focus_chat {
            self.screen = Screen::Chat;
        }
        self.status = match self.preferences.save_last_conversation(&conversation.id) {
            Ok(()) => "Connected".into(),
            Err(error) => format!("Connected · could not remember chat: {error:#}"),
        };
        Ok(())
    }

    async fn ensure_wide_chat(&mut self) -> Result<()> {
        if self.layout_mode() == LayoutMode::Wide
            && self.opened.is_none()
            && !self.conversations.is_empty()
            && self.screen != Screen::DisconnectConfirm
        {
            self.load_selected(false).await?;
        }
        Ok(())
    }

    async fn handle_resize(&mut self, width: u16) -> Result<()> {
        let previous = self.layout_mode();
        self.terminal_width = width;
        if previous == LayoutMode::Narrow && self.layout_mode() == LayoutMode::Wide {
            self.ensure_wide_chat().await?;
        }
        Ok(())
    }

    async fn send_input(&mut self) {
        let text = self.input.trim().to_string();
        let Some(conversation) = self.opened_conversation().cloned() else {
            return;
        };
        if text.is_empty() || self.sending {
            return;
        }

        self.sending = true;
        self.status = "Sending…".into();
        match backend::send(&mut self.manager, &conversation, text).await {
            Ok(message) => {
                self.messages.push(message);
                self.scroll = 0;
                self.input.clear();
                self.cursor = 0;
                self.status = "Sent".into();
            }
            Err(error) => self.status = format!("Send failed: {error:#}"),
        }
        self.sending = false;
    }

    async fn disconnect(&mut self) -> Result<()> {
        if let Some(receiver) = self.receiver.take() {
            receiver.abort();
            let _ = receiver.await;
        }
        let mut store = self.manager.store().clone();

        // Secondary Signal devices cannot revoke themselves server-side. Remove
        // every locally usable credential first, then erase cached content.
        StateStore::clear_registration(&mut store).await?;
        ContentsStore::clear_contents(&mut store).await?;
        Store::clear(&mut store).await?;
        self.disconnected = true;
        Ok(())
    }

    async fn handle_key(&mut self, key: KeyEvent) -> Result<bool> {
        if key.kind != crossterm::event::KeyEventKind::Press {
            return Ok(false);
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return Ok(true);
        }
        match self.screen {
            Screen::Conversations => match key.code {
                KeyCode::Char('q') | KeyCode::Esc => return Ok(true),
                KeyCode::Up | KeyCode::Char('k') => self.selected = self.selected.saturating_sub(1),
                KeyCode::Down | KeyCode::Char('j') => {
                    self.selected =
                        (self.selected + 1).min(self.conversations.len().saturating_sub(1));
                }
                KeyCode::Enter => self.load_selected(true).await?,
                KeyCode::Char('r') => {
                    self.refresh_conversations().await?;
                    self.ensure_wide_chat().await?;
                }
                KeyCode::Char('d') => self.screen = Screen::DisconnectConfirm,
                _ => {}
            },
            Screen::Chat => match key.code {
                KeyCode::Esc => self.screen = Screen::Conversations,
                KeyCode::PageUp => self.scroll = self.scroll.saturating_add(5),
                KeyCode::PageDown => self.scroll = self.scroll.saturating_sub(5),
                KeyCode::Enter if !key.modifiers.contains(KeyModifiers::SHIFT) => {
                    self.send_input().await
                }
                KeyCode::Char(c) => {
                    self.input.insert(self.cursor, c);
                    self.cursor += c.len_utf8();
                }
                KeyCode::Backspace if self.cursor > 0 => {
                    let previous = self.input[..self.cursor]
                        .char_indices()
                        .next_back()
                        .map(|(i, _)| i)
                        .unwrap_or(0);
                    self.input.drain(previous..self.cursor);
                    self.cursor = previous;
                }
                KeyCode::Delete if self.cursor < self.input.len() => {
                    let next = self.input[self.cursor..]
                        .chars()
                        .next()
                        .map(char::len_utf8)
                        .unwrap_or(0);
                    self.input.drain(self.cursor..self.cursor + next);
                }
                KeyCode::Left => {
                    self.cursor = self.input[..self.cursor]
                        .char_indices()
                        .next_back()
                        .map(|(i, _)| i)
                        .unwrap_or(0)
                }
                KeyCode::Right if self.cursor < self.input.len() => {
                    self.cursor += self.input[self.cursor..]
                        .chars()
                        .next()
                        .map(char::len_utf8)
                        .unwrap_or(0);
                }
                KeyCode::Home => self.cursor = 0,
                KeyCode::End => self.cursor = self.input.len(),
                _ => {}
            },
            Screen::DisconnectConfirm => match key.code {
                KeyCode::Char('y') => match self.disconnect().await {
                    Ok(()) => return Ok(true),
                    Err(error) => {
                        self.status = format!("Disconnect failed: {error:#}");
                        self.screen = Screen::Conversations;
                    }
                },
                KeyCode::Esc | KeyCode::Char('n') => self.screen = Screen::Conversations,
                _ => {}
            },
        }
        Ok(false)
    }

    async fn handle_network(&mut self, event: NetworkEvent) -> Result<()> {
        match event {
            NetworkEvent::Message(id) => {
                if self.opened.as_ref().is_some_and(|opened| opened == &id) {
                    let conversation = self
                        .opened_conversation()
                        .cloned()
                        .expect("opened chat exists");
                    self.messages = backend::history(&self.manager, &conversation).await?;
                    self.retain_current_attachment_states();
                    self.scroll = 0;
                }
                self.status = "New message".into();
            }
            NetworkEvent::ConversationsChanged => {
                self.refresh_conversations().await?;
                self.ensure_wide_chat().await?;
                self.status = "Contacts synced".into();
            }
            NetworkEvent::QueueEmpty => self.status = "Connected".into(),
            NetworkEvent::Error(error) => self.status = error,
        }
        Ok(())
    }

    fn queue_visible_images(
        &mut self,
        keys: Vec<String>,
        tx: &mpsc::UnboundedSender<AttachmentEvent>,
    ) {
        for key in keys {
            if !matches!(self.attachment_state(&key), AttachmentState::NotRequested) {
                continue;
            }
            let Some(attachment) = self
                .messages
                .iter()
                .flat_map(|message| &message.attachments)
                .find(|attachment| attachment.key == key && attachment.can_preview())
                .cloned()
            else {
                continue;
            };
            self.attachment_states
                .insert(key.clone(), AttachmentState::Loading);
            let manager = self.manager.clone();
            let cache = self.attachment_cache.clone();
            let slots = self.attachment_slots.clone();
            let tx = tx.clone();
            tokio::task::spawn_local(async move {
                let Ok(_permit) = slots.acquire_owned().await else {
                    return;
                };
                let event = attachments::load_image(manager, cache, attachment).await;
                let _ = tx.send(event);
            });
        }
    }

    fn handle_attachment(&mut self, event: AttachmentEvent) {
        let state = match event.result {
            Ok(image) => AttachmentState::Ready(image),
            Err(error) => AttachmentState::Failed(error),
        };
        self.attachment_states.insert(event.key, state);
    }

    pub async fn run(mut self) -> Result<bool> {
        self.terminal_width = crossterm::terminal::size()?.0;
        let remembered = self.preferences.load_last_conversation();
        self.refresh_conversations().await?;
        self.selected = remembered_selection(&self.conversations, remembered.as_ref());
        self.ensure_wide_chat().await?;
        let (network_tx, mut network_rx) = mpsc::unbounded_channel();
        self.receiver = Some(backend::start_receiver(self.manager.clone(), network_tx));
        let mut terminal = TerminalSession::start()?;
        let mut events = EventStream::new();
        let (attachment_tx, mut attachment_rx) = mpsc::unbounded_channel();

        loop {
            let visible_images = terminal.draw(&mut self)?;
            if terminal.supports_images() {
                self.queue_visible_images(visible_images, &attachment_tx);
            }
            tokio::select! {
                event = events.next() => match event {
                    Some(Ok(Event::Key(key))) if self.handle_key(key).await? => break,
                    Some(Ok(Event::Resize(width, _))) => {
                        self.handle_resize(width).await?;
                        terminal.clear_images();
                    },
                    Some(Err(error)) => return Err(error.into()),
                    None => break,
                    _ => {}
                },
                Some(event) = network_rx.recv() => self.handle_network(event).await?,
                Some(event) = attachment_rx.recv() => self.handle_attachment(event),
                _ = terminal.redraw_requested(), if terminal.supports_images() => {},
            }
        }
        Ok(self.disconnected)
    }
}

#[cfg(test)]
mod tests {
    use super::{remembered_selection, LayoutMode, Screen, WIDE_BREAKPOINT};
    use crate::backend::{Conversation, ConversationId};

    #[test]
    fn screens_are_distinct() {
        assert_ne!(Screen::Chat, Screen::Conversations);
        assert_ne!(Screen::DisconnectConfirm, Screen::Conversations);
    }

    #[test]
    fn layout_switches_at_120_columns() {
        assert_eq!(WIDE_BREAKPOINT, 120);
        assert_eq!(LayoutMode::for_width(119), LayoutMode::Narrow);
        assert_eq!(LayoutMode::for_width(120), LayoutMode::Wide);
        assert_eq!(LayoutMode::for_width(200), LayoutMode::Wide);
    }

    #[test]
    fn remembered_chat_is_selected_with_first_chat_fallback() {
        let conversations = vec![
            Conversation {
                id: ConversationId::Group([1; 32]),
                title: "One".into(),
                subtitle: String::new(),
                group_revision: Some(1),
            },
            Conversation {
                id: ConversationId::Group([2; 32]),
                title: "Two".into(),
                subtitle: String::new(),
                group_revision: Some(1),
            },
        ];
        assert_eq!(
            remembered_selection(&conversations, Some(&ConversationId::Group([2; 32]))),
            1
        );
        assert_eq!(
            remembered_selection(&conversations, Some(&ConversationId::Group([9; 32]))),
            0
        );
        assert_eq!(remembered_selection(&conversations, None), 0);
    }
}
