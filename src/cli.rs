use std::{
    io::{self, Write},
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

use crate::backend::{self, ChatMessage, Conversation};

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

fn message_json(message: &ChatMessage) -> Value {
    let attachments = message
        .attachments
        .iter()
        .map(|attachment| {
            json!({
                "name": attachment.file_name,
                "content_type": attachment.content_type,
                "size": attachment.size,
                "width": attachment.width,
                "height": attachment.height,
            })
        })
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

fn write_json(value: &Value) -> Result<()> {
    let stdout = io::stdout();
    let mut output = stdout.lock();
    serde_json::to_writer(&mut output, value)?;
    writeln!(output)?;
    Ok(())
}

fn body_for_human(message: &ChatMessage) -> String {
    let mut body = message.body.replace('\n', "\n    ");
    if !message.attachments.is_empty() {
        let noun = if message.attachments.len() == 1 {
            "attachment"
        } else {
            "attachments"
        };
        if !body.is_empty() {
            body.push(' ');
        }
        body.push_str(&format!("[{0} {noun}]", message.attachments.len()));
    }
    body
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

fn warn_sync(error: &anyhow::Error) {
    eprintln!(
        "warning: Signal synchronization did not complete ({error:#}); using locally cached data"
    );
}

pub async fn run(
    manager: &mut Manager<SqliteStore, Registered>,
    command: Command,
    json_output: bool,
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
                println!("{}  {}", chat.title, chat.id.stable_id());
                for message in &messages {
                    let sender = if message.mine {
                        "you"
                    } else {
                        message.sender.as_deref().unwrap_or("unknown")
                    };
                    println!(
                        "{}  {}: {}",
                        timestamp(message.timestamp),
                        sender,
                        body_for_human(message)
                    );
                }
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
                for (chat, message) in &messages {
                    let sender = if message.mine {
                        "you"
                    } else {
                        message.sender.as_deref().unwrap_or("unknown")
                    };
                    println!(
                        "{}  {} — {}: {}",
                        timestamp(message.timestamp),
                        chat.title,
                        sender,
                        body_for_human(message)
                    );
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use chrono::{Local, TimeZone};

    use crate::backend::{ChatMessage, MessageKind};

    use super::{message_json, parse_date, parse_time_range, positive_usize, ParsedDate};

    fn now() -> chrono::DateTime<Local> {
        Local
            .with_ymd_and_hms(2026, 8, 8, 12, 0, 0)
            .single()
            .unwrap()
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
        let value = message_json(&ChatMessage {
            timestamp: 1_786_185_000_123,
            mine: true,
            sender: None,
            body: "hello".into(),
            kind: MessageKind::Text,
            attachments: Vec::new(),
        });
        assert_eq!(value["timestamp_ms"], 1_786_185_000_123_u64);
        assert_eq!(value["direction"], "sent");
        assert_eq!(value["sender"], "you");
        assert_eq!(value["kind"], "text");
    }
}
