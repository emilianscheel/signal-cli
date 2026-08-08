mod app;
mod backend;
mod ui;

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;
use directories::ProjectDirs;
use presage::{manager::Registered, model::identity::OnNewIdentity, Manager};
use presage_store_sqlite::SqliteStore;

use crate::{app::App, backend::link_device};

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

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    let args = Args::parse();
    init_logging(args.verbose);

    let custom_data_path = args.data.is_some();
    let path = data_path(args.data)?;
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
    local
        .run_until(async move {
            let manager: Manager<SqliteStore, Registered> =
                match Manager::load_registered(store.clone()).await {
                    Ok(manager) => manager,
                    Err(_) => link_device(store, args.device_name).await?,
                };

            App::new(manager).run().await
        })
        .await
}

#[cfg(test)]
mod tests {
    use super::{data_path, protect_local_data};
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
}
