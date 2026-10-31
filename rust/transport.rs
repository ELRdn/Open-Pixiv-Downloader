use crate::error::{invalid, Error, Result};
use base64::Engine;
use reqwest::blocking::{Client as Http, Response};
use serde_json::Value;
use std::{
    io::{Read, Write},
    path::Path,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};
use url::Url;

pub const DEMO: &str = include_str!("../examples/synthetic-fixture.json");
pub trait Transport: Send + Sync {
    fn api(&self, path: &str, query: &[(&str, String)]) -> Result<Value>;
    fn download(&self, url: &str, path: &Path) -> Result<()>;
    fn requests(&self) -> usize;
    fn stopped(&self) -> bool;
    fn cancel(&self);
}

pub fn validate_url(value: &str, asset: bool) -> Result<Url> {
    let u = Url::parse(value).map_err(|_| invalid("Invalid remote URL"))?;
    let host = u.host_str().unwrap_or("");
    let allowed = if asset {
        host == "pximg.net" || host.ends_with(".pximg.net")
    } else {
        host == "www.pixiv.net"
    };
    if !allowed
        || u.scheme() != "https"
        || u.port_or_known_default() != Some(443)
        || !u.username().is_empty()
        || u.password().is_some()
        || value.contains('\\')
        || value.chars().any(char::is_control)
    {
        return Err(invalid("Remote URL is outside the allowed HTTPS origin"));
    }
    Ok(u)
}
fn api_key(path: &str, query: &[(&str, String)]) -> Result<String> {
    if !path.starts_with("/ajax/") || path.contains(['?', '#']) || path.contains("..") {
        return Err(invalid("Invalid API path"));
    }
    let mut key = path.to_owned();
    if !query.is_empty() {
        key.push('?');
        key.push_str(
            &url::form_urlencoded::Serializer::new(String::new())
                .extend_pairs(query.iter().map(|(k, v)| (*k, v)))
                .finish(),
        );
    }
    Ok(key)
}

