//! Advisory update checks. Remote data never becomes an executable command.
use reqwest::blocking::Client;
use semver::Version;
use serde::Deserialize;
use std::{io::Read, sync::mpsc, thread, time::Duration};

pub const MANIFEST_URL: &str =
    "https://raw.githubusercontent.com/ELRdn/Open-Pixiv-Downloader/main/Cargo.toml";
pub const INSTALL_COMMAND:&str="cargo install --git https://github.com/ELRdn/Open-Pixiv-Downloader.git --branch main --locked --force";
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
    pub fn message(&self) -> String {
        match self {
            Self::Unchecked => "更新は未確認です".into(),
            Self::Checking => "最新版を確認しています…".into(),
            Self::Current(_) => "新しいアップデートはありません".into(),
            Self::Available(v) => format!("アップデートが利用できます！ v{v}"),
            Self::Failed(_) => "更新を確認できませんでした".into(),
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
        return Err("更新情報のサイズが上限を超えました。".into());
    }
    let text = std::str::from_utf8(bytes).map_err(|_| "更新情報の文字コードを確認できません。")?;
    let manifest: Manifest =
        toml::from_str(text).map_err(|_| "更新情報の形式を確認できません。")?;
    if manifest.package.name != "open-pixiv-downloader" {
        return Err("更新情報の製品名が一致しません。".into());
    }
    Version::parse(&manifest.package.version)
        .map_err(|_| "更新情報のバージョンを確認できません。".into())
}

fn fetch(client: &Client, url: &str) -> Result<Version, String> {
    let response = client
        .get(url)
        .send()
        .map_err(|_| "GitHubに接続できません。F8で再確認できます。")?;
    if !response.status().is_success() {
        return Err(format!(
            "GitHubの更新情報を取得できません（HTTP {}）。",
            response.status().as_u16()
        ));
    }
    if response
        .content_length()
        .is_some_and(|n| n > MAX_MANIFEST as u64)
    {
        return Err("更新情報のサイズが上限を超えました。".into());
    }
    let mut bytes = Vec::new();
    response
        .take(MAX_MANIFEST as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "更新情報の受信が完了しませんでした。")?;
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
        .map_err(|_| "更新確認を初期化できません。")?;
    fetch(&client, MANIFEST_URL)
}
pub fn background() -> mpsc::Receiver<Result<Version, String>> {
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let _ = sender.send(check());
    });
    receiver
}
pub fn print_instructions() {
    println!("\nOpen Pixiv Downloaderの更新：PowerShellで次の1行を実行してください。\n\n{INSTALL_COMMAND}\n\n更新後は opixiv で起動できます。");
}

#[cfg(test)]
mod tests {
    use super::*;
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
