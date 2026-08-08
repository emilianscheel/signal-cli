use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use presage::libsignal_service::protocol::ServiceId;
use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::backend::ConversationId;

#[derive(Debug, Serialize, Deserialize)]
struct Preferences {
    last_conversation: SavedConversation,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind", content = "id", rename_all = "snake_case")]
enum SavedConversation {
    Contact(String),
    Group(String),
}

impl From<&ConversationId> for SavedConversation {
    fn from(value: &ConversationId) -> Self {
        match value {
            ConversationId::Contact(id) => Self::Contact(id.service_id_string()),
            ConversationId::Group(key) => Self::Group(hex::encode(key)),
        }
    }
}

impl SavedConversation {
    fn into_conversation_id(self) -> Option<ConversationId> {
        match self {
            Self::Contact(id) => {
                ServiceId::parse_from_service_id_string(&id).map(ConversationId::Contact)
            }
            Self::Group(key) => hex::decode(key)
                .ok()?
                .try_into()
                .ok()
                .map(ConversationId::Group),
        }
    }
}

#[derive(Debug, Clone)]
pub struct PreferencesStore {
    path: PathBuf,
}

impl PreferencesStore {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    pub fn load_last_conversation(&self) -> Option<ConversationId> {
        let bytes = match std::fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return None,
            Err(error) => {
                warn!(path = %self.path.display(), %error, "failed to read UI preferences");
                return None;
            }
        };
        match serde_json::from_slice::<Preferences>(&bytes) {
            Ok(preferences) => preferences.last_conversation.into_conversation_id(),
            Err(error) => {
                warn!(path = %self.path.display(), %error, "ignoring invalid UI preferences");
                None
            }
        }
    }

    pub fn save_last_conversation(&self, id: &ConversationId) -> Result<()> {
        let preferences = Preferences {
            last_conversation: SavedConversation::from(id),
        };
        let bytes = serde_json::to_vec_pretty(&preferences)?;
        let temporary = temporary_path(&self.path);

        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        write_private(&temporary, &bytes)
            .with_context(|| format!("write UI preferences {}", temporary.display()))?;
        std::fs::rename(&temporary, &self.path)
            .with_context(|| format!("commit UI preferences {}", self.path.display()))?;
        protect_file(&self.path)?;
        Ok(())
    }

    #[cfg(test)]
    fn path(&self) -> &Path {
        &self.path
    }
}

fn temporary_path(path: &Path) -> PathBuf {
    let mut temporary = path.as_os_str().to_os_string();
    temporary.push(".tmp");
    PathBuf::from(temporary)
}

#[cfg(unix)]
fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::{io::Write, os::unix::fs::OpenOptionsExt};

    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

#[cfg(not(unix))]
fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    std::fs::write(path, bytes)?;
    Ok(())
}

#[cfg(unix)]
fn protect_file(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(())
}

#[cfg(not(unix))]
fn protect_file(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::PreferencesStore;
    use crate::backend::ConversationId;

    #[test]
    fn round_trips_group_conversation() {
        let directory = tempfile::tempdir().unwrap();
        let store = PreferencesStore::new(directory.path().join("ui.json"));
        let id = ConversationId::Group([42; 32]);

        store.save_last_conversation(&id).unwrap();

        assert_eq!(store.load_last_conversation(), Some(id));
        assert!(!directory.path().join("ui.json.tmp").exists());
    }

    #[test]
    fn malformed_preferences_are_ignored() {
        let directory = tempfile::tempdir().unwrap();
        let store = PreferencesStore::new(directory.path().join("ui.json"));
        std::fs::write(store.path(), "not json").unwrap();
        assert_eq!(store.load_last_conversation(), None);
    }

    #[cfg(unix)]
    #[test]
    fn preferences_are_private() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().unwrap();
        let store = PreferencesStore::new(directory.path().join("ui.json"));
        store
            .save_last_conversation(&ConversationId::Group([7; 32]))
            .unwrap();
        assert_eq!(
            std::fs::metadata(store.path())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
}
