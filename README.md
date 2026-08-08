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
- Unicode editing and keyboard-only navigation
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

## First run

1. Run `signal` in a terminal wide enough to display a QR code.
2. On iPhone, open **Signal → Settings → Linked Devices → Link New Device**.
3. Scan the QR code shown in the terminal.
4. Wait for contacts and groups to synchronize, then select a chat with `Enter`.

The database is stored in the operating system's local data directory. Use
`signal --data /path/to/signal.db` to choose a different location.

## Keys

| Screen | Key | Action |
| --- | --- | --- |
| Chats | `↑`/`↓` or `j`/`k` | Select a conversation |
| Chats | `Enter` | Open the selected conversation |
| Chats | `r` | Reload synced contacts and groups |
| Chat | `Enter` | Send the current message |
| Chat | `PageUp`/`PageDown` | Scroll message history |
| Chat | `Esc` | Return to the chat list |
| Anywhere | `Ctrl-C` | Quit safely |

## Security and scope

The local database contains linked-device credentials and decrypted message
content. Keep it on a trusted disk and do not copy or commit it. Removing it
requires linking the CLI again. You can revoke the client at any time from
Signal's **Linked Devices** screen.

This is an independent, unofficial client and is not affiliated with Signal.
The current release focuses on fast text chat; attachments, reactions, typing
indicators, safety-number management, and disappearing-message cleanup are not
yet exposed in the UI.

## Development checks

```sh
cargo fmt --all -- --check
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
```

The project is AGPL-3.0-only because its Signal client dependency, Presage, is
AGPL-3.0-only.
