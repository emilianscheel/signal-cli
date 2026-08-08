# Signal CLI

A fast, keyboard-first Signal client built in Rust. Run `signal`, choose a chat,
and message your Signal contacts without leaving the terminal.

The interface is deliberately small: conversation history at the top, a focused
composer at the bottom, and keyboard controls inspired by Codex CLI and Claude
Code.

## What works

- First-run linking from Signal on iPhone using a terminal QR code
- Persistent linked-device credentials and Signal protocol state
- Synced contacts and groups
- Live incoming direct and group messages
- Direct and group text messages
- Local message history (up to the latest 250 messages per chat)
- Compact inline images in terminals with Kitty, iTerm2, or Sixel graphics support
- Filenames, media types, and sizes for videos, PDFs, and other attachments
- Authenticated attachment downloads by stable ID or filename query
- Scriptable chat listing, reading, sending, and cross-chat briefs
- Human-readable date filters and JSON output for automation
- Unicode editing and keyboard-only navigation
- Responsive 32-column chat sidebar in terminals 120 columns or wider
- Last-opened chat restoration across launches
- Restrictive default `0700` data-directory and `0600` database permissions on Unix

Signal does not provide a single API token. This program performs the same
secondary-device provisioning flow as Signal Desktop and stores the resulting
identity keys and session state in its local database.

## Install

You need a current Rust toolchain and the native build tools required by
libsignal (on macOS, Xcode Command Line Tools are sufficient).

```sh
cargo install --path . --locked
```

This installs a binary named `signal` in Cargo's bin directory, normally
`~/.cargo/bin`. Ensure that directory is on your `PATH`, then launch it:

```sh
signal
```

For development:

```sh
cargo run --release
```

## One-shot commands

Running `signal` without a command opens the interactive interface. The same
linked-device database can also be used from scripts or directly from your
shell:

```sh
signal list
signal read emilian
signal read emilian --since yesterday --until now --limit 30
signal send emilian "See you soon"
signal brief
signal brief 10
signal download "annual report"
signal download 0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef
```

`list` prints all synced contacts and groups, including chats without local
messages. Each entry has a stable ID such as `contact:<Signal service ID>` or
`group:<64 hex characters>`. A `read` or `send` target can be that exact ID or
a case-insensitive part of a contact or group name. If a name matches more than
one chat, no action is taken and the matching IDs are printed so you can choose
one explicitly.

`read` returns the latest 15 matching messages in chronological order by
default. Use `--limit N` to change that maximum. `brief` returns the latest 15
messages across all chats, newest first, or the positional count supplied as
`signal brief N`. Their human and JSON outputs include each attachment's
filename, size, media type, and downloadable ID when its Signal pointer is
complete.

`download` searches attachment filenames case-insensitively across all locally
stored history, independent of the `read` and TUI message limits. An exact
64-character attachment ID takes precedence over filename matching. If a query
matches multiple distinct files, nothing is downloaded and the matching IDs,
names, sizes, chats, and timestamps are listed. Repeated appearances of the
same ID count as one file.

Downloads are authenticated and decrypted by Signal's attachment protocol and
saved in the current directory. Signal-provided path components are stripped.
Unnamed files receive a digest-based name, and an existing filename is never
overwritten: subsequent downloads use names such as `report (1).pdf`. On Unix,
downloaded files are created with `0600` permissions.

Date bounds use `--since` and `--until`. `since` is inclusive; a date-only
`until` includes that entire local calendar day, while a date and time is an
exclusive bound. Multi-word values need shell quotes. Supported forms include:

```sh
signal read emilian --since "2 hours ago"
signal read emilian --since 2026-08-01 --until 2026-08-08
signal read emilian --since "2026-08-08 09:30" --until now
signal read emilian --since 2026-08-08T09:30:00+02:00
signal read emilian --since @1786174200
signal read emilian --since @1786174200000ms
```

The keywords `now`, `today`, and `yesterday` are supported, as are relative
seconds, minutes, hours, days, and weeks. Local times that are ambiguous or do
not exist because of a daylight-saving transition must be supplied as RFC 3339
with an explicit offset.

Add `--json` before or after a command for stable machine-readable output:

```sh
signal --json list
signal brief 10 --json
signal download "annual report" --json
```

Command results are written to stdout. Provisioning, freshness warnings, and
errors go to stderr, so stdout remains parseable JSON. Before a one-shot
command, Signal CLI waits up to ten seconds for pending linked-device updates.
If synchronization times out or the network is unavailable, it warns and uses
the local cache; an attempted `send` still succeeds or fails based on Signal's
send operation.

Only history already delivered to this linked device is available. Signal does
not offer linked clients a way to fetch arbitrary older server history.
Text-containing messages are printed as text. JSON output includes structured
attachment metadata; human-readable `read` and `brief` output includes one file
line per attachment. Other non-text messages such as stickers, reactions,
edits, polls, calls, and stories are shown with concise placeholders.

## First run

1. Run `signal` in a terminal wide enough to display a QR code.
2. On iPhone, open **Signal → Settings → Linked Devices → Link New Device**.
3. Scan the QR code shown in the terminal.
4. Wait for contacts and groups to synchronize, then select a chat with `Enter`.

The database is stored in the operating system's local data directory. Use
`signal --data /path/to/signal.db` to choose a different location.

The interactive interface automatically downloads images near the visible
viewport when the terminal supports a native graphics protocol. Previews are
limited to three per message, 40% of the chat width, and 12 terminal rows.
Other terminals show the same attachment metadata without downloading image
bytes.

## Keys

| Screen | Key | Action |
| --- | --- | --- |
| Chats | `↑`/`↓` or `j`/`k` | Select a conversation |
| Chats | `Enter` | Open the selected conversation |
| Chats | `r` | Reload synced contacts and groups |
| Chats | `d`, then `y` | Erase local account data and disconnect |
| Chat | `Enter` | Send the current message |
| Chat | `PageUp`/`PageDown` | Scroll message history |
| Chat | `Esc` | Return to the chat list |
| Anywhere | `Ctrl-C` | Quit safely |

At widths of 120 columns or more, chats remain visible in a fixed 32-column
sidebar. Press `Esc` to focus the sidebar, move with the arrow keys, and press
`Enter` to open the highlighted chat and return focus to the composer. Narrower
terminals retain the separate chat-list and chat screens. The last opened chat
is remembered in a private local preference file.

## Security and scope

The local database contains linked-device credentials and decrypted message
content. Keep it on a trusted disk and do not copy or commit it. Removing it
requires linking the CLI again. You can revoke the client at any time from
Signal's **Linked Devices** screen.

Downloaded image previews are decrypted and cached beside the database in a
private `0700` directory with `0600` files. The cache is capped at 250 MiB and
is removed by the confirmed disconnect action.

The in-app disconnect action erases all local credentials, protocol sessions,
contacts, groups, and messages after confirmation. Signal does not allow a
secondary device to revoke itself, so also remove **Signal CLI** from the
iPhone's **Linked Devices** screen to complete server-side revocation.

This is an independent, unofficial client and is not affiliated with Signal.
The current release supports receiving attachment previews and metadata but
does not yet send, open, or play attachments. Attachments can be saved through
the one-shot `download` command. Reactions, typing indicators, safety-number
management, and disappearing-message cleanup are not yet exposed in the UI.

## Development checks

```sh
cargo fmt --all -- --check
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
```

The project is AGPL-3.0-only because its Signal client dependency, Presage, is
AGPL-3.0-only.
