use std::{
    collections::HashSet,
    io::{self, IsTerminal, Write},
    time::Duration,
};

use anyhow::{bail, Context, Result};
use chrono::{
    DateTime, Days, Duration as ChronoDuration, Local, LocalResult, NaiveDate, NaiveDateTime,
    SecondsFormat, TimeZone,
};
use clap::Subcommand;
use presage::{manager::Registered, Manager};
use presage_store_sqlite::SqliteStore;
use serde_json::{json, Value};
use unicode_width::UnicodeWidthStr;

use crate::{
    attachments::{self, AttachmentCache},
    backend::{self, AttachmentOccurrence, ChatAttachment, ChatMessage, Conversation},
};

const DEFAULT_LIMIT: usize = 15;
const SYNC_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, Subcommand)]
pub enum Command {
    /// List all synced contacts and groups.
    List,
    /// Read recent messages from one chat.
    Read {
        /// Stable chat ID or case-insensitive name fragment.
        chat: String,
        /// Inclusive lower timestamp bound.
        #[arg(long, value_name = "DATE")]
        since: Option<String>,
        /// Exclusive upper timestamp bound (date-only values include the whole day).
        #[arg(long, value_name = "DATE")]
        until: Option<String>,
        /// Maximum number of messages to return.
        #[arg(long, default_value_t = DEFAULT_LIMIT, value_parser = positive_usize)]
        limit: usize,
    },
    /// Send a text message to one chat.
    Send {
        /// Stable chat ID or case-insensitive name fragment.
        chat: String,
        /// Exact message text (quote it when it contains spaces).
        message: String,
    },
    /// Show the newest messages across all chats.
    Brief {
        /// Maximum number of messages to return.
        #[arg(default_value_t = DEFAULT_LIMIT, value_parser = positive_usize)]
        limit: usize,
    },
    /// Download one attachment into the current directory.
    Download {
        /// Exact file digest or case-insensitive filename fragment.
        file: String,
    },
}

