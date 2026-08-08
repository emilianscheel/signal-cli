use std::{
    ops::Range,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{bail, Context, Result};
use futures::{channel::oneshot, future, StreamExt};
use presage::{
    libsignal_service::{
        configuration::SignalServers,
        content::{Content, ContentBody, DataMessage, GroupContextV2},
        proto::AttachmentPointer,
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

    pub fn stable_id(&self) -> String {
        match self {
            Self::Contact(id) => format!("contact:{}", id.service_id_string()),
            Self::Group(key) => format!("group:{}", hex::encode(key)),
        }
    }

    pub const fn kind(&self) -> &'static str {
        match self {
            Self::Contact(_) => "contact",
            Self::Group(_) => "group",
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

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageKind {
    Text,
    Attachment,
    Sticker,
    Reaction,
    Deleted,
    Edited,
    Poll,
    Payment,
    Contact,
    Call,
    Story,
    NonText,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ChatAttachment {
    pub key: String,
    pub file_name: Option<String>,
    pub content_type: Option<String>,
    pub size: Option<u32>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub pointer: AttachmentPointer,
}

impl ChatAttachment {
    pub fn display_name(&self) -> &str {
        self.file_name.as_deref().unwrap_or("attachment")
    }

    pub fn is_supported_image(&self) -> bool {
        matches!(
            self.content_type
                .as_deref()
                .and_then(|value| value.split(';').next())
                .map(str::trim),
            Some("image/png" | "image/jpeg" | "image/jpg" | "image/webp" | "image/gif")
        )
    }

    pub fn can_preview(&self) -> bool {
        const MAX_BYTES: u32 = 50 * 1024 * 1024;
        const MAX_PIXELS: u64 = 40_000_000;
        self.is_supported_image()
            && self.pointer.digest.is_some()
            && self.size.is_none_or(|size| size <= MAX_BYTES)
            && match (self.width, self.height) {
                (Some(width), Some(height)) => u64::from(width) * u64::from(height) <= MAX_PIXELS,
                _ => true,
            }
    }
}

fn chat_attachments(timestamp: u64, pointers: &[AttachmentPointer]) -> Vec<ChatAttachment> {
    pointers
        .iter()
        .enumerate()
        .map(|(index, pointer)| ChatAttachment {
            key: pointer
                .digest
                .as_deref()
                .map(hex::encode)
                .or_else(|| pointer.client_uuid.as_deref().map(hex::encode))
                .unwrap_or_else(|| format!("missing-{timestamp}-{index}")),
            file_name: pointer
                .file_name
                .clone()
                .filter(|name| !name.trim().is_empty()),
            content_type: pointer
                .content_type
                .clone()
                .filter(|value| !value.trim().is_empty()),
            size: pointer.size,
            width: pointer.width,
            height: pointer.height,
            pointer: pointer.clone(),
        })
        .collect()
}

#[derive(Clone, Debug, PartialEq)]
pub struct ChatMessage {
    pub timestamp: u64,
    pub mine: bool,
    pub sender: Option<String>,
    pub body: String,
    pub kind: MessageKind,
    pub attachments: Vec<ChatAttachment>,
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
    stderr: bool,
) -> Result<Manager<SqliteStore, Registered>> {
    if stderr {
        eprintln!("Welcome to Signal CLI\n");
        eprintln!("On your iPhone, open Signal → Settings → Linked Devices → Link New Device.\n");
    } else {
        println!("Welcome to Signal CLI\n");
        println!("On your iPhone, open Signal → Settings → Linked Devices → Link New Device.\n");
    }

    let (tx, rx) = oneshot::channel();
    let (manager, shown) = future::join(
        Manager::link_secondary_device(store, SignalServers::Production, device_name, tx),
        async move {
            let url = rx.await.context("Signal provisioning was cancelled")?;
            if stderr {
                eprint!("{}", transparent_qr(url.as_str())?);
            } else {
                print!("{}", transparent_qr(url.as_str())?);
            }
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
    let mut messages = latest_messages(manager, conversation, None, None, 250).await?;
    messages.reverse();
    Ok(messages)
}

pub async fn latest_messages(
    manager: &Manager<SqliteStore, Registered>,
    conversation: &Conversation,
    since: Option<u64>,
    until: Option<u64>,
    limit: usize,
) -> Result<Vec<ChatMessage>> {
    let thread = conversation.id.thread();
    let iter = match (since, until) {
        (Some(start), Some(end)) => {
            manager
                .store()
                .messages(&thread, Range { start, end })
                .await?
        }
        (Some(start), None) => manager.store().messages(&thread, start..).await?,
        (None, Some(end)) => manager.store().messages(&thread, ..end).await?,
        (None, None) => manager.store().messages(&thread, ..).await?,
    };

    let mut messages = Vec::new();
    for content in iter {
        let content = content.context("read a stored message")?;
        if let Some(message) = display_message(&content, manager).await {
            messages.push(message);
            if messages.len() == limit {
                break;
            }
        }
    }
    Ok(messages)
}

pub fn matching_conversations<'a>(
    conversations: &'a [Conversation],
    query: &str,
) -> Vec<&'a Conversation> {
    if let Some(exact) = conversations
        .iter()
        .find(|conversation| conversation.id.stable_id() == query)
    {
        return vec![exact];
    }

    let query = query.to_lowercase();
    conversations
        .iter()
        .filter(|conversation| conversation.title.to_lowercase().contains(&query))
        .collect()
}

async fn sender_name(
    content: &Content,
    manager: &Manager<SqliteStore, Registered>,
) -> Option<String> {
    let name = manager
        .store()
        .contact_by_id(&content.metadata.sender)
        .await
        .ok()
        .flatten()
        .map(|contact| contact.name)
        .filter(|name| !name.is_empty());
    name.or_else(|| Some(content.metadata.sender.service_id_string()))
}

fn data_message_body(
    data: &DataMessage,
    edited: bool,
) -> Option<(String, MessageKind, Vec<AttachmentPointer>)> {
    let attachment_count = data.attachments.len();
    if let Some(body) = data.body.as_ref().filter(|body| !body.is_empty()) {
        return Some((
            body.clone(),
            if edited {
                MessageKind::Edited
            } else {
                MessageKind::Text
            },
            data.attachments.clone(),
        ));
    }

    let (body, kind) = if let Some(reaction) = &data.reaction {
        let emoji = reaction.emoji.as_deref().unwrap_or("?");
        let action = if reaction.remove.unwrap_or(false) {
            "removed reaction"
        } else {
            "reaction"
        };
        (format!("[{action} {emoji}]"), MessageKind::Reaction)
    } else if data.delete.is_some() || data.admin_delete.is_some() {
        ("[message deleted]".into(), MessageKind::Deleted)
    } else if let Some(sticker) = &data.sticker {
        let suffix = sticker.emoji.as_deref().unwrap_or("");
        (format!("[sticker{suffix}]"), MessageKind::Sticker)
    } else if attachment_count > 0 {
        (String::new(), MessageKind::Attachment)
    } else if let Some(poll) = &data.poll_create {
        let question = poll.question.as_deref().unwrap_or("poll");
        (format!("[poll: {question}]"), MessageKind::Poll)
    } else if data.poll_vote.is_some() || data.poll_terminate.is_some() {
        ("[poll update]".into(), MessageKind::Poll)
    } else if data.payment.is_some() {
        ("[payment]".into(), MessageKind::Payment)
    } else if !data.contact.is_empty() {
        ("[shared contact]".into(), MessageKind::Contact)
    } else if data.group_call_update.is_some() {
        ("[group call update]".into(), MessageKind::Call)
    } else if data.gift_badge.is_some() {
        ("[gift badge]".into(), MessageKind::NonText)
    } else if data.pin_message.is_some() || data.unpin_message.is_some() {
        ("[pinned-message update]".into(), MessageKind::NonText)
    } else {
        return None;
    };
    Some((
        if edited {
            "[edited message]".into()
        } else {
            body
        },
        if edited { MessageKind::Edited } else { kind },
        data.attachments.clone(),
    ))
}

pub async fn display_message(
    content: &Content,
    manager: &Manager<SqliteStore, Registered>,
) -> Option<ChatMessage> {
    use presage::libsignal_service::proto::SyncMessage;

    let (mine, rendered) = match &content.body {
        ContentBody::DataMessage(data) => (false, data_message_body(data, false)),
        ContentBody::EditMessage(edit) => (
            false,
            edit.data_message
                .as_ref()
                .and_then(|data| data_message_body(data, true)),
        ),
        ContentBody::SynchronizeMessage(SyncMessage {
            sent: Some(sent), ..
        }) => {
            let rendered = if let Some(data) = &sent.message {
                data_message_body(data, false)
            } else if let Some(edit) = &sent.edit_message {
                edit.data_message
                    .as_ref()
                    .and_then(|data| data_message_body(data, true))
            } else if sent.story_message.is_some() {
                Some(("[story]".into(), MessageKind::Story, Vec::new()))
            } else {
                None
            };
            (true, rendered)
        }
        ContentBody::CallMessage(_) => (
            false,
            Some(("[call event]".into(), MessageKind::Call, Vec::new())),
        ),
        ContentBody::StoryMessage(_) => (
            false,
            Some(("[story]".into(), MessageKind::Story, Vec::new())),
        ),
        ContentBody::DecryptionErrorMessage(_) => (
            false,
            Some((
                "[message could not be decrypted]".into(),
                MessageKind::NonText,
                Vec::new(),
            )),
        ),
        ContentBody::NullMessage(_)
        | ContentBody::SynchronizeMessage(_)
        | ContentBody::ReceiptMessage(_)
        | ContentBody::TypingMessage(_)
        | ContentBody::PniSignatureMessage(_) => return None,
    };
    let (body, kind, attachment_pointers) = rendered?;
    let timestamp = content.timestamp();
    Some(ChatMessage {
        timestamp,
        mine,
        sender: if mine {
            None
        } else {
            sender_name(content, manager).await
        },
        body,
        kind,
        attachments: chat_attachments(timestamp, &attachment_pointers),
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
        kind: MessageKind::Text,
        attachments: Vec::new(),
    })
}

pub async fn sync_pending(
    manager: &Manager<SqliteStore, Registered>,
    timeout: Duration,
) -> Result<()> {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let receiver = start_receiver(manager.clone(), tx);
    let result = tokio::time::timeout(timeout, async {
        while let Some(event) = rx.recv().await {
            match event {
                NetworkEvent::QueueEmpty => return Ok(()),
                NetworkEvent::Error(error) => bail!(error),
                NetworkEvent::Message(_) | NetworkEvent::ConversationsChanged => {}
            }
        }
        bail!("Signal receive stream ended before synchronization completed")
    })
    .await;
    receiver.abort();
    let _ = receiver.await;
    result.context("timed out waiting for Signal synchronization")?
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
    use presage::libsignal_service::proto::{data_message, AttachmentPointer, DataMessage};

    use super::{
        chat_attachments, data_message_body, matching_conversations, transparent_qr, Conversation,
        ConversationId, MessageKind,
    };

    #[test]
    fn qr_uses_terminal_background_without_ansi_colors() {
        let rendered = transparent_qr("sgnl://linkdevice?uuid=test&pub_key=test").unwrap();
        assert!(rendered.contains("██"));
        assert!(!rendered.contains("\u{1b}["));
    }

    #[test]
    fn stable_group_id_is_hex_prefixed() {
        assert_eq!(
            ConversationId::Group([42; 32]).stable_id(),
            format!("group:{}", "2a".repeat(32))
        );
    }

    #[test]
    fn matching_prefers_an_exact_stable_id() {
        let conversations = vec![
            Conversation {
                id: ConversationId::Group([1; 32]),
                title: "Emilian".into(),
                subtitle: String::new(),
                group_revision: Some(1),
            },
            Conversation {
                id: ConversationId::Group([2; 32]),
                title: "Emilian family".into(),
                subtitle: String::new(),
                group_revision: Some(2),
            },
        ];
        assert_eq!(matching_conversations(&conversations, "emilian").len(), 2);
        assert_eq!(
            matching_conversations(&conversations, &conversations[0].id.stable_id()).len(),
            1
        );
    }

    #[test]
    fn text_preserves_attachment_metadata() {
        let data = DataMessage {
            body: Some("caption".into()),
            attachments: vec![AttachmentPointer::default()],
            ..Default::default()
        };
        assert_eq!(
            data_message_body(&data, false),
            Some((
                "caption".into(),
                MessageKind::Text,
                vec![AttachmentPointer::default()]
            ))
        );
    }

    #[test]
    fn renders_common_non_text_placeholders() {
        let reaction = DataMessage {
            reaction: Some(data_message::Reaction {
                emoji: Some("👍".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(
            data_message_body(&reaction, false),
            Some(("[reaction 👍]".into(), MessageKind::Reaction, Vec::new()))
        );

        let attachment = DataMessage {
            attachments: vec![AttachmentPointer::default(), AttachmentPointer::default()],
            ..Default::default()
        };
        assert_eq!(
            data_message_body(&attachment, false),
            Some((
                String::new(),
                MessageKind::Attachment,
                vec![AttachmentPointer::default(), AttachmentPointer::default()]
            ))
        );

        let poll = DataMessage {
            poll_create: Some(data_message::PollCreate {
                question: Some("Lunch?".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(
            data_message_body(&poll, false),
            Some(("[poll: Lunch?]".into(), MessageKind::Poll, Vec::new()))
        );
    }

    #[test]
    fn transport_only_data_is_omitted() {
        assert_eq!(data_message_body(&DataMessage::default(), false), None);
    }

    #[test]
    fn attachment_metadata_and_preview_limits_are_preserved() {
        let pointer = AttachmentPointer {
            digest: Some(vec![0xab; 32]),
            file_name: Some("photo.jpg".into()),
            content_type: Some("image/jpeg".into()),
            size: Some(2_000_000),
            width: Some(1_600),
            height: Some(1_200),
            ..Default::default()
        };
        let attachment = chat_attachments(42, &[pointer])[0].clone();
        assert_eq!(attachment.key, "ab".repeat(32));
        assert_eq!(attachment.display_name(), "photo.jpg");
        assert!(attachment.can_preview());

        let mut oversized = attachment;
        oversized.width = Some(10_000);
        oversized.height = Some(10_000);
        assert!(!oversized.can_preview());
    }
}
