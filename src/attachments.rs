use std::{
    fs,
    io::{Cursor, Write},
    path::{Path, PathBuf},
    sync::Arc,
    time::SystemTime,
};

use anyhow::{bail, Context, Result};
use image::{DynamicImage, ImageReader};
use presage::{manager::Registered, Manager};
use presage_store_sqlite::SqliteStore;

use crate::backend::ChatAttachment;

pub const MAX_ATTACHMENT_BYTES: usize = 50 * 1024 * 1024;
pub const MAX_IMAGE_PIXELS: u64 = 40_000_000;
const MAX_CACHE_BYTES: u64 = 250 * 1024 * 1024;

#[derive(Clone, Debug)]
pub struct AttachmentCache {
    path: PathBuf,
}

impl AttachmentCache {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    fn file(&self, key: &str) -> PathBuf {
        self.path.join(key)
    }

    pub fn read(&self, key: &str) -> Result<Option<Vec<u8>>> {
        match fs::read(self.file(key)) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error).with_context(|| format!("read cached attachment {key}")),
        }
    }

    pub fn remove(&self, key: &str) {
        let _ = fs::remove_file(self.file(key));
    }

    pub fn write(&self, key: &str, bytes: &[u8]) -> Result<()> {
        self.ensure_private_directory()?;
        let destination = self.file(key);
        let temporary = self.path.join(format!(".{key}.tmp"));
        write_private(&temporary, bytes)
            .with_context(|| format!("write attachment cache {}", temporary.display()))?;
        fs::rename(&temporary, &destination)
            .with_context(|| format!("commit attachment cache {}", destination.display()))?;
        protect_file(&destination)?;
        self.evict_to_limit(MAX_CACHE_BYTES)?;
        Ok(())
    }

    fn ensure_private_directory(&self) -> Result<()> {
        fs::create_dir_all(&self.path)
            .with_context(|| format!("create attachment cache {}", self.path.display()))?;
        protect_directory(&self.path)
    }

    fn evict_to_limit(&self, limit: u64) -> Result<()> {
        let mut entries = Vec::new();
        let mut total = 0_u64;
        for entry in fs::read_dir(&self.path)? {
            let entry = entry?;
            let metadata = entry.metadata()?;
            if !metadata.is_file() || entry.file_name().to_string_lossy().starts_with('.') {
                continue;
            }
            total = total.saturating_add(metadata.len());
            entries.push((
                metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH),
                metadata.len(),
                entry.path(),
            ));
        }
        entries.sort_by_key(|entry| entry.0);
        for (_, size, path) in entries {
            if total <= limit {
                break;
            }
            fs::remove_file(&path)
                .with_context(|| format!("evict cached attachment {}", path.display()))?;
            total = total.saturating_sub(size);
        }
        Ok(())
    }

    #[cfg(test)]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

#[derive(Clone, Debug)]
pub enum AttachmentState {
    NotRequested,
    Loading,
    Ready(Arc<DynamicImage>),
    Failed(String),
}

#[derive(Debug)]
pub struct AttachmentEvent {
    pub key: String,
    pub result: Result<Arc<DynamicImage>, String>,
}

pub async fn load_image(
    manager: Manager<SqliteStore, Registered>,
    cache: AttachmentCache,
    attachment: ChatAttachment,
) -> AttachmentEvent {
    let key = attachment.key.clone();
    let result = load_image_inner(manager, cache, &attachment)
        .await
        .map(Arc::new)
        .map_err(|error| format!("{error:#}"));
    AttachmentEvent { key, result }
}

async fn load_image_inner(
    manager: Manager<SqliteStore, Registered>,
    cache: AttachmentCache,
    attachment: &ChatAttachment,
) -> Result<DynamicImage> {
    let key = attachment.key.clone();
    let cached = tokio::task::spawn_blocking({
        let cache = cache.clone();
        let key = key.clone();
        move || cache.read(&key)
    })
    .await
    .context("join attachment cache read")??;

    if let Some(bytes) = cached {
        match decode_image(bytes).await {
            Ok(image) => return Ok(image),
            Err(_) => {
                let cache = cache.clone();
                let stale_key = key.clone();
                let _ = tokio::task::spawn_blocking(move || cache.remove(&stale_key)).await;
            }
        }
    }

    let bytes = manager
        .get_attachment(&attachment.pointer)
        .await
        .context("download Signal attachment")?;
    if bytes.len() > MAX_ATTACHMENT_BYTES {
        bail!("image exceeds the 50 MiB preview limit");
    }
    let image = decode_image(bytes.clone()).await?;
    tokio::task::spawn_blocking(move || cache.write(&key, &bytes))
        .await
        .context("join attachment cache write")??;
    Ok(image)
}

async fn decode_image(bytes: Vec<u8>) -> Result<DynamicImage> {
    tokio::task::spawn_blocking(move || decode_image_blocking(&bytes))
        .await
        .context("join image decoder")?
}

fn decode_image_blocking(bytes: &[u8]) -> Result<DynamicImage> {
    if bytes.len() > MAX_ATTACHMENT_BYTES {
        bail!("image exceeds the 50 MiB preview limit");
    }
    let dimensions = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .context("detect image format")?
        .into_dimensions()
        .context("read image dimensions")?;
    if u64::from(dimensions.0) * u64::from(dimensions.1) > MAX_IMAGE_PIXELS {
        bail!("image exceeds the 40 megapixel preview limit");
    }
    ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .context("detect image format")?
        .decode()
        .context("decode image")
}

#[cfg(unix)]
fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::os::unix::fs::OpenOptionsExt;

    let mut file = fs::OpenOptions::new()
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
    fs::write(path, bytes)?;
    Ok(())
}

#[cfg(unix)]
fn protect_directory(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

#[cfg(not(unix))]
fn protect_directory(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
fn protect_file(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    Ok(())
}

#[cfg(not(unix))]
fn protect_file(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{decode_image_blocking, AttachmentCache};

    #[test]
    fn cache_round_trips_bytes() {
        let directory = tempfile::tempdir().unwrap();
        let cache = AttachmentCache::new(directory.path().join("attachments"));
        cache.write("aabb", b"image").unwrap();
        assert_eq!(cache.read("aabb").unwrap(), Some(b"image".to_vec()));
        assert!(!cache.path().join(".aabb.tmp").exists());
    }

    #[cfg(unix)]
    #[test]
    fn cache_is_private() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().unwrap();
        let cache = AttachmentCache::new(directory.path().join("attachments"));
        cache.write("aabb", b"image").unwrap();
        assert_eq!(
            std::fs::metadata(cache.path())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::metadata(cache.path().join("aabb"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }

    #[test]
    fn malformed_images_are_rejected() {
        assert!(decode_image_blocking(b"not an image").is_err());
    }

    #[test]
    fn cache_evicts_oldest_files_to_its_limit() {
        let directory = tempfile::tempdir().unwrap();
        let cache = AttachmentCache::new(directory.path().join("attachments"));
        cache.write("aaaa", b"old!").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        cache.write("bbbb", b"new!").unwrap();
        cache.evict_to_limit(4).unwrap();
        assert_eq!(cache.read("aaaa").unwrap(), None);
        assert_eq!(cache.read("bbbb").unwrap(), Some(b"new!".to_vec()));
    }
}
