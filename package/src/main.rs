mod app;
mod attachments;
mod backend;
mod cli;
mod preferences;
mod sync;
mod ui;
mod updater;

use std::{path::PathBuf, process::ExitCode};

use anyhow::{Context, Result};
use clap::Parser;
use directories::ProjectDirs;
use presage::{manager::Registered, model::identity::OnNewIdentity, Manager};
use presage_store_sqlite::SqliteStore;

use crate::{
    app::App, attachments::AttachmentCache, backend::link_device, preferences::PreferencesStore,
};

fn link_progress_message(progress: sync::SyncProgress) -> String {
    match progress {
        sync::SyncProgress::WaitingForPhone => "Waiting for phone to prepare history".into(),
        sync::SyncProgress::Downloading {
            downloaded_bytes,
            total_bytes,
        } => match total_bytes {
            Some(total) if total > 0 => format!(
                "Downloading history: {downloaded_bytes}/{total} bytes ({}%)",
                downloaded_bytes.saturating_mul(100) / total
            ),
            _ => format!("Downloading history: {downloaded_bytes} bytes"),
        },
        sync::SyncProgress::Validating => "Validating history archive".into(),
        sync::SyncProgress::Importing { imported_messages } => {
            format!("Importing history: {imported_messages} messages")
        }
        sync::SyncProgress::RefreshingPending => "Refreshing contacts and queued messages".into(),
    }
}

#[cfg(unix)]
fn protect_local_data(path: &std::path::Path, protect_parent: bool) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    if protect_parent {
        let parent = path.parent().context("data path has no parent directory")?;
        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))?;
    }
    if path.exists() {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn protect_local_data(_path: &std::path::Path, _protect_parent: bool) -> Result<()> {
    Ok(())
}

fn preference_path(path: &std::path::Path) -> PathBuf {
    let mut preference = path.as_os_str().to_os_string();
    preference.push(".ui.json");
    PathBuf::from(preference)
}

fn attachment_cache_path(path: &std::path::Path) -> PathBuf {
    let mut cache = path.as_os_str().to_os_string();
    cache.push(".attachments");
    PathBuf::from(cache)
}

fn remove_local_data(
    path: &std::path::Path,
    preferences: &std::path::Path,
    attachments: &std::path::Path,
) -> Result<()> {
    let mut candidates = Vec::new();
    for suffix in ["", "-wal", "-shm"] {
        let mut candidate = path.as_os_str().to_os_string();
        candidate.push(suffix);
        candidates.push(PathBuf::from(candidate));
    }
    candidates.push(preferences.to_path_buf());
    let mut temporary_preferences = preferences.as_os_str().to_os_string();
    temporary_preferences.push(".tmp");
    candidates.push(PathBuf::from(temporary_preferences));

    for candidate in candidates {
        match std::fs::remove_file(&candidate) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("remove local data {}", candidate.display()));
            }
        }
    }
    match std::fs::remove_dir_all(attachments) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error)
                .with_context(|| format!("remove attachment cache {}", attachments.display()));
        }
    }
    Ok(())
}

#[derive(Debug, Parser)]
#[command(
    name = "signal",
    version,
    about = "A fast Signal client for your terminal"
)]
struct Args {
    /// Override the local Signal database location.
    #[arg(long, value_name = "PATH")]
    data: Option<PathBuf>,

    /// Name shown in Signal's Linked Devices screen.
    #[arg(long, default_value = "Signal CLI")]
    device_name: String,

    /// Enable diagnostic logs (also accepts RUST_LOG).
    #[arg(long)]
    verbose: bool,

    /// Emit machine-readable JSON for one-shot commands.
    #[arg(long, global = true)]
    json: bool,

    /// Run the managed-install update helper.
    #[arg(long, hide = true)]
    internal_update: bool,

    #[command(subcommand)]
    command: Option<cli::Command>,
}

fn data_path(override_path: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(path) = override_path {
        return Ok(path);
    }
    let dirs = ProjectDirs::from("dev", "signal-cli", "signal")
        .context("could not determine the user data directory")?;
    Ok(dirs.data_local_dir().join("signal.db"))
}