pub struct Fixture {
    data: Value,
    count: AtomicUsize,
    stop: Arc<AtomicBool>,
}
impl Fixture {
    pub fn new(source: &str, stop: Arc<AtomicBool>) -> Result<Self> {
        let data: Value = serde_json::from_str(source)?;
        if data["synthetic"] != true
            || !data["responses"].is_object()
            || !data["assets"].is_object()
        {
            return Err(invalid("Expected a synthetic fixture"));
        }
        Ok(Self {
            data,
            count: AtomicUsize::new(0),
            stop,
        })
    }
    fn admit(&self) -> Result<()> {
        if self.stopped() {
            return Err(Error::Stop("Stopped by user".into()));
        }
        self.count.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}
impl Transport for Fixture {
    fn api(&self, path: &str, query: &[(&str, String)]) -> Result<Value> {
        self.admit()?;
        self.data["responses"]
            .get(api_key(path, query)?)
            .cloned()
            .ok_or_else(|| Error::Unavailable("Fixture response unavailable".into()))
    }
    fn download(&self, url: &str, path: &Path) -> Result<()> {
        validate_url(url, true)?;
        self.admit()?;
        let source = self.data["assets"][url]
            .as_str()
            .ok_or_else(|| Error::Unavailable("Fixture asset unavailable".into()))?;
        let data = base64::engine::general_purpose::STANDARD
            .decode(source)
            .map_err(|_| invalid("Invalid base64 fixture asset"))?;
        crate::archive::atomic(path, &data)
    }
    fn requests(&self) -> usize {
        self.count.load(Ordering::SeqCst)
    }
    fn stopped(&self) -> bool {
        self.stop.load(Ordering::SeqCst)
    }
    fn cancel(&self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

struct Admission {
    last: Option<Instant>,
    count: usize,
}
pub struct Online {
    http: Http,
    gate: Mutex<Admission>,
    stop: Arc<AtomicBool>,
    interval: Duration,
    budget: usize,
    max_bytes: u64,
}
impl Online {
    pub fn new(
        interval: f64,
        budget: usize,
        max_bytes: u64,
        stop: Arc<AtomicBool>,
    ) -> Result<Self> {
        if !interval.is_finite()
            || !(2.0..=3600.0).contains(&interval)
            || !(1..=5000).contains(&budget)
        {
            return Err(invalid("Use interval 2–3600 seconds and budget 1–5000"));
        }
        let http = Http::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(30))
            .user_agent(concat!(
                "OpenPixivDownloader/",
                env!("CARGO_PKG_VERSION"),
                " (local archival research)"
            ))
            .build()
            .map_err(|_| invalid("Unable to initialize HTTPS transport"))?;
        Ok(Self {
            http,
            gate: Mutex::new(Admission {
                last: None,
                count: 0,
            }),
            stop,
            interval: Duration::from_secs_f64(interval),
            budget,
            max_bytes,
        })
    }
    fn stop_error(&self, message: &str) -> Error {
        self.cancel();
        Error::Stop(message.into())
    }
    fn check_stop(&self) -> Result<()> {
        if self.stopped() {
            Err(Error::Stop(
                "Job stopped; completed works remain saved".into(),
            ))
        } else {
            Ok(())
        }
    }
    fn open(&self, value: &str, asset: bool) -> Result<Response> {
        let url = validate_url(value, asset)?;
        self.open_validated(url, asset)
    }
    fn open_validated(&self, url: Url, asset: bool) -> Result<Response> {
        // Headers/admission are serialized. Bodies can transfer concurrently.
        let mut gate = self.gate.lock().unwrap();
        self.check_stop()?;
        if gate.count >= self.budget {
            return Err(self.stop_error("Request budget reached"));
        }
        if let Some(last) = gate.last {
            while last.elapsed() < self.interval {
                self.check_stop()?;
                std::thread::sleep(
                    self.interval
                        .saturating_sub(last.elapsed())
                        .min(Duration::from_millis(50)),
                );
            }
        }
        self.check_stop()?;
        gate.last = Some(Instant::now());
        gate.count += 1;
        let response = self
            .http
            .get(url)
            .header("Referer", "https://www.pixiv.net/")
            .header("Accept-Encoding", "identity")
            .header("Accept", if asset { "*/*" } else { "application/json" })
            .send()
            .map_err(|_| invalid("HTTPS request failed; retry the run later"))?;
        match response.status().as_u16() {
            401 | 403 | 429 => Err(self.stop_error(&format!(
                "HTTP {}; all requests stopped",
                response.status().as_u16()
            ))),
            300..=399 => {
                Err(self.stop_error("HTTP redirect received; inspect your browser before resuming"))
            }
            404 | 410 => Err(Error::Unavailable("Resource unavailable".into())),
            200 => Ok(response),
            _ => Err(invalid(&format!(
                "HTTP {}; request failed (no automatic retry)",
                response.status().as_u16()
            ))),
        }
    }
    fn read(
        &self,
        mut response: Response,
        mut output: impl Write,
        limit: u64,
        asset: bool,
    ) -> Result<()> {
        let expected = response.content_length();
        if expected.is_some_and(|n| n == 0 || n > limit) {
            return Err(invalid(
                "Response size is empty or exceeds the configured limit",
            ));
        }
        let ct = response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_ascii_lowercase();
        if ct.contains("text/html") || (asset && ct.contains("application/json")) {
            return Err(self.stop_error("Expected data; received a challenge or error document"));
        }
        let mut total = 0_u64;
        let mut buf = [0_u8; 65536];
        loop {
            self.check_stop()?;
            let n = response
                .read(&mut buf)
                .map_err(|_| invalid("Incomplete HTTP transfer; temporary data discarded"))?;
            if n == 0 {
                break;
            }
            if total == 0
                && String::from_utf8_lossy(&buf[..n])
                    .trim_start()
                    .to_ascii_lowercase()
                    .starts_with('<')
            {
                return Err(self.stop_error("Expected data; received HTML"));
            }
            total += n as u64;
            if total > limit {
                return Err(invalid("Response exceeds configured size limit"));
            }
            output.write_all(&buf[..n])?;
        }
        if total == 0 || expected.is_some_and(|n| n != total) {
            return Err(invalid("Incomplete HTTP transfer"));
        }
        Ok(())
    }
}
impl Transport for Online {
    fn api(&self, path: &str, query: &[(&str, String)]) -> Result<Value> {
        let response = self.open(
            &format!("https://www.pixiv.net{}", api_key(path, query)?),
            false,
        )?;
        let mut bytes = Vec::new();
        self.read(response, &mut bytes, 16 * 1024 * 1024, false)?;
        let value: Value = serde_json::from_slice(&bytes)
            .map_err(|_| self.stop_error("Expected JSON; incompatible response"))?;
        if !value.is_object() || value.get("error").is_none() || value.get("body").is_none() {
            return Err(invalid("API envelope changed"));
        }
        if value["error"] != false {
            return Err(Error::Unavailable(
                "API reports inaccessible resource".into(),
            ));
        }
        Ok(value["body"].clone())
    }
    fn download(&self, url: &str, path: &Path) -> Result<()> {
        let response = self.open(url, true)?;
        std::fs::create_dir_all(path.parent().unwrap())?;
        let mut file = tempfile::NamedTempFile::new_in(path.parent().unwrap())?;
        self.read(response, &mut file, self.max_bytes, true)?;
        file.as_file().sync_all()?;
        file.persist(path).map_err(|e| Error::Io(e.error))?;
        Ok(())
    }
    fn requests(&self) -> usize {
        self.gate.lock().unwrap().count
    }
    fn stopped(&self) -> bool {
        self.stop.load(Ordering::SeqCst)
    }
    fn cancel(&self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn test_server(
        status: u16,
        body: &str,
        requests: usize,
    ) -> (Url, std::thread::JoinHandle<Vec<Instant>>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
        let body = body.to_owned();
        let thread = std::thread::spawn(move || {
            let mut times = Vec::new();
            for stream in listener.incoming().take(requests) {
                let mut stream = stream.unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                let mut headers = Vec::new();
                let mut byte = [0];
                while !headers.ends_with(b"\r\n\r\n") {
                    stream.read_exact(&mut byte).unwrap();
                    headers.push(byte[0]);
                }
                times.push(Instant::now());
                write!(stream,"HTTP/1.1 {status} Test\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).unwrap();
            }
            times
        });
        (url, thread)
    }
    fn local_client(budget: usize) -> Online {
        Online {
            http: Http::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(Duration::from_secs(3))
                .build()
                .unwrap(),
            gate: Mutex::new(Admission {
                last: None,
                count: 0,
            }),
            stop: Arc::default(),
            interval: Duration::from_millis(30),
            budget,
            max_bytes: 1024,
        }
    }
    #[test]
    fn shared_request_interval_and_budget_over_real_sockets() {
        let (url, server) = test_server(200, "{}", 2);
        let client = Arc::new(local_client(2));
        let workers: Vec<_> = (0..2)
            .map(|_| {
                let client = client.clone();
                let url = url.clone();
                std::thread::spawn(move || client.open_validated(url, false).unwrap())
            })
            .collect();
        for worker in workers {
            worker.join().unwrap();
        }
        let times = server.join().unwrap();
        assert!(times[1].duration_since(times[0]) >= Duration::from_millis(25));
        assert!(matches!(
            client.open_validated(url, false),
            Err(Error::Stop(_))
        ));
        assert_eq!(client.requests(), 2);
    }
    #[test]
    fn rejection_latches_before_another_request() {
        for status in [302, 401, 403, 429] {
            let (url, server) = test_server(status, "blocked", 1);
            let client = local_client(10);
            assert!(matches!(
                client.open_validated(url.clone(), false),
                Err(Error::Stop(_))
            ));
            assert!(matches!(
                client.open_validated(url, false),
                Err(Error::Stop(_))
            ));
            assert_eq!(client.requests(), 1);
            server.join().unwrap();
        }
    }
    #[test]
    fn html_media_is_rejected_before_persisting() {
        let (url, server) = test_server(200, "<html>challenge</html>", 1);
        let client = local_client(10);
        let response = client.open_validated(url, true).unwrap();
        let mut bytes = Vec::new();
        assert!(matches!(
            client.read(response, &mut bytes, 1024, true),
            Err(Error::Stop(_))
        ));
        assert!(bytes.is_empty());
        assert!(client.stopped());
        server.join().unwrap();
    }
    #[test]
    fn strict_remote_origins() {
        for s in [
            "http://i.pximg.net/a.png",
            "https://pximg.net.evil/a.png",
            "https://u@i.pximg.net/a.png",
            "https://i.pximg.net:123/a.png",
        ] {
            assert!(validate_url(s, true).is_err());
        }
        assert!(validate_url("https://i.pximg.net/a.png", true).is_ok());
        assert!(validate_url("https://www.pixiv.net/ajax/illust/1", false).is_ok());
    }
    #[test]
    fn fixture_cancel_and_query_encoding() {
        let f = Fixture::new(DEMO, Arc::default()).unwrap();
        assert!(f
            .api(
                "/ajax/novel/1004/insert_illusts",
                &[("id[]", "1002-2".into())]
            )
            .is_ok());
        f.cancel();
        assert!(matches!(
            f.api("/ajax/illust/1001", &[]),
            Err(Error::Stop(_))
        ));
        assert_eq!(f.requests(), 1);
    }
}
