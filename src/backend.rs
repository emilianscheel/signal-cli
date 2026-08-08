use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use futures::{channel::oneshot, future, StreamExt};
use presage::{
    libsignal_service::{
        configuration::SignalServers,
        content::{Content, ContentBody, DataMessage, GroupContextV2},
        protocol::ServiceId,
    },
    manager::Registered,
    model::{contacts::Contact, groups::Group, messages::Received},
    store::{ContentExt, ContentsStore, Thread},
    Manager,
};
use presage_store_sqlite::SqliteStore;
use qrcode::{Color as QrColor, QrCode};
use tokio::sync::mpsc;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConversationId {
    Contact(ServiceId),
    Group([u8; 32]),
}

impl ConversationId {
    pub fn thread(&self) -> Thread {
        match self {
            Self::Contact(id) => Thread::Contact(*id),
            Self::Group(key) => Thread::Group(*key),
        }
    }
}

#[derive(Clone, Debug)]
pub struct Conversation {
    pub id: ConversationId,
    pub title: String,
    pub subtitle: String,
    pub group_revision: Option<u32>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChatMessage {
    pub timestamp: u64,
    pub mine: bool,
    pub sender: Option<String>,
    pub body: String,
}

#[derive(Debug)]
pub enum NetworkEvent {
    Message(ConversationId),
    ConversationsChanged,
    QueueEmpty,
    Error(String),
}

fn transparent_qr(value: &str) -> Result<String> {
    const QUIET_ZONE: usize = 2;

    let code = QrCode::new(value.as_bytes()).context("encode provisioning QR code")?;
    let width = code.width();
    let row_width = (width + QUIET_ZONE * 2) * 2 + 1;
    let mut output = String::with_capacity(row_width * (width + QUIET_ZONE * 2));
    let empty_row = " ".repeat((width + QUIET_ZONE * 2) * 2);

    for _ in 0..QUIET_ZONE {
        output.push_str(&empty_row);
        output.push('\n');
    }
    for y in 0..width {
        output.push_str(&" ".repeat(QUIET_ZONE * 2));
        for x in 0..width {
            output.push_str(if code[(x, y)] == QrColor::Dark {
                "██"
            } else {
                "  "
            });
        }
        output.push_str(&" ".repeat(QUIET_ZONE * 2));
        output.push('\n');
    }
    for _ in 0..QUIET_ZONE {
        output.push_str(&empty_row);
        output.push('\n');
    }

    Ok(output)
}

pub async fn link_device(
    store: SqliteStore,
    device_name: String,
) -> Result<Manager<SqliteStore, Registered>> {
    println!("Welcome to Signal CLI\n");
    println!("On your iPhone, open Signal → Settings → Linked Devices → Link New Device.\n");

    let (tx, rx) = oneshot::channel();
    let (manager, shown) = future::join(
        Manager::link_secondary_device(store, SignalServers::Production, device_name, tx),
        async move {
            let url = rx.await.context("Signal provisioning was cancelled")?;
            print!("{}", transparent_qr(url.as_str())?);
            Ok::<_, anyhow::Error>(())
        },
    )
    .await;
    shown?;
    manager.context("link this terminal as a Signal device")
}

pub async fn conversations(
    manager: &Manager<SqliteStore, Registered>,
) -> Result<Vec<Conversation>> {
    let mut result = Vec::new();

    for contact in manager.store().contacts().await?.flatten() {
        let Contact {
            uuid,
            phone_number,
            name,
            ..
        } = contact;
        let id = ServiceId::Aci(uuid.into());
        let title = if name.trim().is_empty() {
            phone_number
                .as_ref()
                .map(ToString::to_string)
                .unwrap_or_else(|| id.service_id_string())
        } else {
            name
        };
        result.push(Conversation {
            id: ConversationId::Contact(id),
            title,
            subtitle: phone_number
                .map(|n| n.to_string())
                .unwrap_or_else(|| "Signal contact".into()),
            group_revision: None,
        });
    }

    for group in manager.store().groups().await?.flatten() {
        let (
            key,
            Group {
                title,
                members,
                revision,
                ..
            },
        ) = group;
        result.push(Conversation {
            id: ConversationId::Group(key),
            title: if title.is_empty() {
                "Unnamed group".into()
            } else {
                title
            },
            subtitle: format!("{} members", members.len()),
            group_revision: Some(revision),
        });
    }

    result.sort_by(|a, b| a.title.to_lowercase().cmp(&b.title.to_lowercase()));
    Ok(result)
}

pub async fn history(
    manager: &Manager<SqliteStore, Registered>,
    conversation: &Conversation,
) -> Result<Vec<ChatMessage>> {
    let mut messages = Vec::new();
    let iter = manager
        .store()
        .messages(&conversation.id.thread(), ..)
        .await?;
    for content in iter.flatten().take(250) {
        if let Some(message) = display_message(&content, manager).await {
            messages.push(message);
        }
    }
    messages.reverse();
    Ok(messages)
}

async fn sender_name(
    content: &Content,
    manager: &Manager<SqliteStore, Registered>,
) -> Option<String> {
    manager
        .store()
        .contact_by_id(&content.metadata.sender)
        .await
        .ok()
        .flatten()
        .map(|contact| contact.name)
        .filter(|name| !name.is_empty())
}

pub async fn display_message(
    content: &Content,
    manager: &Manager<SqliteStore, Registered>,
) -> Option<ChatMessage> {
    use presage::libsignal_service::proto::{sync_message::Sent, SyncMessage};

    let (mine, data) = match &content.body {
        ContentBody::DataMessage(data) => (false, data),
        ContentBody::SynchronizeMessage(SyncMessage {
            sent: Some(Sent {
                message: Some(data),
                ..
            }),
            ..
        }) => (true, data),
        _ => return None,
    };
    let body = data.body.clone()?;
    Some(ChatMessage {
        timestamp: content.timestamp(),
        mine,
        sender: if mine {
            None
        } else {
            sender_name(content, manager).await
        },
        body,
    })
}

pub async fn send(
    manager: &mut Manager<SqliteStore, Registered>,
    conversation: &Conversation,
    text: String,
) -> Result<ChatMessage> {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before Unix epoch")?
        .as_millis() as u64;

    let mut message = DataMessage {
        body: Some(text.clone()),
        timestamp: Some(timestamp),
        ..Default::default()
    };

    match conversation.id {
        ConversationId::Contact(id) => manager.send_message(id, message, timestamp).await?,
        ConversationId::Group(key) => {
            message.group_v2 = Some(GroupContextV2 {
                master_key: Some(key.to_vec()),
                revision: conversation.group_revision,
                ..Default::default()
            });
            manager
                .send_message_to_group(&key, message, timestamp)
                .await?;
        }
    }

    Ok(ChatMessage {
        timestamp,
        mine: true,
        sender: None,
        body: text,
    })
}

pub fn start_receiver(
    mut manager: Manager<SqliteStore, Registered>,
    tx: mpsc::UnboundedSender<NetworkEvent>,
) -> tokio::task::JoinHandle<()> {
    tokio::task::spawn_local(async move {
        let stream = match manager.receive_messages().await {
            Ok(stream) => stream,
            Err(error) => {
                let _ = tx.send(NetworkEvent::Error(error.to_string()));
                return;
            }
        };
        futures::pin_mut!(stream);
        while let Some(received) = stream.next().await {
            let event = match received {
                Received::Content(content) => Thread::try_from(content.as_ref())
                    .map(|thread| match thread {
                        Thread::Contact(id) => NetworkEvent::Message(ConversationId::Contact(id)),
                        Thread::Group(key) => NetworkEvent::Message(ConversationId::Group(key)),
                    })
                    .unwrap_or_else(|error| NetworkEvent::Error(error.to_string())),
                Received::Contacts => NetworkEvent::ConversationsChanged,
                Received::QueueEmpty => NetworkEvent::QueueEmpty,
            };
            if tx.send(event).is_err() {
                break;
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::transparent_qr;

    #[test]
    fn qr_uses_terminal_background_without_ansi_colors() {
        let rendered = transparent_qr("sgnl://linkdevice?uuid=test&pub_key=test").unwrap();
        assert!(rendered.contains("██"));
        assert!(!rendered.contains("\u{1b}["));
    }
}
