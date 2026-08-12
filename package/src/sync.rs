use std::{
    fs::OpenOptions,
    io::Write,
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use futures::StreamExt;
use libsignal_message_backup::{
    backup::{serialize::Backup as SerializableBackup, Purpose},
    frame::FileReaderFactory,
    key::MessageBackupKey,
    BackupReader,
};
use presage::{
    libsignal_service::{
        content::{Content, ContentBody, DataMessage, GroupContextV2, Metadata},
        groups_v2::Role,
        prelude::ProfileKey,
        proto::{attachment_pointer, sync_message, AttachmentPointer, SyncMessage},
        protocol::{Aci, ServiceId},
    },
    manager::Registered,
    model::{
        contacts::Contact,
        groups::{Group, Member},
    },
    store::{ContentsStore, StateStore, Store, Thread},
    Manager,
};
use presage_store_sqlite::SqliteStore;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use crate::backend::{self, ConversationId, NetworkEvent};

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LinkSyncState {
    pub ephemeral_backup_key: Option<String>,
    pub account_aci: Option<String>,
    pub device_id: Option<u32>,
    pub password: Option<String>,
    pub cdn: Option<u32>,
    pub archive_key: Option<String>,
    pub downloaded_bytes: u64,
    pub download_complete: bool,
    pub imported_messages: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SyncProgress {
    WaitingForPhone,
    Downloading {
        downloaded_bytes: u64,
        total_bytes: Option<u64>,
    },
    Validating,
    Importing {
        imported_messages: u64,
    },
    RefreshingPending,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SyncReport {
    pub imported_messages: u64,
    pub received_messages: u64,
    pub incoming_conversations: Vec<ConversationId>,
    pub contacts_updated: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransferOutcome {
    NoTransfer,
    Ready,
}

#[derive(Deserialize)]
struct TransferArchive {
    cdn: u32,
    key: String,
}

#[derive(Clone, Debug)]
pub struct LinkSyncPaths {
    pub state: PathBuf,
    pub archive: PathBuf,
}

impl LinkSyncPaths {
    pub fn for_database(database: &Path) -> Self {
        Self {
            state: with_suffix(database, ".sync.json"),
            archive: with_suffix(database, ".transfer.partial"),
        }
    }

    pub fn cleanup(&self) -> Result<()> {
        remove_if_present(&self.state)?;
        remove_if_present(&with_suffix(&self.state, ".tmp"))?;
        remove_if_present(&self.archive)?;
        Ok(())
    }
}

fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut value = path.as_os_str().to_os_string();
    value.push(suffix);
    PathBuf::from(value)
}

fn remove_if_present(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("remove {}", path.display())),
    }
}

pub fn save_link_state(path: &Path, state: &LinkSyncState) -> Result<()> {
    let temporary = with_suffix(path, ".tmp");
    let encoded = serde_json::to_vec(state).context("encode link synchronization state")?;
    let mut options = OpenOptions::new();
    options.create(true).truncate(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(&temporary)
        .with_context(|| format!("create {}", temporary.display()))?;
    file.write_all(&encoded)
        .with_context(|| format!("write {}", temporary.display()))?;
    file.sync_all()
        .with_context(|| format!("flush {}", temporary.display()))?;
    std::fs::rename(&temporary, path).with_context(|| format!("replace {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

pub fn load_link_state(path: &Path) -> Result<Option<LinkSyncState>> {
    let encoded = match std::fs::read(path) {
        Ok(encoded) => encoded,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).with_context(|| format!("read {}", path.display())),
    };
    serde_json::from_slice(&encoded)
        .context("decode link synchronization state")
        .map(Some)
}

pub async fn download_link_history<F>(
    manager: &Manager<SqliteStore, Registered>,
    paths: &LinkSyncPaths,
    mut progress: F,
) -> Result<TransferOutcome>
where
    F: FnMut(SyncProgress),
{
    let Some(mut state) = load_link_state(&paths.state)? else {
        return Ok(TransferOutcome::NoTransfer);
    };
    if state.ephemeral_backup_key.is_none() {
        paths.cleanup()?;
        return Ok(TransferOutcome::NoTransfer);
    }
    if paths.archive.exists() && state.download_complete {
        return Ok(TransferOutcome::Ready);
    }
    let aci = state
        .account_aci
        .as_deref()
        .context("link-sync state is missing the account ACI")?;
    let device_id = state
        .device_id
        .context("link-sync state is missing the linked device ID")?;
    let password = state
        .password
        .as_deref()
        .context("link-sync state is missing transfer authentication")?;
    let username = format!("{aci}.{device_id}");
    let client = reqwest::Client::builder()
        .user_agent("signal-tui")
        .build()
        .context("create history-transfer client")?;

    let transfer = if let (Some(cdn), Some(key)) = (state.cdn, state.archive_key.clone()) {
        TransferArchive { cdn, key }
    } else {
        progress(SyncProgress::WaitingForPhone);
        let poll_started = tokio::time::Instant::now();
        loop {
            if poll_started.elapsed() >= Duration::from_secs(5 * 60) {
                bail!("timed out waiting for the primary device to prepare history; the transfer will resume next launch");
            }
            let response = client
                .get("https://chat.signal.org/v1/devices/transfer_archive?timeout=10")
                .basic_auth(&username, Some(password))
                .send()
                .await
                .context("poll link-time history transfer")?;
            if response.status() == reqwest::StatusCode::NO_CONTENT {
                tokio::time::sleep(Duration::from_secs(2)).await;
                continue;
            }
            if response.status().is_success() {
                break response
                    .json::<TransferArchive>()
                    .await
                    .context("decode history-transfer location")?;
            }
            let status = response.status();
            let error = response.text().await.unwrap_or_default();
            if error.contains("CONTINUE_WITHOUT_UPLOAD") {
                paths.cleanup()?;
                return Ok(TransferOutcome::NoTransfer);
            }
            if error.contains("RELINK_REQUESTED") {
                rollback_incomplete_link(manager, paths).await?;
                bail!("Signal requested relinking; scan a new linked-device QR code");
            }
            if matches!(
                status,
                reqwest::StatusCode::UNAUTHORIZED | reqwest::StatusCode::FORBIDDEN
            ) {
                rollback_incomplete_link(manager, paths).await?;
                bail!("Signal rejected history-transfer authentication; relink this device");
            }
            bail!("history-transfer poll failed with HTTP {status}: {error}");
        }
    };

    state.cdn = Some(transfer.cdn);
    state.archive_key = Some(transfer.key.clone());
    save_link_state(&paths.state, &state)?;
    let base = match transfer.cdn {
        0 => "https://cdn.signal.org",
        2 => "https://cdn2.signal.org",
        3 => "https://cdn3.signal.org",
        cdn => bail!("unsupported Signal history-transfer CDN {cdn}"),
    };
    let url = format!(
        "{base}/attachments/{}",
        transfer.key.trim_start_matches('/')
    );
    let mut downloaded = std::fs::metadata(&paths.archive)
        .map(|metadata| metadata.len())
        .unwrap_or_default();
    let mut request = client.get(url);
    if downloaded > 0 {
        request = request.header(reqwest::header::RANGE, format!("bytes={downloaded}-"));
    }
    let response = request.send().await.context("download history archive")?;
    if response.status() == reqwest::StatusCode::RANGE_NOT_SATISFIABLE {
        state.download_complete = true;
        save_link_state(&paths.state, &state)?;
        return Ok(TransferOutcome::Ready);
    }
    let append = response.status() == reqwest::StatusCode::PARTIAL_CONTENT && downloaded > 0;
    if !response.status().is_success() {
        bail!(
            "history archive download failed with HTTP {}",
            response.status()
        );
    }
    if !append {
        downloaded = 0;
    }
    let total = response
        .content_length()
        .map(|remaining| remaining + downloaded);
    let mut options = OpenOptions::new();
    options
        .create(true)
        .write(true)
        .append(append)
        .truncate(!append);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(&paths.archive)
        .with_context(|| format!("open {}", paths.archive.display()))?;
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.context("read history archive download")?;
        file.write_all(&chunk)
            .context("write partial history archive")?;
        downloaded += chunk.len() as u64;
        state.downloaded_bytes = downloaded;
        save_link_state(&paths.state, &state)?;
        progress(SyncProgress::Downloading {
            downloaded_bytes: downloaded,
            total_bytes: total,
        });
    }
    file.sync_all().context("flush history archive")?;
    state.download_complete = true;
    save_link_state(&paths.state, &state)?;
    Ok(TransferOutcome::Ready)
}

/// Imports an archive captured during initial device linking.
///
/// Archive support is deliberately gated on a captured ephemeral key. A normal
/// linked account has no way to request that one-time key again.
pub async fn import_link_history<F>(
    manager: &Manager<SqliteStore, Registered>,
    paths: &LinkSyncPaths,
    mut progress: F,
) -> Result<SyncReport>
where
    F: FnMut(SyncProgress),
{
    let Some(mut state) = load_link_state(&paths.state)? else {
        return Ok(SyncReport::default());
    };
    let (Some(ephemeral_key), Some(account_aci)) = (
        state.ephemeral_backup_key.as_deref(),
        state.account_aci.as_deref(),
    ) else {
        return Ok(SyncReport::default());
    };
    if !paths.archive.exists() {
        progress(SyncProgress::WaitingForPhone);
        bail!("waiting for the primary Signal device to provide the history archive");
    }
    let archive_size = std::fs::metadata(&paths.archive)?.len();
    progress(SyncProgress::Downloading {
        downloaded_bytes: archive_size,
        total_bytes: Some(archive_size),
    });
    let ephemeral_key: [u8; 32] = hex::decode(ephemeral_key)
        .context("decode ephemeral backup key")?
        .try_into()
        .map_err(|value: Vec<u8>| {
            anyhow::anyhow!("ephemeral backup key is {} bytes, expected 32", value.len())
        })?;
    let aci = Aci::parse_from_service_id_string(account_aci)
        .context("parse account ACI from link state")?;
    let backup_key: presage::libsignal_service::libsignal_account_keys::BackupKey =
        presage::libsignal_service::libsignal_account_keys::BackupKey(ephemeral_key);
    let backup_id = backup_key.derive_backup_id(&aci);
    let message_backup_key = MessageBackupKey::derive(&backup_key, &backup_id, None);

    save_link_state(&paths.state, &state)?;
    progress(SyncProgress::Validating);
    let reader = match BackupReader::new_encrypted_compressed(
        &message_backup_key,
        FileReaderFactory {
            path: &paths.archive,
        },
        Purpose::DeviceTransfer,
    )
    .await
    {
        Ok(reader) => reader,
        Err(error) => {
            rollback_incomplete_link(manager, paths).await?;
            return Err(error).context("authenticate and open the link-time history archive");
        }
    };
    let completed = match reader.read_all().await.result {
        Ok(completed) => completed,
        Err(error) => {
            rollback_incomplete_link(manager, paths).await?;
            return Err(error).context("validate the link-time history archive");
        }
    };
    let canonical = SerializableBackup::from(completed).to_string_pretty();
    let canonical: serde_json::Value =
        serde_json::from_str(&canonical).context("decode validated history frames")?;
    let mut store = manager.store().clone();
    restore_recipients(&mut store, &canonical).await?;
    let mut imported_messages = 0;
    for chat in canonical
        .get("chats")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
    {
        let Some(thread) = chat.get("recipient").and_then(recipient_thread) else {
            continue;
        };
        for item in chat
            .get("items")
            .and_then(serde_json::Value::as_array)
            .into_iter()
            .flatten()
        {
            if let Some(content) = archive_content(item, &thread, aci)? {
                store.save_message(&thread, content).await?;
                imported_messages += 1;
            }
            for revision in item
                .get("revisions")
                .and_then(serde_json::Value::as_array)
                .into_iter()
                .flatten()
            {
                if let Some(content) = archive_content(revision, &thread, aci)? {
                    store.save_message(&thread, content).await?;
                    imported_messages += 1;
                }
            }
            if imported_messages > 0 && imported_messages % 100 == 0 {
                progress(SyncProgress::Importing { imported_messages });
                state.imported_messages = imported_messages;
                save_link_state(&paths.state, &state)?;
            }
        }
    }
    progress(SyncProgress::Importing { imported_messages });
    paths.cleanup()?;
    Ok(SyncReport {
        imported_messages,
        ..Default::default()
    })
}

async fn rollback_incomplete_link(
    manager: &Manager<SqliteStore, Registered>,
    paths: &LinkSyncPaths,
) -> Result<()> {
    let mut store = manager.store().clone();
    StateStore::clear_registration(&mut store).await?;
    ContentsStore::clear_contents(&mut store).await?;
    Store::clear(&mut store).await?;
    paths.cleanup()?;
    Ok(())
}

async fn restore_recipients(store: &mut SqliteStore, backup: &serde_json::Value) -> Result<()> {
    for recipient in backup
        .get("recipients")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
    {
        if let Some(contact) = recipient.get("Contact") {
            let Some(aci) = contact
                .get("aci")
                .and_then(serde_json::Value::as_str)
                .and_then(Aci::parse_from_service_id_string)
            else {
                continue;
            };
            let profile_key = contact
                .get("profile_key")
                .and_then(serde_json::Value::as_str)
                .and_then(|value| hex::decode(value).ok())
                .unwrap_or_default();
            store
                .save_contact(&Contact {
                    uuid: aci.into(),
                    phone_number: None,
                    name: contact_name(contact),
                    verified: Default::default(),
                    profile_key,
                    expire_timer: 0,
                    expire_timer_version: 2,
                    inbox_position: 0,
                    avatar: None,
                })
                .await?;
        } else if let Some(group) = recipient.get("Group") {
            let Some(master_key) = group
                .get("master_key")
                .and_then(serde_json::Value::as_str)
                .and_then(|value| hex::decode(value).ok())
                .and_then(|value| value.try_into().ok())
            else {
                continue;
            };
            let snapshot = group.get("snapshot").unwrap_or(group);
            let title = snapshot
                .get("title")
                .and_then(serde_json::Value::as_str)
                .filter(|title| !title.trim().is_empty())
                .unwrap_or("Unnamed group")
                .to_owned();
            let revision = snapshot
                .get("version")
                .and_then(serde_json::Value::as_u64)
                .and_then(|value| u32::try_from(value).ok())
                .unwrap_or_default();
            let members = snapshot
                .get("members")
                .and_then(serde_json::Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|member| {
                    let aci = member
                        .get("user_id")?
                        .as_str()
                        .and_then(Aci::parse_from_service_id_string)?;
                    Some(Member {
                        aci,
                        role: if member.get("role").and_then(serde_json::Value::as_str)
                            == Some("Administrator")
                        {
                            Role::Administrator
                        } else {
                            Role::Default
                        },
                        profile_key: ProfileKey::create([0; 32]),
                        joined_at_revision: member
                            .get("joined_at_version")
                            .and_then(serde_json::Value::as_u64)
                            .and_then(|value| u32::try_from(value).ok())
                            .unwrap_or_default(),
                        label: member
                            .get("label_string")
                            .and_then(serde_json::Value::as_str)
                            .map(str::to_owned),
                        label_emoji: member
                            .get("label_emoji")
                            .and_then(serde_json::Value::as_str)
                            .map(str::to_owned),
                    })
                })
                .collect();
            store
                .save_group(
                    master_key,
                    Group {
                        title,
                        avatar: String::new(),
                        disappearing_messages_timer: None,
                        access_control: None,
                        revision,
                        members,
                        pending_members: Vec::new(),
                        requesting_members: Vec::new(),
                        invite_link_password: Vec::new(),
                        description: snapshot
                            .get("description")
                            .and_then(serde_json::Value::as_str)
                            .map(str::to_owned),
                    },
                )
                .await?;
        }
    }
    Ok(())
}

fn contact_name(contact: &serde_json::Value) -> String {
    let nickname = contact.get("nickname");
    let profile_name = ["profile_given_name", "profile_family_name"]
        .into_iter()
        .filter_map(|field| contact.get(field).and_then(serde_json::Value::as_str))
        .filter(|part| !part.trim().is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    if !profile_name.is_empty() {
        return profile_name;
    }
    if let Some(nickname) = nickname {
        let name = ["given_name", "family_name"]
            .into_iter()
            .filter_map(|field| nickname.get(field).and_then(serde_json::Value::as_str))
            .filter(|part| !part.trim().is_empty())
            .collect::<Vec<_>>()
            .join(" ");
        if !name.is_empty() {
            return name;
        }
    }
    let system_name = ["system_given_name", "system_family_name"]
        .into_iter()
        .filter_map(|field| contact.get(field).and_then(serde_json::Value::as_str))
        .filter(|part| !part.trim().is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    if !system_name.is_empty() {
        return system_name;
    }
    contact
        .get("e164")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("Unknown contact")
        .to_owned()
}

fn recipient_thread(recipient: &serde_json::Value) -> Option<Thread> {
    if let Some(contact) = recipient.get("Contact") {
        return contact
            .get("aci")
            .and_then(serde_json::Value::as_str)
            .and_then(Aci::parse_from_service_id_string)
            .map(ServiceId::Aci)
            .map(Thread::Contact);
    }
    recipient
        .get("Group")?
        .get("master_key")?
        .as_str()
        .and_then(|value| hex::decode(value).ok())?
        .try_into()
        .ok()
        .map(Thread::Group)
}

fn recipient_aci(recipient: &serde_json::Value) -> Option<Aci> {
    recipient
        .get("Contact")?
        .get("aci")?
        .as_str()
        .and_then(Aci::parse_from_service_id_string)
}

fn archive_content(
    item: &serde_json::Value,
    thread: &Thread,
    own_aci: Aci,
) -> Result<Option<Content>> {
    let Some(timestamp) = item.get("sent_at").and_then(serde_json::Value::as_u64) else {
        return Ok(None);
    };
    let Some((body_text, attachments)) = archive_message(item.get("message")) else {
        return Ok(None);
    };
    let mine = item
        .get("direction")
        .and_then(|direction| direction.get("Outgoing"))
        .is_some();
    let sender = if mine {
        ServiceId::Aci(own_aci)
    } else {
        item.get("author")
            .and_then(recipient_aci)
            .map(ServiceId::Aci)
            .or(match thread {
                Thread::Contact(id) => Some(*id),
                Thread::Group(_) => None,
            })
            .unwrap_or(ServiceId::Aci(own_aci))
    };
    let destination = if mine {
        match thread {
            Thread::Contact(id) => *id,
            Thread::Group(_) => ServiceId::Aci(own_aci),
        }
    } else {
        ServiceId::Aci(own_aci)
    };
    let datetime = DateTime::<Utc>::from_timestamp_millis(timestamp as i64)
        .context("archive message timestamp is outside the supported range")?;
    let mut data = DataMessage {
        body: Some(body_text),
        timestamp: Some(timestamp),
        attachments,
        ..Default::default()
    };
    if let Thread::Group(master_key) = thread {
        data.group_v2 = Some(GroupContextV2 {
            master_key: Some(master_key.to_vec()),
            ..Default::default()
        });
    }
    let body = if mine {
        ContentBody::SynchronizeMessage(SyncMessage {
            sent: Some(sync_message::Sent {
                destination_service_id: match thread {
                    Thread::Contact(id) => Some(id.service_id_string()),
                    Thread::Group(_) => None,
                },
                timestamp: Some(timestamp),
                message: Some(data),
                ..Default::default()
            }),
            ..Default::default()
        })
    } else {
        ContentBody::DataMessage(data)
    };
    Ok(Some(Content {
        metadata: Metadata {
            sender,
            destination,
            sender_device: 1u8.try_into().expect("device ID 1 is valid"),
            timestamp: datetime,
            server_timestamp: datetime,
            needs_receipt: false,
            unidentified_sender: false,
            was_plaintext: false,
            server_guid: None,
        },
        body,
    }))
}

fn archive_message(
    message: Option<&serde_json::Value>,
) -> Option<(String, Vec<AttachmentPointer>)> {
    let message = message?;
    if let Some(standard) = message.get("Standard") {
        let attachments = standard
            .get("attachments")
            .and_then(serde_json::Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(archive_attachment)
            .collect::<Vec<_>>();
        if let Some(text) = standard
            .get("text")
            .and_then(|text| text.get("text"))
            .and_then(serde_json::Value::as_str)
        {
            return Some((text.to_owned(), attachments));
        }
        if !attachments.is_empty() {
            return Some((String::new(), attachments));
        }
    }
    [
        ("Contact", "[shared contact]"),
        ("Voice", "[voice message]"),
        ("Sticker", "[sticker]"),
        ("RemoteDeleted", "[message deleted]"),
        ("Update", "[chat update]"),
        ("PaymentNotification", "[payment]"),
        ("GiftBadge", "[gift badge]"),
        ("ViewOnce", "[view-once message]"),
        ("DirectStoryReply", "[story reply]"),
        ("Poll", "[poll]"),
        ("AdminDeleted", "[message deleted by admin]"),
    ]
    .into_iter()
    .find(|(kind, _)| message.get(*kind).is_some())
    .map(|(_, label)| (label.to_owned(), Vec::new()))
}

fn archive_attachment(value: &serde_json::Value) -> Option<AttachmentPointer> {
    let pointer = value.get("pointer")?;
    let locator = pointer.get("locator_info")?.get("LocatorInfo")?;
    let key = locator
        .get("key")?
        .as_str()
        .and_then(|value| hex::decode(value).ok());
    let digest = locator
        .get("integrity_check")?
        .get("EncryptedDigest")?
        .get("digest")?
        .as_str()
        .and_then(|value| hex::decode(value).ok());
    let transit = locator.get("transit")?;
    let cdn_key = transit.get("cdn_key")?.as_str()?.to_owned();
    let cdn_number = transit
        .get("cdn_number")?
        .as_u64()
        .and_then(|value| u32::try_from(value).ok());
    Some(AttachmentPointer {
        content_type: pointer
            .get("content_type")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned),
        key,
        size: locator
            .get("plaintext_size")
            .and_then(serde_json::Value::as_u64)
            .and_then(|value| u32::try_from(value).ok()),
        digest,
        file_name: pointer
            .get("file_name")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned),
        width: pointer
            .get("width")
            .and_then(serde_json::Value::as_u64)
            .and_then(|value| u32::try_from(value).ok()),
        height: pointer
            .get("height")
            .and_then(serde_json::Value::as_u64)
            .and_then(|value| u32::try_from(value).ok()),
        cdn_number,
        attachment_identifier: Some(attachment_pointer::AttachmentIdentifier::CdnKey(cdn_key)),
        ..Default::default()
    })
}

pub async fn refresh_pending<F>(
    manager: &Manager<SqliteStore, Registered>,
    timeout: Duration,
    mut progress: F,
) -> Result<SyncReport>
where
    F: FnMut(SyncProgress),
{
    progress(SyncProgress::RefreshingPending);
    let mut manager = manager.clone();
    manager
        .request_contacts()
        .await
        .context("request contacts from the primary Signal device")?;

    let (tx, mut rx) = mpsc::unbounded_channel();
    let receiver = backend::start_receiver(manager, tx);
    let result = tokio::time::timeout(timeout, async {
        let mut report = SyncReport::default();
        let mut queue_empty = false;
        while let Some(event) = rx.recv().await {
            match event {
                NetworkEvent::Message {
                    conversation,
                    incoming,
                } => {
                    report.received_messages += 1;
                    if incoming {
                        report.incoming_conversations.push(conversation);
                    }
                }
                NetworkEvent::ConversationsChanged => report.contacts_updated = true,
                NetworkEvent::QueueEmpty => queue_empty = true,
                NetworkEvent::Error(error) => bail!(error),
            }
            if queue_empty && report.contacts_updated {
                return Ok(report);
            }
        }
        bail!("Signal receive stream ended before synchronization completed")
    })
    .await;
    receiver.abort();
    let _ = receiver.await;
    result.context("timed out waiting for queued Signal messages")?
}

#[cfg(test)]
mod tests {
    use super::{
        archive_message, contact_name, load_link_state, recipient_thread, save_link_state,
        LinkSyncPaths, LinkSyncState,
    };
    use presage::store::Thread;
    use serde_json::json;

    #[test]
    fn state_round_trips_and_cleanup_removes_every_sidecar() {
        let directory = tempfile::tempdir().unwrap();
        let paths = LinkSyncPaths::for_database(&directory.path().join("signal.db"));
        let state = LinkSyncState {
            ephemeral_backup_key: Some("secret".into()),
            downloaded_bytes: 42,
            ..Default::default()
        };
        save_link_state(&paths.state, &state).unwrap();
        std::fs::write(&paths.archive, b"partial").unwrap();
        assert_eq!(load_link_state(&paths.state).unwrap(), Some(state));

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&paths.state)
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }

        paths.cleanup().unwrap();
        assert!(!paths.state.exists());
        assert!(!paths.archive.exists());
    }

    #[test]
    fn canonical_recipients_use_human_names_and_stable_threads() {
        let contact = json!({
            "profile_given_name": "Vera",
            "profile_family_name": "Scheel",
            "e164": "+49123"
        });
        assert_eq!(contact_name(&contact), "Vera Scheel");
        assert_eq!(contact_name(&json!({"e164": "+49123"})), "+49123");
        assert_eq!(contact_name(&json!({})), "Unknown contact");

        let group = json!({"Group": {"master_key": "2a".repeat(32)}});
        assert_eq!(recipient_thread(&group), Some(Thread::Group([42; 32])));
    }

    #[test]
    fn canonical_messages_keep_text_and_use_existing_placeholders() {
        assert_eq!(
            archive_message(Some(&json!({
                "Standard": {"text": {"text": "hello"}, "attachments": []}
            }))),
            Some(("hello".into(), Vec::new()))
        );
        assert_eq!(
            archive_message(Some(&json!({"RemoteDeleted": null}))),
            Some(("[message deleted]".into(), Vec::new()))
        );
        assert_eq!(
            archive_message(Some(&json!({"Poll": {}}))),
            Some(("[poll]".into(), Vec::new()))
        );
    }
}