fn positive_usize(value: &str) -> std::result::Result<usize, String> {
    let value = value
        .parse::<usize>()
        .map_err(|_| "must be a positive integer".to_string())?;
    if value == 0 {
        Err("must be greater than zero".into())
    } else {
        Ok(value)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimeRange {
    pub since: Option<u64>,
    pub until: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ParsedDate {
    Instant(u64),
    LocalDate(NaiveDate),
}

fn millis(datetime: DateTime<impl TimeZone>) -> Result<u64> {
    u64::try_from(datetime.timestamp_millis()).context("date is before the Unix epoch")
}

fn local_datetime(value: NaiveDateTime) -> Result<u64> {
    match Local.from_local_datetime(&value) {
        LocalResult::Single(datetime) => millis(datetime),
        LocalResult::Ambiguous(_, _) => {
            bail!("local date/time {value} is ambiguous; use RFC 3339 with an explicit offset")
        }
        LocalResult::None => {
            bail!("local date/time {value} does not exist; use RFC 3339 with an explicit offset")
        }
    }
}

fn parse_relative(value: &str, now: DateTime<Local>) -> Result<Option<u64>> {
    let Some(raw) = value.strip_suffix(" ago") else {
        return Ok(None);
    };
    let fields: Vec<_> = raw.split_whitespace().collect();
    let (amount, unit) = match fields.as_slice() {
        [combined] => {
            let split = combined
                .find(|character: char| !character.is_ascii_digit())
                .unwrap_or(combined.len());
            (&combined[..split], &combined[split..])
        }
        [amount, unit] => (*amount, *unit),
        _ => bail!("invalid relative date {value:?}"),
    };
    let amount = amount
        .parse::<i64>()
        .context("relative date needs a numeric amount")?;
    let duration = match unit {
        "s" | "sec" | "secs" | "second" | "seconds" => ChronoDuration::seconds(amount),
        "m" | "min" | "mins" | "minute" | "minutes" => ChronoDuration::minutes(amount),
        "h" | "hr" | "hrs" | "hour" | "hours" => ChronoDuration::hours(amount),
        "d" | "day" | "days" => ChronoDuration::days(amount),
        "w" | "week" | "weeks" => ChronoDuration::weeks(amount),
        _ => bail!("unsupported relative-date unit {unit:?}"),
    };
    Ok(Some(millis(now - duration)?))
}

fn parse_date(value: &str, now: DateTime<Local>) -> Result<ParsedDate> {
    let value = value.trim();
    let lower = value.to_lowercase();
    match lower.as_str() {
        "now" => return Ok(ParsedDate::Instant(millis(now)?)),
        "today" => return Ok(ParsedDate::LocalDate(now.date_naive())),
        "yesterday" => {
            let date = now
                .date_naive()
                .checked_sub_days(Days::new(1))
                .context("yesterday is outside the supported date range")?;
            return Ok(ParsedDate::LocalDate(date));
        }
        _ => {}
    }
    if let Some(relative) = parse_relative(&lower, now)? {
        return Ok(ParsedDate::Instant(relative));
    }
    if let Some(unix) = value.strip_prefix('@') {
        let milliseconds = if let Some(value) = unix.strip_suffix("ms") {
            value
                .parse::<u64>()
                .context("invalid Unix-millisecond timestamp")?
        } else {
            unix.parse::<u64>()
                .context("invalid Unix timestamp")?
                .checked_mul(1_000)
                .context("Unix timestamp is too large")?
        };
        return Ok(ParsedDate::Instant(milliseconds));
    }
    if let Ok(datetime) = DateTime::parse_from_rfc3339(value) {
        return Ok(ParsedDate::Instant(millis(datetime)?));
    }
    for format in ["%Y-%m-%d %H:%M:%S", "%Y-%m-%d %H:%M"] {
        if let Ok(datetime) = NaiveDateTime::parse_from_str(value, format) {
            return Ok(ParsedDate::Instant(local_datetime(datetime)?));
        }
    }
    if let Ok(date) = NaiveDate::parse_from_str(value, "%Y-%m-%d") {
        return Ok(ParsedDate::LocalDate(date));
    }
    bail!("invalid date {value:?}; try 'yesterday', '2 hours ago', '2026-08-08 14:30', RFC 3339, or @<unix-seconds>")
}

fn date_bound(value: ParsedDate, until: bool) -> Result<u64> {
    match value {
        ParsedDate::Instant(timestamp) => Ok(timestamp),
        ParsedDate::LocalDate(date) => {
            let date = if until {
                date.checked_add_days(Days::new(1))
                    .context("date is outside the supported range")?
            } else {
                date
            };
            local_datetime(date.and_hms_opt(0, 0, 0).expect("midnight is valid"))
        }
    }
}

pub fn parse_time_range(
    since: Option<&str>,
    until: Option<&str>,
    now: DateTime<Local>,
) -> Result<TimeRange> {
    let since = since
        .map(|value| parse_date(value, now).and_then(|value| date_bound(value, false)))
        .transpose()?;
    let until = until
        .map(|value| parse_date(value, now).and_then(|value| date_bound(value, true)))
        .transpose()?;
    if matches!((since, until), (Some(start), Some(end)) if start >= end) {
        bail!("--since must be earlier than --until");
    }
    Ok(TimeRange { since, until })
}

fn chat_json(chat: &Conversation) -> Value {
    json!({
        "id": chat.id.stable_id(),
        "kind": chat.id.kind(),
        "name": chat.title,
        "subtitle": chat.subtitle,
    })
}

fn timestamp(timestamp: u64) -> String {
    i64::try_from(timestamp)
        .ok()
        .and_then(|timestamp| Local.timestamp_millis_opt(timestamp).single())
        .map(|datetime| datetime.to_rfc3339_opts(SecondsFormat::Secs, false))
        .unwrap_or_else(|| timestamp.to_string())
}

fn attachment_json(attachment: &ChatAttachment) -> Value {
    json!({
        "id": attachment.download_id(),
        "name": attachment.file_name.clone().unwrap_or_else(|| attachments::safe_download_name(attachment)),
        "content_type": attachment.content_type,
        "size": attachment.size,
        "width": attachment.width,
        "height": attachment.height,
    })
}

fn message_json(message: &ChatMessage) -> Value {
    let attachments = message
        .attachments
        .iter()
        .map(attachment_json)
        .collect::<Vec<_>>();
    json!({
        "timestamp": timestamp(message.timestamp),
        "timestamp_ms": message.timestamp,
        "direction": if message.mine { "sent" } else { "received" },
        "sender": if message.mine { "you" } else { message.sender.as_deref().unwrap_or("unknown") },
        "kind": message.kind,
        "body": message.body,
        "attachment_count": message.attachments.len(),
        "attachments": attachments,
    })
}

fn brief_message_name(chat: &Conversation, message: &ChatMessage) -> String {
    if message.mine {
        format!("you → {}", chat.title)
    } else if matches!(chat.id, backend::ConversationId::Group(_)) {
        format!(
            "{} · {}",
            chat.title,
            message.sender.as_deref().unwrap_or("Unknown contact")
        )
    } else {
        chat.title.clone()
    }
}

fn human_size(size: Option<u32>) -> String {
    const UNITS: [&str; 4] = ["B", "KiB", "MiB", "GiB"];
    let Some(size) = size else {
        return "unknown size".into();
    };
    let mut value = f64::from(size);
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

fn write_json(value: &Value) -> Result<()> {
    let stdout = io::stdout();
    let mut output = stdout.lock();
    serde_json::to_writer(&mut output, value)?;
    writeln!(output)?;
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
struct HumanMessageRow {
    datetime: String,
    name: String,
    content: Vec<String>,
    incoming: bool,
}

fn short_timestamp(timestamp: u64) -> String {
    i64::try_from(timestamp)
        .ok()
        .and_then(|timestamp| Local.timestamp_millis_opt(timestamp).single())
        .map(|datetime| datetime.format("%d %b %H:%M").to_string())
        .unwrap_or_else(|| timestamp.to_string())
}

fn message_content(message: &ChatMessage) -> Vec<String> {
    let mut content = if message.body.is_empty() {
        Vec::new()
    } else {
        message.body.split('\n').map(str::to_string).collect()
    };
    content.extend(message.attachments.iter().map(|attachment| {
        format!(
            "file {} · {} · {} · {}",
            attachment.download_id().as_deref().unwrap_or("unavailable"),
            attachment
                .file_name
                .clone()
                .unwrap_or_else(|| attachments::safe_download_name(attachment)),
            human_size(attachment.size),
            attachment.content_type.as_deref().unwrap_or("unknown type")
        )
    }));
    if content.is_empty() {
        content.push(String::new());
    }
    content
}

fn message_row(message: &ChatMessage, name: String) -> HumanMessageRow {
    HumanMessageRow {
        datetime: short_timestamp(message.timestamp),
        name,
        content: message_content(message),
        incoming: !message.mine,
    }
}

fn pad_to_width(value: &str, width: usize) -> String {
    let padding = width.saturating_sub(UnicodeWidthStr::width(value));
    format!("{value}{}", " ".repeat(padding))
}

fn blue(value: String, enabled: bool) -> String {
    if enabled && !value.is_empty() {
        format!("\x1b[38;2;70;130;255m{value}\x1b[0m")
    } else {
        value
    }
}

fn color_enabled() -> bool {
    io::stdout().is_terminal()
        && std::env::var_os("NO_COLOR").is_none()
        && !std::env::var("TERM").is_ok_and(|term| term == "dumb")
}

fn render_message_rows(rows: &[HumanMessageRow], color: bool) -> String {
    let datetime_width = rows
        .iter()
        .map(|row| UnicodeWidthStr::width(row.datetime.as_str()))
        .max()
        .unwrap_or(0);
    let name_width = rows
        .iter()
        .map(|row| UnicodeWidthStr::width(row.name.as_str()))
        .max()
        .unwrap_or(0);
    let mut output = String::new();
    for row in rows {
        for (index, content) in row.content.iter().enumerate() {
            let datetime = if index == 0 {
                pad_to_width(&row.datetime, datetime_width)
            } else {
                " ".repeat(datetime_width)
            };
            let name = if index == 0 {
                pad_to_width(&row.name, name_width)
            } else {
                " ".repeat(name_width)
            };
            output.push_str(&datetime);
            output.push_str("  ");
            output.push_str(&blue(name, color && row.incoming));
            output.push_str("  ");
            output.push_str(&blue(content.clone(), color && row.incoming));
            output.push('\n');
        }
    }
    output
}

fn resolve_one<'a>(conversations: &'a [Conversation], query: &str) -> Result<&'a Conversation> {
    if query.is_empty() {
        bail!("chat query cannot be empty");
    }
    let matches = backend::matching_conversations(conversations, query);
    match matches.as_slice() {
        [] => bail!("no chat matches {query:?}; run `signal list` to see available chats"),
        [conversation] => Ok(conversation),
        _ => {
            let choices = matches
                .iter()
                .map(|chat| format!("  {}  {}", chat.id.stable_id(), chat.title))
                .collect::<Vec<_>>()
                .join("\n");
            bail!("multiple chats match {query:?}; use one of these stable IDs:\n{choices}")
        }
    }
}

fn resolve_attachment<'a>(
    catalog: &'a [AttachmentOccurrence],
    query: &str,
) -> Result<&'a AttachmentOccurrence> {
    if query.is_empty() {
        bail!("file query cannot be empty");
    }
    let query_lower = query.to_lowercase();
    if let Some(exact) = catalog.iter().find(|occurrence| {
        occurrence
            .attachment
            .download_id()
            .is_some_and(|id| id.eq_ignore_ascii_case(query))
    }) {
        return Ok(exact);
    }

    let mut seen = HashSet::new();
    let matches = catalog
        .iter()
        .filter(|occurrence| {
            occurrence
                .attachment
                .file_name
                .as_deref()
                .is_some_and(|name| name.to_lowercase().contains(&query_lower))
        })
        .filter(|occurrence| {
            occurrence
                .attachment
                .download_id()
                .is_some_and(|id| seen.insert(id))
        })
        .collect::<Vec<_>>();
    match matches.as_slice() {
        [] => bail!("no downloadable file matches {query:?}; use `signal read` or `signal brief` to see file IDs"),
        [occurrence] => Ok(occurrence),
        _ => {
            let choices = matches
                .iter()
                .map(|occurrence| {
                    format!(
                        "  {}  {}  {}  {}  {}",
                        occurrence.attachment.download_id().expect("match has an ID"),
                        occurrence.attachment.display_name(),
                        human_size(occurrence.attachment.size),
                        occurrence.chat.title,
                        timestamp(occurrence.timestamp)
                    )
                })
                .collect::<Vec<_>>()
                .join("\n");
            bail!("multiple files match {query:?}; use one of these IDs:\n{choices}")
        }
    }
}

fn warn_sync(error: &anyhow::Error) {
    eprintln!(
        "warning: Signal synchronization did not complete ({error:#}); using locally cached data"
    );
}

pub async fn run(
    manager: &mut Manager<SqliteStore, Registered>,
    command: Command,
    json_output: bool,
    attachment_cache: AttachmentCache,
) -> Result<()> {
    if let Err(error) = backend::sync_pending(manager, SYNC_TIMEOUT).await {
        warn_sync(&error);
    }
    let conversations = backend::conversations(manager).await?;

    match command {
        Command::List => {
            if json_output {
                write_json(
                    &json!({ "chats": conversations.iter().map(chat_json).collect::<Vec<_>>() }),
                )?;
            } else {
                for chat in &conversations {
                    println!("{}  {}  {}", chat.id.stable_id(), chat.title, chat.subtitle);
                }
            }
        }
        Command::Read {
            chat,
            since,
            until,
            limit,
        } => {
            let chat = resolve_one(&conversations, &chat)?;
            let range = parse_time_range(since.as_deref(), until.as_deref(), Local::now())?;
            let mut messages =
                backend::latest_messages(manager, chat, range.since, range.until, limit).await?;
            messages.reverse();
            if json_output {
                write_json(
                    &json!({ "chat": chat_json(chat), "messages": messages.iter().map(message_json).collect::<Vec<_>>() }),
                )?;
            } else {
                println!("{}  {}\n", chat.title, chat.id.stable_id());
                let rows = messages
                    .iter()
                    .map(|message| {
                        let name = if message.mine {
                            "you".to_string()
                        } else {
                            message.sender.clone().unwrap_or_else(|| "unknown".into())
                        };
                        message_row(message, name)
                    })
                    .collect::<Vec<_>>();
                print!("{}", render_message_rows(&rows, color_enabled()));
            }
        }
        Command::Send { chat, message } => {
            if message.is_empty() {
                bail!("message cannot be empty");
            }
            let chat = resolve_one(&conversations, &chat)?;
            let sent = backend::send(manager, chat, message).await?;
            if json_output {
                write_json(
                    &json!({ "sent": true, "chat": chat_json(chat), "message": message_json(&sent) }),
                )?;
            } else {
                println!(
                    "sent to {} ({}) at {}",
                    chat.title,
                    chat.id.stable_id(),
                    timestamp(sent.timestamp)
                );
            }
        }
        Command::Brief { limit } => {
            let mut messages = Vec::new();
            for chat in &conversations {
                for message in backend::latest_messages(manager, chat, None, None, limit).await? {
                    messages.push((chat, message));
                }
            }
            messages.sort_by(|(left_chat, left), (right_chat, right)| {
                right
                    .timestamp
                    .cmp(&left.timestamp)
                    .then_with(|| left_chat.id.stable_id().cmp(&right_chat.id.stable_id()))
                    .then_with(|| left.body.cmp(&right.body))
            });
            messages.truncate(limit);
            if json_output {
                let messages = messages
                    .iter()
                    .map(|(chat, message)| {
                        let mut value = message_json(message);
                        value
                            .as_object_mut()
                            .expect("message JSON is an object")
                            .insert("chat".into(), chat_json(chat));
                        value
                    })
                    .collect::<Vec<_>>();
                write_json(&json!({ "messages": messages }))?;
            } else {
                let rows = messages
                    .iter()
                    .map(|(chat, message)| message_row(message, brief_message_name(chat, message)))
                    .collect::<Vec<_>>();
                print!("{}", render_message_rows(&rows, color_enabled()));
            }
        }
        Command::Download { file } => {
            let catalog = backend::attachment_catalog(manager, &conversations).await?;
            let occurrence = resolve_attachment(&catalog, &file)?;
            let directory = std::env::current_dir().context("determine current directory")?;
            let path = attachments::download_to_directory(
                manager,
                &attachment_cache,
                &occurrence.attachment,
                &directory,
            )
            .await?;
            let id = occurrence
                .attachment
                .download_id()
                .expect("resolved attachment is downloadable");
            if json_output {
                write_json(&json!({
                    "downloaded": true,
                    "file": {
                        "id": id,
                        "name": occurrence.attachment.file_name.clone().unwrap_or_else(|| attachments::safe_download_name(&occurrence.attachment)),
                        "size": occurrence.attachment.size,
                        "content_type": occurrence.attachment.content_type,
                        "chat": chat_json(&occurrence.chat),
                        "timestamp": timestamp(occurrence.timestamp),
                        "timestamp_ms": occurrence.timestamp,
                    },
                    "path": path,
                }))?;
            } else {
                println!(
                    "downloaded {} ({}, {}) from {} to {}",
                    occurrence.attachment.display_name(),
                    human_size(occurrence.attachment.size),
                    id,
                    occurrence.chat.title,
                    path.display()
                );
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use chrono::{Local, TimeZone};
    use presage::libsignal_service::proto::{attachment_pointer, AttachmentPointer};

    use crate::backend::{
        AttachmentOccurrence, ChatAttachment, ChatMessage, Conversation, ConversationId,
        MessageKind,
    };

    use super::{
        brief_message_name, message_json, parse_date, parse_time_range, positive_usize,
        render_message_rows, resolve_attachment, HumanMessageRow, ParsedDate,
    };

    fn now() -> chrono::DateTime<Local> {
        Local
            .with_ymd_and_hms(2026, 8, 8, 12, 0, 0)
            .single()
            .unwrap()
    }

    fn occurrence(byte: u8, name: &str, timestamp: u64) -> AttachmentOccurrence {
        let pointer = AttachmentPointer {
            digest: Some(vec![byte; 32]),
            key: Some(vec![byte; 64]),
            attachment_identifier: Some(attachment_pointer::AttachmentIdentifier::CdnId(1)),
            file_name: Some(name.into()),
            size: Some(1_024),
            ..Default::default()
        };
        AttachmentOccurrence {
            chat: Conversation {
                id: ConversationId::Group([byte; 32]),
                title: format!("Chat {byte}"),
                subtitle: String::new(),
                group_revision: Some(1),
            },
            timestamp,
            attachment: ChatAttachment {
                key: hex::encode(pointer.digest.as_ref().unwrap()),
                file_name: pointer.file_name.clone(),
                content_type: Some("application/pdf".into()),
                size: pointer.size,
                width: None,
                height: None,
                pointer,
            },
        }
    }

    #[test]
    fn limit_must_be_positive() {
        assert_eq!(positive_usize("15").unwrap(), 15);
        assert!(positive_usize("0").is_err());
        assert!(positive_usize("nope").is_err());
    }

    #[test]
    fn parses_relative_and_unix_ranges() {
        let range = parse_time_range(Some("2 hours ago"), Some("now"), now()).unwrap();
        assert_eq!(
            range.until.unwrap() - range.since.unwrap(),
            2 * 60 * 60 * 1_000
        );
        assert_eq!(
            parse_date("@100", now()).unwrap(),
            ParsedDate::Instant(100_000)
        );
        assert_eq!(
            parse_date("@200000ms", now()).unwrap(),
            ParsedDate::Instant(200_000)
        );
        assert_eq!(
            parse_time_range(Some("@100"), Some("@200000ms"), now())
                .unwrap()
                .since,
            Some(100_000)
        );
    }

    #[test]
    fn date_only_until_includes_the_whole_day() {
        let range = parse_time_range(Some("2026-08-08"), Some("2026-08-08"), now()).unwrap();
        assert_eq!(
            range.until.unwrap() - range.since.unwrap(),
            24 * 60 * 60 * 1_000
        );
    }

    #[test]
    fn rejects_reversed_range() {
        assert!(parse_time_range(Some("now"), Some("2 hours ago"), now()).is_err());
    }

    #[test]
    fn json_message_contract_contains_machine_and_human_timestamps() {
        let file = occurrence(0xab, "Report.pdf", 1).attachment;
        let value = message_json(&ChatMessage {
            timestamp: 1_786_185_000_123,
            mine: true,
            sender: None,
            body: "hello".into(),
            kind: MessageKind::Text,
            attachments: vec![file],
        });
        assert_eq!(value["timestamp_ms"], 1_786_185_000_123_u64);
        assert_eq!(value["direction"], "sent");
        assert_eq!(value["sender"], "you");
        assert_eq!(value["kind"], "text");
        assert_eq!(value["attachments"][0]["id"], "ab".repeat(32));
        assert_eq!(value["attachments"][0]["size"], 1_024);
    }

    #[test]
    fn file_resolution_prefers_ids_and_deduplicates_repeats() {
        let newest = occurrence(1, "Report.pdf", 30);
        let repeated = occurrence(1, "Report.pdf", 20);
        let other = occurrence(2, "Report-final.pdf", 10);
        let catalog = vec![newest, repeated, other];

        let id = "01".repeat(32).to_uppercase();
        assert_eq!(resolve_attachment(&catalog, &id).unwrap().timestamp, 30);
        assert!(resolve_attachment(&catalog, "report").is_err());

        let only_repeat = &catalog[..2];
        assert_eq!(
            resolve_attachment(only_repeat, "REPORT").unwrap().timestamp,
            30
        );
    }

    #[test]
    fn human_messages_render_in_aligned_columns() {
        let rows = vec![
            HumanMessageRow {
                datetime: "08 Aug 15:21".into(),
                name: "you".into(),
                content: vec!["Hallo".into()],
                incoming: false,
            },
            HumanMessageRow {
                datetime: "08 Aug 15:22".into(),
                name: "Emilian".into(),
                content: vec!["Wie geht es dir?".into(), "zweite Zeile".into()],
                incoming: true,
            },
        ];
        assert_eq!(
            render_message_rows(&rows, false),
            concat!(
                "08 Aug 15:21  you      Hallo\n",
                "08 Aug 15:22  Emilian  Wie geht es dir?\n",
                "                       zweite Zeile\n"
            )
        );
    }

    #[test]
    fn brief_uses_human_chat_and_sender_names() {
        let direct = Conversation {
            id: ConversationId::Contact(
                presage::libsignal_service::protocol::ServiceId::parse_from_service_id_string(
                    "0b9240d8-ae86-42dd-bb03-df2805b010b7",
                )
                .unwrap(),
            ),
            title: "Vera Scheel".into(),
            subtitle: String::new(),
            group_revision: None,
        };
        let group = Conversation {
            id: ConversationId::Group([3; 32]),
            title: "Weekend plans".into(),
            subtitle: String::new(),
            group_revision: Some(1),
        };
        let incoming = ChatMessage {
            timestamp: 1,
            mine: false,
            sender: Some("Birgit Mallmann".into()),
            body: "Hello".into(),
            kind: MessageKind::Text,
            attachments: Vec::new(),
        };
        assert_eq!(brief_message_name(&direct, &incoming), "Vera Scheel");
        assert_eq!(
            brief_message_name(&group, &incoming),
            "Weekend plans · Birgit Mallmann"
        );

        let unknown = ChatMessage {
            sender: None,
            ..incoming.clone()
        };
        assert_eq!(
            brief_message_name(&group, &unknown),
            "Weekend plans · Unknown contact"
        );

        let sent = ChatMessage {
            mine: true,
            sender: None,
            ..incoming
        };
        assert_eq!(brief_message_name(&direct, &sent), "you → Vera Scheel");
    }

    #[test]
    fn color_is_applied_only_to_incoming_name_and_content() {
        let rows = vec![
            HumanMessageRow {
                datetime: "08 Aug 15:21".into(),
                name: "you".into(),
                content: vec!["mine".into()],
                incoming: false,
            },
            HumanMessageRow {
                datetime: "08 Aug 15:22".into(),
                name: "Emilian".into(),
                content: vec!["theirs".into()],
                incoming: true,
            },
        ];
        let rendered = render_message_rows(&rows, true);
        assert!(rendered.contains("you      mine"));
        assert!(rendered.contains("\x1b[38;2;70;130;255mEmilian\x1b[0m"));
        assert!(rendered.contains("\x1b[38;2;70;130;255mtheirs\x1b[0m"));
    }
}
