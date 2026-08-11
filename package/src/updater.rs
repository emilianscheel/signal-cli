use std::{
    collections::HashMap,
    fs::{File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

use anyhow::{bail, Context, Result};
use futures::StreamExt;
use semver::Version;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::process::{Child, Command};

const MANAGED_MARKER: &str = ".signal-managed-install";
const UPDATE_LOCK: &str = ".signal-update.lock";
const SUCCESS_MARKER: &str = ".signal-update-success.json";
const LATEST_RELEASE: &str = "https://github.com/emilianscheel/signal-cli/releases/latest/download";
const MAX_MANIFEST_BYTES: u64 = 64 * 1024;
const MAX_ARTIFACT_BYTES: u64 = 512 * 1024 * 1024;

#[derive(Debug, Deserialize)]
struct ReleaseManifest {
    schema: u32,
    version: Version,
    artifacts: HashMap<String, ReleaseArtifact>,
}

#[derive(Debug, Deserialize)]
struct ReleaseArtifact {
    name: String,
    sha256: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct SuccessMarker {
    version: Version,
}

#[derive(Debug)]
struct ManagedInstall {
    executable: PathBuf,
    directory: PathBuf,
}

impl ManagedInstall {
    fn detect() -> Option<Self> {
        let executable = std::env::current_exe().ok()?.canonicalize().ok()?;
        Self::from_executable(executable)
    }

    fn from_executable(executable: PathBuf) -> Option<Self> {
        let directory = executable.parent()?.to_path_buf();
        directory.join(MANAGED_MARKER).is_file().then_some(Self {
            executable,
            directory,
        })
    }

    fn lock_path(&self) -> PathBuf {
        self.directory.join(UPDATE_LOCK)
    }

    fn success_path(&self) -> PathBuf {
        self.directory.join(SUCCESS_MARKER)
    }
}

struct UpdateLock {
    _file: File,
}

impl UpdateLock {
    fn acquire(path: PathBuf) -> Result<Option<Self>> {
        use std::os::{fd::AsRawFd, unix::fs::OpenOptionsExt};

        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(path)
            .context("open update lock")?;
        // The kernel releases this advisory lock if a helper exits or crashes,
        // so a persistent lock file can never strand future updates.
        let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if result == 0 {
            Ok(Some(Self { _file: file }))
        } else {
            let error = std::io::Error::last_os_error();
            match error.raw_os_error() {
                Some(code) if code == libc::EWOULDBLOCK || code == libc::EAGAIN => Ok(None),
                _ => Err(error).context("lock updater"),
            }
        }
    }
}

struct TemporaryDownload {
    path: PathBuf,
    committed: bool,
}

impl TemporaryDownload {
    fn new(directory: &Path) -> Result<(Self, File)> {
        for attempt in 0..100_u32 {
            let path = directory.join(format!(".signal-update-{}-{attempt}", std::process::id()));
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(file) => {
                    return Ok((
                        Self {
                            path,
                            committed: false,
                        },
                        file,
                    ));
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error).context("create temporary update file"),
            }
        }
        bail!("could not allocate a temporary update file")
    }
}

impl Drop for TemporaryDownload {
    fn drop(&mut self) {
        if !self.committed {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

pub struct UpdateMonitor {
    child: Child,
}

impl UpdateMonitor {
    pub async fn wait(mut self) -> Option<String> {
        let status = self.child.wait().await.ok()?;
        status.success().then(take_success_notice).flatten()
    }
}

fn updates_disabled() -> bool {
    updates_disabled_value(std::env::var_os("SIGNAL_NO_UPDATE").as_deref())
}

fn updates_disabled_value(value: Option<&std::ffi::OsStr>) -> bool {
    value.is_some_and(|value| value == "1")
}

fn release_target() -> Option<&'static str> {
    match (std::env::consts::ARCH, std::env::consts::OS) {
        ("x86_64", "macos") => Some("x86_64-apple-darwin"),
        ("aarch64", "macos") => Some("aarch64-apple-darwin"),
        ("x86_64", "linux") => Some("x86_64-unknown-linux-gnu"),
        ("aarch64", "linux") => Some("aarch64-unknown-linux-gnu"),
        _ => None,
    }
}

fn validate_manifest(
    manifest: ReleaseManifest,
    current: &Version,
    target: &str,
) -> Result<Option<(Version, ReleaseArtifact)>> {
    if manifest.schema != 1 {
        bail!("unsupported release manifest schema {}", manifest.schema);
    }
    if !manifest.version.pre.is_empty() || manifest.version <= *current {
        return Ok(None);
    }
    let artifact = manifest
        .artifacts
        .into_iter()
        .find_map(|(key, artifact)| (key == target).then_some(artifact))
        .with_context(|| format!("release has no artifact for {target}"))?;
    let expected_name = format!("signal-{target}");
    if artifact.name != expected_name {
        bail!("unexpected artifact name {:?}", artifact.name);
    }
    if artifact.sha256.len() != 64 || !artifact.sha256.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        bail!("invalid artifact checksum")
    }
    Ok(Some((manifest.version, artifact)))
}

pub fn take_success_notice() -> Option<String> {
    let install = ManagedInstall::detect()?;
    consume_success_marker(
        &install.success_path(),
        &Version::parse(env!("CARGO_PKG_VERSION")).ok()?,
    )
}

fn consume_success_marker(path: &Path, current: &Version) -> Option<String> {
    let bytes = std::fs::read(path).ok()?;
    let _ = std::fs::remove_file(path);
    let marker: SuccessMarker = serde_json::from_slice(&bytes).ok()?;
    if marker.version == *current {
        Some(format!("Updated to v{}", marker.version))
    } else {
        Some(format!(
            "Updated to v{} — restart to use it",
            marker.version
        ))
    }
}

pub fn spawn_helper() -> Option<UpdateMonitor> {
    if updates_disabled() || ManagedInstall::detect().is_none() || release_target().is_none() {
        return None;
    }
    let executable = std::env::current_exe().ok()?;
    let child = Command::new(executable)
        .arg("--internal-update")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(false)
        .spawn()
        .ok()?;
    Some(UpdateMonitor { child })
}

pub async fn run_helper() -> Result<()> {
    if updates_disabled() {
        return Ok(());
    }
    let Some(install) = ManagedInstall::detect() else {
        return Ok(());
    };
    let Some(target) = release_target() else {
        return Ok(());
    };
    let Some(_lock) = UpdateLock::acquire(install.lock_path())? else {
        return Ok(());
    };

    let client = reqwest::Client::builder()
        .user_agent(concat!("signal/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(5 * 60))
        .build()?;
    let current = Version::parse(env!("CARGO_PKG_VERSION"))?;
    update_from(&client, &install, target, &current, LATEST_RELEASE).await
}

async fn update_from(
    client: &reqwest::Client,
    install: &ManagedInstall,
    target: &str,
    current: &Version,
    latest_release: &str,
) -> Result<()> {
    let manifest_response = client
        .get(format!("{latest_release}/release.json"))
        .send()
        .await?
        .error_for_status()?;
    if manifest_response
        .content_length()
        .is_some_and(|length| length > MAX_MANIFEST_BYTES)
    {
        bail!("release manifest is too large")
    }
    let manifest_bytes = manifest_response.bytes().await?;
    if manifest_bytes.len() as u64 > MAX_MANIFEST_BYTES {
        bail!("release manifest is too large")
    }
    let manifest = serde_json::from_slice::<ReleaseManifest>(&manifest_bytes)?;
    let Some((version, artifact)) = validate_manifest(manifest, current, target)? else {
        return Ok(());
    };

    let response = client
        .get(format!("{latest_release}/{}", artifact.name))
        .send()
        .await?
        .error_for_status()?;
    if response
        .content_length()
        .is_some_and(|length| length > MAX_ARTIFACT_BYTES)
    {
        bail!("release artifact is too large")
    }
    let (mut temporary, mut file) = TemporaryDownload::new(&install.directory)?;
    let mut hasher = Sha256::new();
    let mut stream = response.bytes_stream();
    let mut downloaded = 0_u64;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        downloaded = downloaded.saturating_add(chunk.len() as u64);
        if downloaded > MAX_ARTIFACT_BYTES {
            bail!("release artifact is too large")
        }
        hasher.update(&chunk);
        file.write_all(&chunk)?;
    }
    file.sync_all()?;
    drop(file);

    let actual_checksum = hex::encode(hasher.finalize());
    if !actual_checksum.eq_ignore_ascii_case(&artifact.sha256) {
        bail!("downloaded artifact checksum does not match release manifest")
    }
    set_executable(&temporary.path)?;
    verify_version(&temporary.path, &version).await?;
    std::fs::rename(&temporary.path, &install.executable).context("atomically install update")?;
    temporary.committed = true;
    File::open(&install.directory)?.sync_all()?;
    write_success_marker(install, &version)?;
    Ok(())
}

#[cfg(unix)]
fn set_executable(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))?;
    Ok(())
}

async fn verify_version(path: &Path, version: &Version) -> Result<()> {
    let output = Command::new(path)
        .arg("--version")
        .stdin(Stdio::null())
        .output()
        .await
        .context("run downloaded binary")?;
    if !output.status.success() {
        bail!("downloaded binary failed its version check")
    }
    let stdout = String::from_utf8(output.stdout)?;
    if stdout.split_whitespace().last() != Some(version.to_string().as_str()) {
        bail!("downloaded binary reports an unexpected version")
    }
    Ok(())
}

fn write_success_marker(install: &ManagedInstall, version: &Version) -> Result<()> {
    let bytes = serde_json::to_vec(&SuccessMarker {
        version: version.clone(),
    })?;
    let (mut temporary, mut file) = TemporaryDownload::new(&install.directory)?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    drop(file);
    std::fs::rename(&temporary.path, install.success_path())?;
    temporary.committed = true;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        consume_success_marker, update_from, updates_disabled_value, validate_manifest,
        ManagedInstall, ReleaseArtifact, ReleaseManifest, SuccessMarker, UpdateLock,
        MANAGED_MARKER,
    };
    use semver::Version;
    use sha2::{Digest, Sha256};
    use std::{
        collections::HashMap,
        ffi::OsStr,
        io::{Read, Write},
        net::TcpListener,
        sync::Arc,
    };

    fn manifest(version: &str, target: &str) -> ReleaseManifest {
        ReleaseManifest {
            schema: 1,
            version: Version::parse(version).unwrap(),
            artifacts: HashMap::from([(
                target.into(),
                ReleaseArtifact {
                    name: format!("signal-{target}"),
                    sha256: "a".repeat(64),
                },
            )]),
        }
    }

    #[test]
    fn accepts_only_newer_stable_versions() {
        let current = Version::parse("1.2.3").unwrap();
        assert!(
            validate_manifest(manifest("1.2.4", "target"), &current, "target")
                .unwrap()
                .is_some()
        );
        assert!(
            validate_manifest(manifest("1.2.3", "target"), &current, "target")
                .unwrap()
                .is_none()
        );
        assert!(
            validate_manifest(manifest("2.0.0-alpha.1", "target"), &current, "target")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn rejects_bad_artifact_metadata() {
        let current = Version::parse("1.0.0").unwrap();
        let mut release = manifest("1.1.0", "target");
        release.artifacts.get_mut("target").unwrap().name = "../signal".into();
        assert!(validate_manifest(release, &current, "target").is_err());

        let mut release = manifest("1.1.0", "target");
        release.artifacts.get_mut("target").unwrap().sha256 = "not-a-checksum".into();
        assert!(validate_manifest(release, &current, "target").is_err());
    }

    #[test]
    fn lock_excludes_a_concurrent_updater() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("lock");
        let first = UpdateLock::acquire(path.clone()).unwrap().unwrap();
        assert!(UpdateLock::acquire(path.clone()).unwrap().is_none());
        drop(first);
        assert!(UpdateLock::acquire(path).unwrap().is_some());
    }

    #[test]
    fn abandoned_lock_file_does_not_block_updates() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("lock");
        std::fs::write(&path, "old process").unwrap();
        assert!(UpdateLock::acquire(path).unwrap().is_some());
    }

    #[test]
    fn detects_only_marked_managed_installations() {
        let directory = tempfile::tempdir().unwrap();
        let executable = directory.path().join("signal");
        std::fs::write(&executable, []).unwrap();
        assert!(ManagedInstall::from_executable(executable.clone()).is_none());
        std::fs::write(directory.path().join(MANAGED_MARKER), []).unwrap();
        assert!(ManagedInstall::from_executable(executable).is_some());
    }

    #[test]
    fn opt_out_requires_the_documented_value() {
        assert!(updates_disabled_value(Some(OsStr::new("1"))));
        assert!(!updates_disabled_value(Some(OsStr::new("true"))));
        assert!(!updates_disabled_value(None));
    }

    #[test]
    fn success_notice_is_consumed_once() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("success.json");
        std::fs::write(
            &path,
            serde_json::to_vec(&SuccessMarker {
                version: Version::parse("2.0.0").unwrap(),
            })
            .unwrap(),
        )
        .unwrap();
        assert_eq!(
            consume_success_marker(&path, &Version::parse("1.0.0").unwrap()),
            Some("Updated to v2.0.0 — restart to use it".into())
        );
        assert!(!path.exists());
        assert_eq!(
            consume_success_marker(&path, &Version::parse("1.0.0").unwrap()),
            None
        );
    }

    fn serve_release(manifest: Vec<u8>, artifact: Vec<u8>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let manifest = Arc::new(manifest);
        let artifact = Arc::new(artifact);
        std::thread::spawn(move || {
            for _ in 0..2 {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = [0_u8; 4096];
                let length = stream.read(&mut request).unwrap();
                let request = String::from_utf8_lossy(&request[..length]);
                let body = if request.starts_with("GET /release.json ") {
                    &manifest
                } else {
                    &artifact
                };
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                )
                .unwrap();
                stream.write_all(body).unwrap();
            }
        });
        format!("http://{address}")
    }

    fn test_install(directory: &std::path::Path) -> ManagedInstall {
        let executable = directory.join("signal");
        std::fs::write(&executable, "old binary").unwrap();
        std::fs::write(directory.join(MANAGED_MARKER), []).unwrap();
        ManagedInstall::from_executable(executable).unwrap()
    }

    fn release_json(target: &str, artifact: &[u8], checksum: &str) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "schema": 1,
            "version": "9.0.0",
            "artifacts": {
                (target): {
                    "name": format!("signal-{target}"),
                    "sha256": if checksum.is_empty() {
                        hex::encode(Sha256::digest(artifact))
                    } else {
                        checksum.into()
                    }
                }
            }
        }))
        .unwrap()
    }

    #[tokio::test]
    async fn valid_download_atomically_replaces_the_managed_binary() {
        let directory = tempfile::tempdir().unwrap();
        let install = test_install(directory.path());
        let target = "test-target";
        let artifact = b"#!/bin/sh\nprintf 'signal 9.0.0\\n'\n".to_vec();
        let server = serve_release(release_json(target, &artifact, ""), artifact.clone());

        update_from(
            &reqwest::Client::new(),
            &install,
            target,
            &Version::parse("1.0.0").unwrap(),
            &server,
        )
        .await
        .unwrap();

        assert_eq!(std::fs::read(&install.executable).unwrap(), artifact);
        assert!(install.success_path().is_file());
    }

    #[tokio::test]
    async fn checksum_failure_preserves_the_existing_binary() {
        let directory = tempfile::tempdir().unwrap();
        let install = test_install(directory.path());
        let target = "test-target";
        let artifact = b"#!/bin/sh\nprintf 'signal 9.0.0\\n'\n".to_vec();
        let server = serve_release(release_json(target, &artifact, &"0".repeat(64)), artifact);

        assert!(update_from(
            &reqwest::Client::new(),
            &install,
            target,
            &Version::parse("1.0.0").unwrap(),
            &server,
        )
        .await
        .is_err());

        assert_eq!(
            std::fs::read_to_string(&install.executable).unwrap(),
            "old binary"
        );
        assert!(!install.success_path().exists());
    }

    #[tokio::test]
    async fn reported_version_mismatch_preserves_the_existing_binary() {
        let directory = tempfile::tempdir().unwrap();
        let install = test_install(directory.path());
        let target = "test-target";
        let artifact = b"#!/bin/sh\nprintf 'signal 8.0.0\\n'\n".to_vec();
        let server = serve_release(release_json(target, &artifact, ""), artifact);

        assert!(update_from(
            &reqwest::Client::new(),
            &install,
            target,
            &Version::parse("1.0.0").unwrap(),
            &server,
        )
        .await
        .is_err());

        assert_eq!(
            std::fs::read_to_string(&install.executable).unwrap(),
            "old binary"
        );
        assert!(!install.success_path().exists());
    }
}
