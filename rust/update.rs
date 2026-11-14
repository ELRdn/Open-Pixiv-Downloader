//! Fixed-source update checks and staged self replacement.
use crate::{
    error::{invalid, Result as AppResult},
    i18n::Language,
};
use reqwest::blocking::Client;
use semver::Version;
use serde::Deserialize;
use std::{
    io::Read,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::mpsc,
    thread,
    time::Duration,
};

pub const MANIFEST_URL: &str =
    "https://raw.githubusercontent.com/ELRdn/Open-Pixiv-Downloader/main/Cargo.toml";
pub const UPDATE_COMMAND: &str = "opixiv update";
const REPOSITORY: &str = "https://github.com/ELRdn/Open-Pixiv-Downloader.git";
const MAX_MANIFEST: usize = 64 * 1024;
pub const CHECK_INTERVAL: Duration = Duration::from_secs(3600);
pub const RETRY_INTERVAL: Duration = Duration::from_secs(60);

#[derive(Clone, Debug, PartialEq)]
pub enum Status {
    Unchecked,
    Checking,
    Current(Version),
    Available(Version),
    Failed(String),
}
impl Status {
    pub fn from_version(latest: Version) -> Self {
        let current = Version::parse(env!("CARGO_PKG_VERSION"))
            .expect("Cargo package version is valid SemVer");
        if latest.cmp_precedence(&current).is_gt() {
            Self::Available(latest)
        } else {
            Self::Current(latest)
        }
    }
    pub fn message(&self, language: Language) -> String {
        match self {
            Self::Unchecked => language
                .text("Updates not checked", "更新は未確認です")
                .into(),
            Self::Checking => language
                .text("Checking for updates…", "最新版を確認しています…")
                .into(),
            Self::Current(_) => language
                .text("No new updates available", "新しいアップデートはありません")
                .into(),
            Self::Available(v) => tr_format!(
                language,
                "Update available! v{v}",
                "アップデートが利用できます！ v{v}"
            ),
            Self::Failed(_) => language
                .text("Could not check for updates", "更新を確認できませんでした")
                .into(),
        }
    }
}

#[derive(Deserialize)]
struct Manifest {
    package: Package,
}
#[derive(Deserialize)]
struct Package {
    name: String,
    version: String,
}
fn parse_manifest(bytes: &[u8]) -> Result<Version, String> {
    if bytes.len() > MAX_MANIFEST {
        return Err("Update metadata exceeds the size limit.".into());
    }
    let text = std::str::from_utf8(bytes).map_err(|_| "Invalid update metadata encoding.")?;
    let manifest: Manifest = toml::from_str(text).map_err(|_| "Invalid update metadata format.")?;
    if manifest.package.name != "open-pixiv-downloader" {
        return Err("Update metadata package name does not match.".into());
    }
    Version::parse(&manifest.package.version).map_err(|_| "Invalid update metadata version.".into())
}

fn fetch(client: &Client, url: &str) -> Result<Version, String> {
    let response = client
        .get(url)
        .send()
        .map_err(|_| "Cannot connect to GitHub. Try again later.")?;
    if !response.status().is_success() {
        return Err(format!(
            "Cannot fetch update metadata from GitHub (HTTP {}).",
            response.status().as_u16()
        ));
    }
    if response
        .content_length()
        .is_some_and(|n| n > MAX_MANIFEST as u64)
    {
        return Err("Update metadata exceeds the size limit.".into());
    }
    let mut bytes = Vec::new();
    response
        .take(MAX_MANIFEST as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "Could not finish receiving update metadata.")?;
    parse_manifest(&bytes)
}
pub fn check() -> Result<Version, String> {
    let client = Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(3))
        .timeout(Duration::from_secs(5))
        .user_agent(concat!(
            "OpenPixivDownloader/",
            env!("CARGO_PKG_VERSION"),
            " (update check)"
        ))
        .build()
        .map_err(|_| "Cannot initialize the update check.")?;
    fetch(&client, MANIFEST_URL)
}
pub fn background() -> mpsc::Receiver<Result<Version, String>> {
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let _ = sender.send(check());
    });
    receiver
}
pub fn print_instructions(language: Language) {
    println!("{}", tr_format!(language, "\nTo update Open Pixiv Downloader, run this in your terminal:\n\n{UPDATE_COMMAND}\n\nCheck only: opixiv update --check\nAfter updating, launch with opixiv.", "\nOpen Pixiv Downloaderの更新：PowerShellで次を実行してください。\n\n{UPDATE_COMMAND}\n\n更新確認のみ: opixiv update --check\n更新後は opixiv で起動できます。"));
}

