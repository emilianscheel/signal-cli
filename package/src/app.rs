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
    sync::{self, LinkSyncPaths, SyncReport},
    ui::TerminalSession,
    updater::UpdateMonitor,
};

pub const WIDE_BREAKPOINT: u16 = 120;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayoutMode {
    Narrow,
    Wide,
}

fn matching_conversation_indices(conversations: &[Conversation], query: &str) -> Vec<usize> {
    let query = query.to_lowercase();
    conversations
        .iter()
        .enumerate()
        .filter(|(_, conversation)| {
            query.is_empty()
                || conversation.title.to_lowercase().contains(&query)
                || conversation.subtitle.to_lowercase().contains(&query)
        })
        .map(|(index, _)| index)
        .collect()
}

fn reconciled_selection(indices: &[usize], selected: usize) -> Option<usize> {
    indices
        .contains(&selected)
        .then_some(selected)
        .or_else(|| indices.first().copied())
}

fn moved_selection(indices: &[usize], selected: usize, down: bool) -> Option<usize> {
    let current = indices.iter().position(|index| *index == selected)?;
    let next = if down {
        (current + 1).min(indices.len().saturating_sub(1))
    } else {
        current.saturating_sub(1)
    };
    indices.get(next).copied()
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
    pub search: String,
    pub search_cursor: usize,
    pub sidebar_offset: usize,
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
    network_tx: Option<mpsc::UnboundedSender<NetworkEvent>>,
    sync_tx: Option<mpsc::UnboundedSender<Result<SyncReport, String>>>,
    pub syncing: bool,
    disconnected: bool,
    link_sync_paths: LinkSyncPaths,
    attachment_cache: AttachmentCache,
    pub attachment_states: HashMap<String, AttachmentState>,
    attachment_slots: Arc<tokio::sync::Semaphore>,
    pub update_notice: Option<String>,
    update_monitor: Option<UpdateMonitor>,
}

