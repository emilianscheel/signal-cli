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
    backend::{self, ChatMessage, Conversation, NetworkEvent},
    ui::TerminalSession,
};

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
    pub messages: Vec<ChatMessage>,
    pub input: String,
    pub cursor: usize,
    pub scroll: u16,
    pub screen: Screen,
    pub status: String,
    pub sending: bool,
    receiver: Option<tokio::task::JoinHandle<()>>,
    disconnected: bool,
}

impl App {
    pub fn new(manager: Manager<SqliteStore, Registered>) -> Self {
        Self {
            manager,
            conversations: Vec::new(),
            selected: 0,
            messages: Vec::new(),
            input: String::new(),
            cursor: 0,
            scroll: 0,
            screen: Screen::Conversations,
            status: "Connecting…".into(),
            sending: false,
            receiver: None,
            disconnected: false,
        }
    }

    pub fn active(&self) -> Option<&Conversation> {
        self.conversations.get(self.selected)
    }

    async fn refresh_conversations(&mut self) -> Result<()> {
        let active = self.active().map(|c| c.id.clone());
        self.conversations = backend::conversations(&self.manager).await?;
        if let Some(id) = active {
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
        Ok(())
    }

    async fn open_selected(&mut self) -> Result<()> {
        let Some(conversation) = self.active().cloned() else {
            self.status =
                "No chats yet — leave Signal open on your phone while contacts sync".into();
            return Ok(());
        };
        self.messages = backend::history(&self.manager, &conversation).await?;
        self.scroll = 0;
        self.screen = Screen::Chat;
        self.status = "Connected".into();
        Ok(())
    }

    async fn send_input(&mut self) {
        let text = self.input.trim().to_string();
        let Some(conversation) = self.active().cloned() else {
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
                KeyCode::Enter => self.open_selected().await?,
                KeyCode::Char('r') => self.refresh_conversations().await?,
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
                if self.active().is_some_and(|c| c.id == id) && self.screen == Screen::Chat {
                    let conversation = self.active().cloned().expect("active chat");
                    self.messages = backend::history(&self.manager, &conversation).await?;
                    self.scroll = 0;
                }
                self.status = "New message".into();
            }
            NetworkEvent::ConversationsChanged => {
                self.refresh_conversations().await?;
                self.status = "Contacts synced".into();
            }
            NetworkEvent::QueueEmpty => self.status = "Connected".into(),
            NetworkEvent::Error(error) => self.status = error,
        }
        Ok(())
    }

    pub async fn run(mut self) -> Result<bool> {
        self.refresh_conversations().await?;
        let (network_tx, mut network_rx) = mpsc::unbounded_channel();
        self.receiver = Some(backend::start_receiver(self.manager.clone(), network_tx));
        let mut terminal = TerminalSession::start()?;
        let mut events = EventStream::new();

        loop {
            terminal.draw(&self)?;
            tokio::select! {
                event = events.next() => match event {
                    Some(Ok(Event::Key(key))) if self.handle_key(key).await? => break,
                    Some(Err(error)) => return Err(error.into()),
                    None => break,
                    _ => {}
                },
                Some(event) = network_rx.recv() => self.handle_network(event).await?,
            }
        }
        Ok(self.disconnected)
    }
}

#[cfg(test)]
mod tests {
    use super::Screen;

    #[test]
    fn screens_are_distinct() {
        assert_ne!(Screen::Chat, Screen::Conversations);
        assert_ne!(Screen::DisconnectConfirm, Screen::Conversations);
    }
}