fn cargo_command(root: &Path) -> Command {
    let mut command = Command::new("cargo");
    command.args([
        "install", "--git", REPOSITORY, "--branch", "main", "--locked", "--force", "--bin",
        "opixiv", "--root",
    ]);
    command.arg(root);
    command
}
fn candidate_path(root: &Path) -> PathBuf {
    root.join("bin").join(if cfg!(windows) {
        "opixiv.exe"
    } else {
        "opixiv"
    })
}
fn binary_version(output: &[u8]) -> AppResult<Version> {
    let text = std::str::from_utf8(output)
        .map_err(|_| invalid("Cannot read the candidate version output."))?;
    let mut parts = text.split_whitespace();
    if parts.next() != Some("opixiv") {
        return Err(invalid("Candidate package name does not match."));
    }
    let version = parts
        .next()
        .ok_or_else(|| invalid("Candidate version output is missing."))?;
    if parts.next().is_some() {
        return Err(invalid("Invalid candidate version output format."));
    }
    Version::parse(version).map_err(|_| invalid("Invalid candidate version output format."))
}
fn validate_candidate(
    candidate: &Path,
    current: &Version,
    expected: Option<&Version>,
) -> AppResult<Version> {
    let metadata = std::fs::symlink_metadata(candidate)
        .map_err(|_| invalid("Cargo finished but the candidate executable is missing."))?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(invalid("The candidate executable is not a regular file."));
    }
    let directory = candidate
        .parent()
        .ok_or_else(|| invalid("Cannot determine the candidate folder."))?;
    let capture = tempfile::NamedTempFile::new_in(directory)?;
    let mut child = Command::new(candidate)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::from(capture.as_file().try_clone()?))
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| invalid("Cannot start the candidate to verify its version."))?;
    let started = std::time::Instant::now();
    let status = loop {
        if started.elapsed() >= Duration::from_secs(5) || capture.as_file().metadata()?.len() > 4096
        {
            let _ = child.kill();
            let _ = child.wait();
            return Err(invalid("Candidate verification exceeded its time or output limit. The current executable was preserved."));
        }
        if let Some(status) = child.try_wait()? {
            break status;
        }
        thread::sleep(Duration::from_millis(20));
    };
    if !status.success() {
        return Err(invalid("Candidate version verification failed."));
    }
    if capture.as_file().metadata()?.len() > 4096 {
        return Err(invalid("Candidate version output exceeds the size limit."));
    }
    let output = std::fs::read(capture.path())?;
    let version = binary_version(&output)?;
    if version.cmp_precedence(current).is_lt()
        || expected.is_some_and(|v| version.cmp_precedence(v).is_lt())
    {
        return Err(invalid(
            "The candidate is older than the current or expected version; replacement cancelled.",
        ));
    }
    Ok(version)
}
fn require_tool(program: &str) -> AppResult<()> {
    let output = Command::new(program)
        .arg("--version")
        .stdin(Stdio::null())
        .output()
        .map_err(|_| {
            invalid(&format!(
                "Cannot start {program}. Install Rust/Cargo and Git, then reopen your terminal."
            ))
        })?;
    if !output.status.success() {
        return Err(invalid(&format!(
            "Failed to check {program}. Verify your Rust/Cargo and Git installation."
        )));
    }
    Ok(())
}