impl App {
    pub fn new(
        manager: Manager<SqliteStore, Registered>,
        preferences: PreferencesStore,
        attachment_cache_path: PathBuf,
        link_sync_paths: LinkSyncPaths,
        update_notice: Option<String>,
        update_monitor: Option<UpdateMonitor>,
    ) -> Self {
        Self {
            manager,
            conversations: Vec::new(),
            selected: 0,
            search: String::new(),
            search_cursor: 0,
            sidebar_offset: 0,
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
            network_tx: None,
            sync_tx: None,
            syncing: false,
            disconnected: false,
            link_sync_paths,
            attachment_cache: AttachmentCache::new(attachment_cache_path),
            attachment_states: HashMap::new(),
            attachment_slots: Arc::new(tokio::sync::Semaphore::new(2)),
            update_notice,
            update_monitor,
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
        self.filtered_conversation_indices()
            .contains(&self.selected)
            .then(|| self.conversations.get(self.selected))
            .flatten()
    }

    pub fn filtered_conversation_indices(&self) -> Vec<usize> {
        matching_conversation_indices(&self.conversations, &self.search)
    }

    pub fn filtered_selection(&self) -> Option<usize> {
        self.filtered_conversation_indices()
            .iter()
            .position(|index| *index == self.selected)
    }

    fn reconcile_search_selection(&mut self) {
        let indices = self.filtered_conversation_indices();
        if let Some(selected) = reconciled_selection(&indices, self.selected) {
            if selected != self.selected {
                self.selected = selected;
                self.sidebar_offset = 0;
            }
        } else {
            self.sidebar_offset = 0;
        }
    }

    fn move_sidebar_selection(&mut self, down: bool) {
        let indices = self.filtered_conversation_indices();
        let Some(next) = moved_selection(&indices, self.selected, down) else {
            if let Some(first) = indices.first() {
                self.selected = *first;
                self.sidebar_offset = 0;
            }
            return;
        };
        self.selected = next;
    }

    fn insert_search_char(&mut self, character: char) {
        self.search.insert(self.search_cursor, character);
        self.search_cursor += character.len_utf8();
        self.reconcile_search_selection();
    }

    fn remove_search_char_before_cursor(&mut self) {
        if self.search_cursor == 0 {
            return;
        }
        let previous = self.search[..self.search_cursor]
            .char_indices()
            .next_back()
            .map(|(index, _)| index)
            .unwrap_or(0);
        self.search.drain(previous..self.search_cursor);
        self.search_cursor = previous;
        self.reconcile_search_selection();
    }

    fn remove_search_char_at_cursor(&mut self) {
        if self.search_cursor >= self.search.len() {
            return;
        }
        let next = self.search[self.search_cursor..]
            .chars()
            .next()
            .map(char::len_utf8)
            .unwrap_or(0);
        self.search
            .drain(self.search_cursor..self.search_cursor + next);
        self.reconcile_search_selection();
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
        let selected = self.conversations.get(self.selected).map(|c| c.id.clone());
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
        self.reconcile_search_selection();
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
        self.link_sync_paths.cleanup()?;
        self.disconnected = true;
        Ok(())
    }

    async fn start_pending_sync(&mut self) {
        if self.syncing {
            self.status = "Sync already in progress".into();
            return;
        }
        if let Some(receiver) = self.receiver.take() {
            receiver.abort();
            let _ = receiver.await;
        }
        let Some(tx) = self.sync_tx.clone() else {
            self.status = "Sync unavailable".into();
            return;
        };
        self.syncing = true;
        self.status = "Syncing contacts and queued messages…".into();
        let manager = self.manager.clone();
        tokio::task::spawn_local(async move {
            let result =
                sync::refresh_pending(&manager, std::time::Duration::from_secs(45), |_| {})
                    .await
                    .map_err(|error| format!("{error:#}"));
            let _ = tx.send(result);
        });
    }

    async fn finish_pending_sync(&mut self, result: Result<SyncReport, String>) {
        self.syncing = false;
        if let Some(tx) = self.network_tx.clone() {
            self.receiver = Some(backend::start_receiver(self.manager.clone(), tx));
        }
        let refresh = self.refresh_conversations().await;
        if refresh.is_ok() && self.opened.is_some() {
            if let Some(conversation) = self.opened_conversation().cloned() {
                match backend::history(&self.manager, &conversation).await {
                    Ok(messages) => self.messages = messages,
                    Err(error) => {
                        self.status = format!("Sync finished; chat refresh failed: {error:#}");
                        return;
                    }
                }
            }
        }
        match (result, refresh) {
            (Ok(report), Ok(())) => {
                let contacts = if report.contacts_updated {
                    "contacts updated"
                } else {
                    "contacts unchanged"
                };
                self.status = format!(
                    "Sync complete · {} queued message{} · {contacts}",
                    report.received_messages,
                    if report.received_messages == 1 {
                        ""
                    } else {
                        "s"
                    }
                );
            }
            (Err(error), _) => self.status = format!("Sync failed: {error}"),
            (Ok(_), Err(error)) => self.status = format!("Sync refresh failed: {error:#}"),
        }
    }

    async fn handle_key(&mut self, key: KeyEvent) -> Result<bool> {
        if key.kind != crossterm::event::KeyEventKind::Press {
            return Ok(false);
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return Ok(true);
        }
        if self.screen != Screen::DisconnectConfirm
            && key.modifiers.contains(KeyModifiers::CONTROL)
            && key.code == KeyCode::Char('s')
        {
            self.start_pending_sync().await;
            return Ok(false);
        }
        match self.screen {
            Screen::Conversations => match key.code {
                KeyCode::Esc if !self.search.is_empty() => {
                    self.search.clear();
                    self.search_cursor = 0;
                    self.reconcile_search_selection();
                }
                KeyCode::Esc => return Ok(true),
                KeyCode::Up => self.move_sidebar_selection(false),
                KeyCode::Down => self.move_sidebar_selection(true),
                KeyCode::Enter => self.load_selected(true).await?,
                KeyCode::Char('r') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.refresh_conversations().await?;
                    self.ensure_wide_chat().await?;
                }
                KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.screen = Screen::DisconnectConfirm
                }
                KeyCode::Char(character)
                    if !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                {
                    self.insert_search_char(character)
                }
                KeyCode::Backspace => self.remove_search_char_before_cursor(),
                KeyCode::Delete => self.remove_search_char_at_cursor(),
                KeyCode::Left => {
                    self.search_cursor = self.search[..self.search_cursor]
                        .char_indices()
                        .next_back()
                        .map(|(index, _)| index)
                        .unwrap_or(0)
                }
                KeyCode::Right if self.search_cursor < self.search.len() => {
                    self.search_cursor += self.search[self.search_cursor..]
                        .chars()
                        .next()
                        .map(char::len_utf8)
                        .unwrap_or(0);
                }
                KeyCode::Home => self.search_cursor = 0,
                KeyCode::End => self.search_cursor = self.search.len(),
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
                KeyCode::Char('y') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    match self.disconnect().await {
                        Ok(()) => return Ok(true),
                        Err(error) => {
                            self.status = format!("Disconnect failed: {error:#}");
                            self.screen = Screen::Conversations;
                        }
                    }
                }
                KeyCode::Esc => self.screen = Screen::Conversations,
                KeyCode::Char('n') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.screen = Screen::Conversations
                }
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
        self.network_tx = Some(network_tx.clone());
        self.receiver = Some(backend::start_receiver(self.manager.clone(), network_tx));
        let (sync_tx, mut sync_rx) = mpsc::unbounded_channel();
        self.sync_tx = Some(sync_tx);
        let mut terminal = TerminalSession::start()?;
        let mut events = EventStream::new();
        let (attachment_tx, mut attachment_rx) = mpsc::unbounded_channel();
        let (update_tx, mut update_rx) = mpsc::unbounded_channel();
        if let Some(monitor) = self.update_monitor.take() {
            tokio::task::spawn_local(async move {
                if let Some(notice) = monitor.wait().await {
                    let _ = update_tx.send(notice);
                }
            });
        }
        let mut update_notice_deadline = self
            .update_notice
            .as_ref()
            .map(|_| tokio::time::Instant::now() + std::time::Duration::from_secs(8));

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
                Some(result) = sync_rx.recv() => self.finish_pending_sync(result).await,
                Some(notice) = update_rx.recv() => {
                    self.update_notice = Some(notice);
                    update_notice_deadline = Some(tokio::time::Instant::now() + std::time::Duration::from_secs(8));
                },
                _ = async {
                    match update_notice_deadline {
                        Some(deadline) => tokio::time::sleep_until(deadline).await,
                        None => futures::future::pending().await,
                    }
                }, if update_notice_deadline.is_some() => {
                    self.update_notice = None;
                    update_notice_deadline = None;
                },
                _ = terminal.redraw_requested(), if terminal.supports_images() => {},
            }
        }
        Ok(self.disconnected)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        matching_conversation_indices, moved_selection, reconciled_selection, remembered_selection,
        LayoutMode, Screen, WIDE_BREAKPOINT,
    };
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

    #[test]
    fn search_matches_names_and_subtitles_case_insensitively() {
        let conversations = vec![
            Conversation {
                id: ConversationId::Group([1; 32]),
                title: "Family Plans".into(),
                subtitle: "4 members".into(),
                group_revision: Some(1),
            },
            Conversation {
                id: ConversationId::Group([2; 32]),
                title: "Vera Scheel".into(),
                subtitle: "+49 152 123456".into(),
                group_revision: Some(1),
            },
        ];

        assert_eq!(
            matching_conversation_indices(&conversations, "fAmIlY"),
            vec![0]
        );
        assert_eq!(
            matching_conversation_indices(&conversations, "152"),
            vec![1]
        );
        assert_eq!(
            matching_conversation_indices(&conversations, "missing"),
            Vec::<usize>::new()
        );
        assert_eq!(
            matching_conversation_indices(&conversations, ""),
            vec![0, 1]
        );
    }

    #[test]
    fn filtered_selection_is_preserved_or_moves_to_the_first_match() {
        let matches = vec![1, 3, 5];
        assert_eq!(reconciled_selection(&matches, 3), Some(3));
        assert_eq!(reconciled_selection(&matches, 2), Some(1));
        assert_eq!(reconciled_selection(&[], 2), None);
        assert_eq!(moved_selection(&matches, 1, true), Some(3));
        assert_eq!(moved_selection(&matches, 5, true), Some(5));
        assert_eq!(moved_selection(&matches, 3, false), Some(1));
        assert_eq!(moved_selection(&matches, 1, false), Some(1));
    }
}