fn init_logging(verbose: bool) {
    let fallback = if verbose {
        "signal=debug,warn"
    } else {
        "error"
    };
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(fallback))
        .add_directive("libsignal=error".parse().expect("valid filter"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_target(false)
        .compact()
        .init();
}

async fn run(
    args: Args,
    update_notice: Option<String>,
    update_monitor: Option<updater::UpdateMonitor>,
) -> Result<()> {
    init_logging(args.verbose);

    let command_mode = args.command.is_some();
    let json_output = args.json;
    let custom_data_path = args.data.is_some();
    let path = data_path(args.data)?;
    let ui_preferences_path = preference_path(&path);
    let attachment_cache_path = attachment_cache_path(&path);
    let link_sync_paths = sync::LinkSyncPaths::for_database(&path);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create data directory {}", parent.display()))?;
    }

    let store = SqliteStore::open(
        path.to_str().context("data path is not valid UTF-8")?,
        OnNewIdentity::Trust,
    )
    .await
    .context("open the local Signal database")?;
    protect_local_data(&path, !custom_data_path)
        .context("restrict permissions on the local Signal database")?;

    let local = tokio::task::LocalSet::new();
    let app_preferences_path = ui_preferences_path.clone();
    let app_attachment_cache_path = attachment_cache_path.clone();
    let app_link_sync_paths = link_sync_paths.clone();
    let disconnected = local
        .run_until(async move {
            let mut manager: Manager<SqliteStore, Registered> =
                match Manager::load_registered(store.clone()).await {
                    Ok(manager) => manager,
                    Err(_) => {
                        link_device(store, args.device_name, command_mode, &app_link_sync_paths)
                            .await?
                    }
                };

            sync::download_link_history(&manager, &app_link_sync_paths, |progress| {
                let message = link_progress_message(progress);
                if command_mode {
                    eprintln!("{message}");
                } else {
                    println!("{message}");
                }
            })
            .await?;

            let history_report =
                sync::import_link_history(&manager, &app_link_sync_paths, |progress| {
                    let message = link_progress_message(progress);
                    if command_mode {
                        eprintln!("{message}");
                    } else {
                        println!("{message}");
                    }
                })
                .await?;
            if history_report.imported_messages > 0 {
                let message = format!(
                    "History import complete: {} messages",
                    history_report.imported_messages
                );
                if command_mode {
                    eprintln!("{message}");
                } else {
                    println!("{message}");
                }
            }

            if let Some(command) = args.command {
                cli::run(
                    &mut manager,
                    command,
                    json_output,
                    AttachmentCache::new(app_attachment_cache_path),
                )
                .await?;
                Ok(false)
            } else {
                App::new(
                    manager,
                    PreferencesStore::new(app_preferences_path),
                    app_attachment_cache_path,
                    app_link_sync_paths,
                    update_notice,
                    update_monitor,
                )
                .run()
                .await
            }
        })
        .await?;

    if disconnected {
        remove_local_data(&path, &ui_preferences_path, &attachment_cache_path)?;
        link_sync_paths.cleanup()?;
    }
    Ok(())
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> ExitCode {
    let args = Args::parse();
    if args.internal_update {
        return if updater::run_helper().await.is_ok() {
            ExitCode::SUCCESS
        } else {
            ExitCode::FAILURE
        };
    }
    let json_output = args.json;
    let update_notice = if json_output {
        None
    } else {
        updater::take_success_notice()
    };
    if args.command.is_some() {
        if let Some(notice) = &update_notice {
            eprintln!("{notice}");
        }
    }
    let update_monitor = updater::spawn_helper();
    match run(args, update_notice, update_monitor).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            if json_output {
                eprintln!("{}", serde_json::json!({ "error": format!("{error:#}") }));
            } else {
                eprintln!("error: {error:#}");
            }
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::{
        attachment_cache_path, cli::Command, data_path, preference_path, protect_local_data,
        remove_local_data, Args,
    };
    use std::path::PathBuf;

    #[test]
    fn explicit_data_path_wins() {
        let path = PathBuf::from("/tmp/signal-test.db");
        assert_eq!(data_path(Some(path.clone())).unwrap(), path);
    }

    #[cfg(unix)]
    #[test]
    fn custom_data_path_does_not_change_parent_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().unwrap();
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
        let database = directory.path().join("signal.db");
        std::fs::write(&database, []).unwrap();

        protect_local_data(&database, false).unwrap();

        assert_eq!(
            std::fs::metadata(directory.path())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o755
        );
        assert_eq!(
            std::fs::metadata(database).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn disconnect_removes_database_and_sidecars() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("signal.db");
        std::fs::write(&database, "database").unwrap();
        std::fs::write(directory.path().join("signal.db-wal"), "wal").unwrap();
        std::fs::write(directory.path().join("signal.db-shm"), "shm").unwrap();
        let preferences = preference_path(&database);
        let attachments = attachment_cache_path(&database);
        std::fs::create_dir(&attachments).unwrap();
        std::fs::write(attachments.join("cached-image"), "image").unwrap();
        std::fs::write(&preferences, "preferences").unwrap();
        let mut temporary_preferences = preferences.as_os_str().to_os_string();
        temporary_preferences.push(".tmp");
        let temporary_preferences = PathBuf::from(temporary_preferences);
        std::fs::write(&temporary_preferences, "temporary").unwrap();

        remove_local_data(&database, &preferences, &attachments).unwrap();

        assert!(!database.exists());
        assert!(!directory.path().join("signal.db-wal").exists());
        assert!(!directory.path().join("signal.db-shm").exists());
        assert!(!preferences.exists());
        assert!(!temporary_preferences.exists());
        assert!(!attachments.exists());
    }

    #[test]
    fn parses_commands_and_global_json_in_either_position() {
        let before = Args::try_parse_from(["signal", "--json", "brief", "10"]).unwrap();
        assert!(before.json);
        assert!(matches!(before.command, Some(Command::Brief { limit: 10 })));

        let after =
            Args::try_parse_from(["signal", "read", "emilian", "--limit", "20", "--json"]).unwrap();
        assert!(after.json);
        assert!(matches!(
            after.command,
            Some(Command::Read { limit: 20, .. })
        ));

        let send = Args::try_parse_from(["signal", "send", "emilian", "  exact text  "]).unwrap();
        assert!(matches!(
            send.command,
            Some(Command::Send { message, .. }) if message == "  exact text  "
        ));

        let download =
            Args::try_parse_from(["signal", "download", "Annual Report", "--json"]).unwrap();
        assert!(download.json);
        assert!(matches!(
            download.command,
            Some(Command::Download { file }) if file == "Annual Report"
        ));
    }

    #[test]
    fn bare_signal_still_selects_the_tui() {
        assert!(Args::try_parse_from(["signal"]).unwrap().command.is_none());
    }

    #[test]
    fn internal_updater_mode_is_parseable_without_a_command() {
        let args = Args::try_parse_from(["signal", "--internal-update"]).unwrap();
        assert!(args.internal_update);
        assert!(args.command.is_none());
    }

    #[test]
    fn clap_rejects_zero_limits() {
        assert!(Args::try_parse_from(["signal", "brief", "0"]).is_err());
        assert!(Args::try_parse_from(["signal", "read", "chat", "--limit", "0"]).is_err());
    }
}