fn restore_if_missing(original: &Path, backup: &Path) -> std::io::Result<()> {
    match std::fs::symlink_metadata(original) {
        Ok(_) => return Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    let directory = original
        .parent()
        .ok_or_else(|| std::io::Error::other("Missing executable directory"))?;
    let temp = tempfile::NamedTempFile::new_in(directory)?;
    std::fs::copy(backup, temp.path())?;
    temp.as_file().sync_all()?;
    temp.persist_noclobber(original).map_err(|e| e.error)?;
    Ok(())
}

/// Build in isolation before replacing the exact executable which was launched.
/// No shell is used, and manifest fields never become command arguments.
pub fn install(check_only: bool, force: bool) -> AppResult<i32> {
    let current = Version::parse(env!("CARGO_PKG_VERSION")).expect("Cargo version is valid SemVer");
    println!("Open Pixiv Downloader update\nCurrent: v{current}");
    let expected = if force {
        None
    } else {
        let latest=check().map_err(|message|invalid(&format!("{message}\nRetry: opixiv update\nReinstall without the version check: opixiv update --force")))?;
        if !latest.cmp_precedence(&current).is_gt() {
            println!("No new updates available (GitHub: v{latest}).");
            return Ok(0);
        }
        println!("Update available: v{latest}");
        Some(latest)
    };
    if check_only {
        return Ok(0);
    }
    require_tool("cargo")?;
    require_tool("git")?;
    let current_exe = std::env::current_exe()?.canonicalize()?;
    let directory = current_exe
        .parent()
        .ok_or_else(|| invalid("Cannot determine the executable folder."))?;
    let guard = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(directory.join(".opixiv-update.lock"))
        .map_err(|_| {
            invalid(
                "Cannot write to the update destination. Check the executable folder permissions.",
            )
        })?;
    guard.try_lock().map_err(|_| {
        invalid("Another update is running or this destination does not support update locks.")
    })?;
    let stage = tempfile::Builder::new()
        .prefix("opixiv-update-")
        .tempdir()?;
    println!("Building the update. This may take a few minutes…");
    let status = cargo_command(stage.path())
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .map_err(|_| invalid("Cannot start Cargo. The current executable was preserved."))?;
    if !status.success() {
        return Err(invalid("Update build failed. The current executable was preserved. Check the Cargo error and retry with opixiv update."));
    }
    let candidate = candidate_path(stage.path());
    let installed = validate_candidate(&candidate, &current, expected.as_ref())?;
    let backup = stage.path().join("previous-opixiv");
    std::fs::copy(&current_exe, &backup)?;
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&backup)?
        .sync_all()?;
    if let Err(error) = self_replace::self_replace(&candidate) {
        // On Windows the library moves the loaded image before installing the new
        // file. Restore a missing public path if a later copy/rename failed.
        if let Err(restore) = restore_if_missing(&current_exe, &backup) {
            let retained = stage.keep();
            return Err(invalid(&format!("Executable replacement and recovery both failed: {error}; {restore}\nBackup retained at: {}",retained.display())));
        }
        return Err(invalid(&format!("Cannot replace the executable: {error}\nThe original was preserved or restored. Close other opixiv instances and check destination permissions.")));
    }
    println!("Updated to v{installed}\nLaunch with opixiv next time.");
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn binary_version_identity_and_precedence_are_checked() {
        assert_eq!(
            binary_version(b"opixiv 0.2.0-alpha.10\n").unwrap(),
            Version::parse("0.2.0-alpha.10").unwrap()
        );
        for bytes in [
            b"other 1.0.0".as_slice(),
            b"opixiv not-a-version",
            b"opixiv 1.0.0 extra",
            b"",
        ] {
            assert!(binary_version(bytes).is_err());
        }
    }
    #[test]
    fn cargo_arguments_are_fixed_and_staging_path_is_one_argument() {
        let path = Path::new("space 日本語 & $ folder");
        let command = cargo_command(path);
        assert_eq!(command.get_program(), "cargo");
        let args: Vec<_> = command.get_args().collect();
        assert_eq!(args.last().unwrap(), &path.as_os_str());
        assert_eq!(args.iter().filter(|a| **a == "--root").count(), 1);
        assert!(args.iter().any(|a| a.to_str() == Some(REPOSITORY)));
    }
    #[test]
    fn rollback_restores_missing_executable_without_overwriting_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let original = dir.path().join("opixiv");
        let backup = dir.path().join("backup");
        std::fs::write(&backup, b"working previous binary").unwrap();
        restore_if_missing(&original, &backup).unwrap();
        assert_eq!(
            std::fs::read(&original).unwrap(),
            b"working previous binary"
        );
        std::fs::write(&original, b"another installation").unwrap();
        restore_if_missing(&original, &backup).unwrap();
        assert_eq!(std::fs::read(&original).unwrap(), b"another installation");
    }
    fn manifest(version: &str) -> String {
        format!("[package]\nname = \"open-pixiv-downloader\"\nversion = \"{version}\"\n")
    }
    #[test]
    fn semver_and_identity_do_not_accept_malformed_update_data() {
        assert_eq!(
            parse_manifest(manifest("0.2.0-alpha.10").as_bytes()).unwrap(),
            Version::parse("0.2.0-alpha.10").unwrap()
        );
        for data in [
            manifest("not-a-version"),
            manifest("0.2.0").replace("open-pixiv-downloader", "other-project"),
            "<html>denied</html>".into(),
            "x".repeat(MAX_MANIFEST + 1),
        ] {
            assert!(parse_manifest(data.as_bytes()).is_err());
        }
        assert!(
            Version::parse("0.2.0-alpha.10").unwrap() > Version::parse("0.2.0-alpha.9").unwrap()
        );
        assert!(matches!(
            Status::from_version(Version::parse("99.0.0").unwrap()),
            Status::Available(_)
        ));
        assert!(matches!(
            Status::from_version(Version::parse(env!("CARGO_PKG_VERSION")).unwrap()),
            Status::Current(_)
        ));
        assert!(matches!(
            Status::from_version(Version::parse("0.1.0").unwrap()),
            Status::Current(_)
        ));
        let metadata =
            Version::parse(&format!("{}+other-build", env!("CARGO_PKG_VERSION"))).unwrap();
        assert!(matches!(Status::from_version(metadata), Status::Current(_)));
    }
    #[test]
    fn real_http_success_denial_and_redirect_are_bounded() {
        use std::{io::Write, net::TcpListener};
        for status in [200, 302, 403] {
            let server = TcpListener::bind("127.0.0.1:0").unwrap();
            let url = format!("http://{}/Cargo.toml", server.local_addr().unwrap());
            let server = thread::spawn(move || {
                let (mut stream, _) = server.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                let mut byte = [0];
                let mut headers = Vec::new();
                while !headers.ends_with(b"\r\n\r\n") {
                    stream.read_exact(&mut byte).unwrap();
                    headers.push(byte[0]);
                }
                let body = manifest("99.0.0");
                write!(stream,"HTTP/1.1 {status} Test\r\nLocation: /redirect-must-not-be-followed\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).unwrap();
            });
            let client = Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(Duration::from_secs(3))
                .build()
                .unwrap();
            let result = fetch(&client, &url);
            assert_eq!(result.is_ok(), status == 200);
            server.join().unwrap();
        }
    }
}
