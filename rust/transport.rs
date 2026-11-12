use crate::error::{invalid, Error, Result, StopReason};
use base64::Engine;
use reqwest::blocking::{Client as Http, Response};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, VecDeque},
    io::{Read, Write},
    path::Path,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Condvar, Mutex,
    },
    time::{Duration, Instant},
};
use url::Url;

pub const DEMO: &str = include_str!("../examples/synthetic-fixture.json");
pub const MAX_REQUEST_BUDGET: usize = 1_000_000;
pub trait Transport: Send + Sync {
    fn api(&self, path: &str, query: &[(&str, String)]) -> Result<Value>;
    fn download(&self, url: &str, path: &Path) -> Result<()>;
    fn requests(&self) -> usize;
    /// Hard/user cancellation only. A spent request budget does not cancel bodies.
    fn stopped(&self) -> bool;
    fn cancel(&self);
    fn exhausted(&self) -> bool {
        false
    }
    fn stop_reason(&self) -> Option<StopReason> {
        if self.stopped() {
            Some(StopReason::UserCancel)
        } else if self.exhausted() {
            Some(StopReason::RequestBudget)
        } else {
            None
        }
    }
    fn metrics(&self) -> Value {
        json!({})
    }
    fn rate_wait(&self) -> Duration {
        Duration::ZERO
    }
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
    api_count: AtomicUsize,
    media_count: AtomicUsize,
    stop: Arc<AtomicBool>,
    budget: usize,
}
impl Fixture {
    #[allow(dead_code)] // Kept for existing fixture integrations and isolated tests.
    pub fn new(source: &str, stop: Arc<AtomicBool>) -> Result<Self> {
        Self::with_budget(source, stop, usize::MAX)
    }
    pub fn with_budget(source: &str, stop: Arc<AtomicBool>, budget: usize) -> Result<Self> {
        let data: Value = serde_json::from_str(source)?;
        if data["synthetic"] != true
            || !data["responses"].is_object()
            || !data["assets"].is_object()
            || budget == 0
        {
            return Err(invalid(
                "Expected a synthetic fixture and positive request budget",
            ));
        }
        Ok(Self {
            data,
            count: 0.into(),
            api_count: 0.into(),
            media_count: 0.into(),
            stop,
            budget,
        })
    }
    fn admit(&self, asset: bool) -> Result<()> {
        if self.stopped() {
            return Err(Error::Stop("Stopped by user".into()));
        }
        let mut current = self.count.load(Ordering::SeqCst);
        loop {
            if current >= self.budget {
                return Err(Error::Budget);
            }
            match self.count.compare_exchange_weak(
                current,
                current + 1,
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(_) => break,
                Err(observed) => current = observed,
            }
        }
        if asset {
            &self.media_count
        } else {
            &self.api_count
        }
        .fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}
impl Transport for Fixture {
    fn api(&self, path: &str, query: &[(&str, String)]) -> Result<Value> {
        self.admit(false)?;
        self.data["responses"]
            .get(api_key(path, query)?)
            .cloned()
            .ok_or_else(|| Error::Unavailable("Fixture response unavailable".into()))
    }
    fn download(&self, url: &str, path: &Path) -> Result<()> {
        validate_url(url, true)?;
        self.admit(true)?;
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
    fn exhausted(&self) -> bool {
        self.requests() >= self.budget
    }
    fn metrics(&self) -> Value {
        json!({"api":{"requests":self.api_count.load(Ordering::SeqCst)},
               "media":{"requests":self.media_count.load(Ordering::SeqCst)},
               "request_budget":self.budget,"requests_remaining":self.budget.saturating_sub(self.requests())})
    }
}

#[derive(Clone, Default)]
struct ClassStats {
    requests: usize,
    completed: usize,
    bytes: u64,
    header_seconds: f64,
    transfer_seconds: f64,
    headers: Vec<f64>,
    transfers: Vec<f64>,
    headers_seen: usize,
    transfers_seen: usize,
    errors: BTreeMap<&'static str, usize>,
}
impl ClassStats {
    fn value(&self, elapsed: Duration) -> Value {
        fn percentile(values: &[f64], p: f64) -> Option<f64> {
            if values.is_empty() {
                return None;
            }
            let mut sorted = values.to_vec();
            sorted.sort_by(f64::total_cmp);
            Some(sorted[((sorted.len() - 1) as f64 * p).round() as usize])
        }
        json!({"requests":self.requests,"completed":self.completed,"bytes":self.bytes,
            "requests_per_second":self.requests as f64 / elapsed.as_secs_f64().max(0.001),
            "header_seconds":self.header_seconds,"transfer_seconds":self.transfer_seconds,
            "header_p50_seconds":percentile(&self.headers,0.5),"header_p95_seconds":percentile(&self.headers,0.95),
            "transfer_p50_seconds":percentile(&self.transfers,0.5),"transfer_p95_seconds":percentile(&self.transfers,0.95),
            "errors":self.errors,"timing_sample":"latest_2048_attempt_samples",
            "header_sample_count":self.headers.len(),"transfer_sample_count":self.transfers.len(),
            "header_samples_total":self.headers_seen,"transfer_samples_total":self.transfers_seen,
            "timing_sample_limit":2048})
    }
}
#[derive(Default)]
struct Admission {
    last: Option<Instant>,
    class_last: [Option<Instant>; 2],
    count: usize,
    active: [usize; 2],
    peak_active: [usize; 2],
    peak_total: usize,
    waiting: Duration,
    wait_end: Option<Instant>,
    admission_wait: Duration,
    next_ticket: u64,
    waiters: VecDeque<(u64, usize)>,
    reason: Option<StopReason>,
    stats: [ClassStats; 2],
    retries: usize,
}
pub struct Online {
    http: Http,
    gate: Mutex<Admission>,
    wake: Condvar,
    stop: Arc<AtomicBool>,
    interval: Duration,
    class_intervals: [Duration; 2],
    budget: usize,
    max_bytes: u64,
    retry_delay: Duration,
    started: Instant,
}
struct Permit<'a> {
    client: &'a Online,
    class: usize,
}
impl Drop for Permit<'_> {
    fn drop(&mut self) {
        self.client.gate.lock().unwrap().active[self.class] -= 1;
        self.client.wake.notify_all();
    }
}
struct Opened<'a> {
    response: Response,
    permit: Permit<'a>,
}
impl Online {
    #[allow(dead_code)] // Compatibility constructor; jobs select explicit profiles.
    pub fn new(
        interval: f64,
        budget: usize,
        max_bytes: u64,
        stop: Arc<AtomicBool>,
    ) -> Result<Self> {
        Self::with_profile(interval, budget, max_bytes, stop, false, 2.0)
    }
    pub fn with_profile(
        interval: f64,
        budget: usize,
        max_bytes: u64,
        stop: Arc<AtomicBool>,
        balanced: bool,
        api_interval: f64,
    ) -> Result<Self> {
        let minimum = if balanced { 0.1 } else { 2.0 };
        if !interval.is_finite()
            || !(minimum..=3600.0).contains(&interval)
            || !api_interval.is_finite()
            || !(0.1..=3600.0).contains(&api_interval)
            || !(1..=MAX_REQUEST_BUDGET).contains(&budget)
            || max_bytes == 0
        {
            return Err(invalid(
                "Use a valid profile interval and request budget 1–1000000",
            ));
        }
        let http = Http::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(10))
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
            gate: Mutex::new(Admission::default()),
            wake: Condvar::new(),
            stop,
            interval: Duration::from_secs_f64(interval),
            class_intervals: [
                Duration::from_secs_f64(api_interval),
                Duration::from_secs_f64(interval),
            ],
            budget,
            max_bytes,
            retry_delay: Duration::from_secs(1),
            started: Instant::now(),
        })
    }
    fn stop_error(&self, reason: StopReason, message: &str) -> Error {
        let mut gate = self.gate.lock().unwrap();
        if gate.reason.is_none() {
            gate.reason = Some(reason);
        }
        self.stop.store(true, Ordering::SeqCst);
        self.wake.notify_all();
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
    fn ready_at(&self, gate: &Admission, class: usize, now: Instant) -> Instant {
        let global = gate.last.map(|last| last + self.interval).unwrap_or(now);
        let class = gate.class_last[class]
            .map(|last| last + self.class_intervals[class])
            .unwrap_or(now);
        global.max(class)
    }
    fn eligible(&self, gate: &Admission, class: usize, now: Instant) -> bool {
        gate.active.iter().sum::<usize>() < 8
            && gate.active[class] < [4, 8][class]
            && self.ready_at(gate, class, now) <= now
    }
    fn admit(&self, asset: bool) -> Result<Permit<'_>> {
        let class = usize::from(asset);
        let entered = Instant::now();
        let mut gate = self.gate.lock().unwrap();
        let ticket = gate.next_ticket;
        gate.next_ticket += 1;
        gate.waiters.push_back((ticket, class));
        loop {
            let error = if self.stopped() {
                Some(Error::Stop(
                    "Job stopped; completed works remain saved".into(),
                ))
            } else if gate.count >= self.budget {
                Some(Error::Budget)
            } else {
                None
            };
            if let Some(error) = error {
                gate.waiters.retain(|(id, _)| *id != ticket);
                gate.admission_wait += entered.elapsed();
                self.wake.notify_all();
                return Err(error);
            }
            let now = Instant::now();
            let next = gate
                .waiters
                .iter()
                .find(|(_, c)| self.eligible(&gate, *c, now))
                .map(|(id, _)| *id);
            if next == Some(ticket) {
                gate.waiters.retain(|(id, _)| *id != ticket);
                gate.last = Some(now);
                gate.class_last[class] = Some(now);
                gate.count += 1;
                gate.active[class] += 1;
                gate.peak_active[class] = gate.peak_active[class].max(gate.active[class]);
                gate.peak_total = gate.peak_total.max(gate.active.iter().sum());
                gate.stats[class].requests += 1;
                gate.admission_wait += entered.elapsed();
                self.wake.notify_all();
                return Ok(Permit {
                    client: self,
                    class,
                });
            }
            let rate_limited = self.ready_at(&gate, class, now) > now;
            let wait = self
                .ready_at(&gate, class, now)
                .saturating_duration_since(now)
                .min(Duration::from_millis(25))
                .max(Duration::from_millis(1));
            // Condvar releases the mutex. DNS, TLS, response headers and body are
            // also outside this lock; no future send slots are pre-reserved.
            let (mut resumed, _) = self.wake.wait_timeout(gate, wait).unwrap();
            if rate_limited {
                let end = Instant::now();
                let start = resumed.wait_end.unwrap_or(now).max(now);
                if end > start {
                    resumed.waiting += end.duration_since(start);
                }
                resumed.wait_end = Some(end);
            }
            gate = resumed;
        }
    }
    fn error(&self, class: usize, kind: &'static str) {
        *self.gate.lock().unwrap().stats[class]
            .errors
            .entry(kind)
            .or_default() += 1;
    }
    fn open_validated(&self, url: Url, asset: bool) -> Result<Opened<'_>> {
        self.open_attempt(url, asset, false)
    }
    fn open_attempt(&self, url: Url, asset: bool, retry: bool) -> Result<Opened<'_>> {
        let permit = self.admit(asset)?;
        if retry {
            self.gate.lock().unwrap().retries += 1;
        }
        let class = permit.class;
        let started = Instant::now();
        self.check_stop()?;
        let response = self
            .http
            .get(url)
            .header("Referer", "https://www.pixiv.net/")
            .header("Accept-Encoding", "identity")
            .header("Accept", if asset { "*/*" } else { "application/json" })
            .send();
        let elapsed = started.elapsed().as_secs_f64();
        {
            let mut gate = self.gate.lock().unwrap();
            gate.stats[class].header_seconds += elapsed;
            if gate.stats[class].headers.len() < 2048 {
                gate.stats[class].headers.push(elapsed);
            } else {
                let index = gate.stats[class].headers_seen % 2048;
                gate.stats[class].headers[index] = elapsed;
            }
            gate.stats[class].headers_seen += 1;
        }
        let response = response.map_err(|e| {
            let kind = if e.is_timeout() {
                "timeout"
            } else if e.is_connect() {
                "connect_or_tls"
            } else {
                "request"
            };
            self.error(class, kind);
            Error::Retryable(format!("HTTPS {kind} failure"))
        })?;
        match response.status().as_u16() {
            401 | 403 | 429 => {
                self.error(class, "http_denied");
                Err(self.stop_error(
                    StopReason::HttpStatus {
                        status: response.status().as_u16(),
                    },
                    &format!("HTTP {}; all requests stopped", response.status().as_u16()),
                ))
            }
            300..=399 => {
                self.error(class, "redirect");
                Err(self.stop_error(
                    StopReason::HttpStatus {
                        status: response.status().as_u16(),
                    },
                    "HTTP redirect received; inspect your browser before resuming",
                ))
            }
            404 | 410 => {
                self.error(class, "unavailable");
                Err(Error::Missing {
                    status: response.status().as_u16(),
                })
            }
            200 => Ok(Opened { response, permit }),
            500..=599 => {
                self.error(class, "server_error");
                Err(Error::Retryable(format!(
                    "HTTP {}; temporary server error",
                    response.status().as_u16()
                )))
            }
            _ => {
                self.error(class, "http_error");
                Err(invalid(&format!(
                    "HTTP {}; request failed",
                    response.status().as_u16()
                )))
            }
        }
    }
    fn read(
        &self,
        mut opened: Opened<'_>,
        mut output: impl Write,
        limit: u64,
        asset: bool,
    ) -> Result<()> {
        let started = Instant::now();
        let class = opened.permit.class;
        let mut total = 0_u64;
        let result = (|| {
            let expected = opened.response.content_length();
            if expected.is_some_and(|n| n == 0 || n > limit) {
                return Err(invalid(
                    "Response size is empty or exceeds the configured limit",
                ));
            }
            let ct = opened
                .response
                .headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_ascii_lowercase();
            if ct.contains("text/html") || (asset && ct.contains("application/json")) {
                return Err(self.stop_error(
                    StopReason::UnexpectedContent,
                    "Expected data; received a challenge or error document",
                ));
            }
            let mut buf = [0_u8; 65536];
            loop {
                self.check_stop()?;
                let n = opened.response.read(&mut buf).map_err(|_| {
                    Error::Retryable("Incomplete HTTP transfer; temporary data discarded".into())
                })?;
                if n == 0 {
                    break;
                }
                if total == 0
                    && String::from_utf8_lossy(&buf[..n])
                        .trim_start()
                        .starts_with('<')
                {
                    return Err(self.stop_error(
                        StopReason::UnexpectedContent,
                        "Expected data; received HTML",
                    ));
                }
                total += n as u64;
                if total > limit {
                    return Err(invalid("Response exceeds configured size limit"));
                }
                output.write_all(&buf[..n])?;
            }
            if total == 0 || expected.is_some_and(|n| n != total) {
                return Err(Error::Retryable("Incomplete HTTP transfer".into()));
            }
            Ok(())
        })();
        let elapsed = started.elapsed().as_secs_f64();
        let mut gate = self.gate.lock().unwrap();
        let stats = &mut gate.stats[class];
        stats.bytes += total;
        stats.transfer_seconds += elapsed;
        if stats.transfers.len() < 2048 {
            stats.transfers.push(elapsed);
        } else {
            let index = stats.transfers_seen % 2048;
            stats.transfers[index] = elapsed;
        }
        stats.transfers_seen += 1;
        if result.is_ok() {
            stats.completed += 1;
        } else {
            *stats
                .errors
                .entry(if matches!(result, Err(Error::Retryable(_))) {
                    "body_transfer"
                } else {
                    "body_validation_or_write"
                })
                .or_default() += 1;
        }
        drop(gate);
        result
    }
    fn api_validated(&self, url: Url) -> Result<Value> {
        for attempt in 0..3 {
            let result = (|| {
                let opened = (if attempt == 0 {
                    self.open_validated(url.clone(), false)
                } else {
                    self.open_attempt(url.clone(), false, true)
                })
                .map_err(|error| match error {
                    Error::Missing { status } => {
                        Error::Unavailable(format!("HTTP {status}; resource unavailable"))
                    }
                    other => other,
                })?;
                let mut bytes = Vec::new();
                self.read(opened, &mut bytes, 16 * 1024 * 1024, false)?;
                let value: Value = serde_json::from_slice(&bytes).map_err(|_| {
                    self.stop_error(
                        StopReason::UnexpectedContent,
                        "Expected JSON; incompatible response",
                    )
                })?;
                if !value.is_object() || value.get("error").is_none() || value.get("body").is_none()
                {
                    return Err(invalid("API envelope changed"));
                }
                if value["error"] != false {
                    return Err(Error::Unavailable(
                        "API reports inaccessible resource".into(),
                    ));
                }
                Ok(value["body"].clone())
            })();
            match result {
                Err(Error::Retryable(_)) if attempt < 2 => self.wait_retry(attempt + 1)?,
                other => return other,
            }
        }
        unreachable!()
    }
    fn download_validated(&self, url: Url, path: &Path) -> Result<()> {
        std::fs::create_dir_all(path.parent().unwrap())?;
        for attempt in 0..3 {
            let result = (|| {
                let opened = self.open_attempt(url.clone(), true, attempt > 0)?;
                let mut file = tempfile::NamedTempFile::new_in(path.parent().unwrap())?;
                self.read(opened, &mut file, self.max_bytes, true)?;
                file.as_file().sync_all()?;
                file.persist(path).map_err(|e| Error::Io(e.error))?;
                Ok(())
            })();
            match result {
                Err(Error::Retryable(_)) if attempt < 2 => self.wait_retry(attempt + 1)?,
                other => return other,
            }
        }
        unreachable!()
    }
    fn wait_retry(&self, attempt: u32) -> Result<()> {
        let started = Instant::now();
        let wait = self.retry_delay.saturating_mul(attempt);
        while started.elapsed() < wait {
            self.check_stop()?;
            if self.exhausted() {
                return Err(Error::Budget);
            }
            std::thread::sleep(
                wait.saturating_sub(started.elapsed())
                    .min(Duration::from_millis(25)),
            );
        }
        Ok(())
    }
}
impl Transport for Online {
    fn api(&self, path: &str, query: &[(&str, String)]) -> Result<Value> {
        let url = validate_url(
            &format!("https://www.pixiv.net{}", api_key(path, query)?),
            false,
        )?;
        self.api_validated(url)
    }
    fn download(&self, url: &str, path: &Path) -> Result<()> {
        self.download_validated(validate_url(url, true)?, path)
    }
    fn requests(&self) -> usize {
        self.gate.lock().unwrap().count
    }
    fn stopped(&self) -> bool {
        self.stop.load(Ordering::SeqCst)
    }
    fn cancel(&self) {
        self.stop.store(true, Ordering::SeqCst);
        self.wake.notify_all();
    }
    fn exhausted(&self) -> bool {
        self.requests() >= self.budget
    }
    fn stop_reason(&self) -> Option<StopReason> {
        let gate = self.gate.lock().unwrap();
        gate.reason.clone().or_else(|| {
            if self.stopped() {
                Some(StopReason::UserCancel)
            } else if gate.count >= self.budget {
                Some(StopReason::RequestBudget)
            } else {
                None
            }
        })
    }
    fn rate_wait(&self) -> Duration {
        self.gate.lock().unwrap().waiting
    }
    fn metrics(&self) -> Value {
        let gate = self.gate.lock().unwrap();
        let mut value = json!({"requests":gate.count,"request_budget":self.budget,"requests_remaining":self.budget.saturating_sub(gate.count),
            "active":gate.active.iter().sum::<usize>(),"api_active":gate.active[0],"media_active":gate.active[1],
            "peak_active":gate.peak_total,"peak_api_active":gate.peak_active[0],"peak_media_active":gate.peak_active[1],
            "rate_wait_seconds":gate.waiting.as_secs_f64(),"admission_wait_seconds_sum":gate.admission_wait.as_secs_f64(),
            "retries":gate.retries,"global_interval_seconds":self.interval.as_secs_f64(),
            "api_interval_seconds":self.class_intervals[0].as_secs_f64(),"media_interval_seconds":self.class_intervals[1].as_secs_f64()});
        let stats = gate.stats.clone();
        drop(gate);
        let elapsed = self.started.elapsed();
        value["api"] = stats[0].value(elapsed);
        value["media"] = stats[1].value(elapsed);
        value
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        net::{TcpListener, TcpStream},
        sync::mpsc,
    };
    fn headers(stream: &mut TcpStream) -> String {
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let mut headers = Vec::new();
        let mut byte = [0];
        while !headers.ends_with(b"\r\n\r\n") {
            stream.read_exact(&mut byte).unwrap();
            headers.push(byte[0]);
        }
        String::from_utf8(headers).unwrap()
    }
    fn client(budget: usize) -> Online {
        Online {
            http: Http::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(Duration::from_secs(3))
                .build()
                .unwrap(),
            gate: Mutex::new(Admission::default()),
            wake: Condvar::new(),
            stop: Arc::default(),
            interval: Duration::from_millis(8),
            class_intervals: [Duration::from_millis(16), Duration::from_millis(8)],
            budget,
            max_bytes: 1024,
            retry_delay: Duration::from_millis(1),
            started: Instant::now(),
        }
    }
    fn server(
        status: u16,
        body: &str,
        count: usize,
    ) -> (Url, std::thread::JoinHandle<Vec<Instant>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
        let body = body.to_owned();
        let thread = std::thread::spawn(move || {
            let mut times = Vec::new();
            for stream in listener.incoming().take(count) {
                let mut stream = stream.unwrap();
                headers(&mut stream);
                times.push(Instant::now());
                write!(stream,"HTTP/1.1 {status} Test\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).unwrap();
            }
            times
        });
        (url, thread)
    }
    #[test]
    fn shared_request_interval_and_budget_over_real_sockets() {
        let (url, server) = server(200, "{}", 2);
        let client = Arc::new(client(2));
        let workers: Vec<_> = (0..2)
            .map(|_| {
                let c = client.clone();
                let u = url.clone();
                std::thread::spawn(move || {
                    c.read(
                        c.open_validated(u, false).unwrap(),
                        &mut Vec::new(),
                        1024,
                        false,
                    )
                    .unwrap();
                })
            })
            .collect();
        for w in workers {
            w.join().unwrap();
        }
        let times = server.join().unwrap();
        assert_eq!(times.len(), 2);
        assert!(times[1].duration_since(times[0]) >= Duration::from_millis(10));
        assert!(matches!(
            client.open_validated(url, false),
            Err(Error::Budget)
        ));
        assert!(!client.stopped());
        assert!(client.exhausted());
        assert_eq!(client.requests(), 2);
        assert_eq!(client.stop_reason(), Some(StopReason::RequestBudget));
        assert_eq!(client.metrics()["api"]["completed"], 2);
    }
    #[test]
    fn spent_budget_does_not_abort_an_admitted_body() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
        let (ready_tx, ready_rx) = mpsc::channel();
        let (finish_tx, finish_rx) = mpsc::channel();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            headers(&mut stream);
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\nConnection: close\r\n\r\n")
                .unwrap();
            stream.flush().unwrap();
            ready_tx.send(()).unwrap();
            finish_rx.recv_timeout(Duration::from_secs(3)).unwrap();
            stream.write_all(b"body").unwrap();
        });
        let client = Arc::new(client(1));
        let c = client.clone();
        let u = url.clone();
        let worker = std::thread::spawn(move || {
            let opened = c.open_validated(u, true).unwrap();
            let mut bytes = Vec::new();
            c.read(opened, &mut bytes, 1024, true).unwrap();
            bytes
        });
        ready_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(matches!(
            client.open_validated(url, false),
            Err(Error::Budget)
        ));
        assert!(!client.stopped());
        finish_tx.send(()).unwrap();
        assert_eq!(worker.join().unwrap(), b"body");
        server.join().unwrap();
    }
    #[test]
    fn a_slow_header_does_not_hold_the_admission_lock() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
        let (ready_tx, ready_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let server = std::thread::spawn(move || {
            let (mut slow, _) = listener.accept().unwrap();
            headers(&mut slow);
            ready_tx.send(()).unwrap();
            let slow = std::thread::spawn(move || {
                release_rx.recv_timeout(Duration::from_secs(3)).unwrap();
                slow.write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}",
                )
                .unwrap();
            });
            let (mut fast, _) = listener.accept().unwrap();
            headers(&mut fast);
            fast.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}")
                .unwrap();
            slow.join().unwrap();
        });
        let client = Arc::new(client(2));
        let c = client.clone();
        let u = url.clone();
        let slow = std::thread::spawn(move || {
            c.read(
                c.open_validated(u, false).unwrap(),
                &mut Vec::new(),
                1024,
                false,
            )
            .unwrap()
        });
        ready_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        let c = client.clone();
        let (done_tx, done_rx) = mpsc::channel();
        let fast = std::thread::spawn(move || {
            c.read(
                c.open_validated(url, true).unwrap(),
                &mut Vec::new(),
                1024,
                true,
            )
            .unwrap();
            done_tx.send(()).unwrap();
        });
        let overlapped = done_rx.recv_timeout(Duration::from_millis(300)).is_ok();
        release_tx.send(()).unwrap();
        slow.join().unwrap();
        fast.join().unwrap();
        server.join().unwrap();
        assert!(overlapped, "Header wait blocked another class");
        assert_eq!(client.metrics()["peak_active"], 2);
    }
    #[test]
    fn in_flight_caps_are_held_until_bodies_finish() {
        let client = Arc::new(client(20));
        let api1 = client.admit(false).unwrap();
        std::thread::sleep(Duration::from_millis(18));
        let api2 = client.admit(false).unwrap();
        let api3 = client.admit(false).unwrap();
        let api4 = client.admit(false).unwrap();
        let c = client.clone();
        let (tx, rx) = mpsc::channel();
        let blocked = std::thread::spawn(move || {
            let _permit = c.admit(false).unwrap();
            tx.send(()).unwrap();
        });
        assert!(rx.recv_timeout(Duration::from_millis(50)).is_err());
        let media1 = client.admit(true).unwrap();
        std::thread::sleep(Duration::from_millis(10));
        let media2 = client.admit(true).unwrap();
        let media3 = client.admit(true).unwrap();
        let media4 = client.admit(true).unwrap();
        assert_eq!(client.metrics()["active"], 8);
        assert_eq!(client.metrics()["api_active"], 4);
        drop(api1);
        rx.recv_timeout(Duration::from_secs(1)).unwrap();
        drop(api2);
        drop(api3);
        drop(api4);
        drop(media1);
        drop(media2);
        drop(media3);
        drop(media4);
        blocked.join().unwrap();
        assert_eq!(client.metrics()["peak_active"], 8);
        assert_eq!(client.metrics()["peak_api_active"], 4);
    }
    #[test]
    fn rejection_latches_before_another_request() {
        for status in [302, 401, 403, 429] {
            let (url, server) = server(status, "blocked", 1);
            let client = client(10);
            assert!(matches!(
                client.open_validated(url.clone(), false),
                Err(Error::Stop(_))
            ));
            assert!(matches!(
                client.open_validated(url, false),
                Err(Error::Stop(_))
            ));
            assert_eq!(client.requests(), 1);
            assert_eq!(
                client.stop_reason(),
                Some(StopReason::HttpStatus { status })
            );
            server.join().unwrap();
        }
    }
    #[test]
    fn html_media_is_rejected_before_persisting() {
        let (url, server) = server(200, "<html>challenge</html>", 1);
        let client = client(10);
        let opened = client.open_validated(url, true).unwrap();
        let mut bytes = Vec::new();
        assert!(matches!(
            client.read(opened, &mut bytes, 1024, true),
            Err(Error::Stop(_))
        ));
        assert!(bytes.is_empty());
        assert!(client.stopped());
        assert_eq!(client.stop_reason(), Some(StopReason::UnexpectedContent));
        server.join().unwrap();
    }
    #[test]
    fn retry_is_finite_counted_and_never_applies_to_denial() {
        let temp = tempfile::tempdir().unwrap();
        let out = temp.path().join("out");
        let (url, server) = server(503, "bad", 3);
        let client = client(10);
        assert!(matches!(
            client.download_validated(url, &out),
            Err(Error::Retryable(_))
        ));
        assert_eq!(client.requests(), 3);
        assert_eq!(client.metrics()["retries"], 2);
        assert!(!out.exists());
        server.join().unwrap();
        let (url, server) = self::server(403, "bad", 1);
        let denied = self::client(10);
        assert!(matches!(
            denied.download_validated(url, &out),
            Err(Error::Stop(_))
        ));
        assert_eq!(denied.requests(), 1);
        assert_eq!(denied.metrics()["retries"], 0);
        server.join().unwrap();
    }
    #[test]
    fn failed_body_retry_discards_partial_temp() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
        let server = std::thread::spawn(move || {
            for (i, stream) in listener.incoming().take(2).enumerate() {
                let mut stream = stream.unwrap();
                headers(&mut stream);
                stream
                    .write_all(if i == 0 {
                        b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\nConnection: close\r\n\r\nx"
                    } else {
                        b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\nConnection: close\r\n\r\nbody"
                    })
                    .unwrap();
            }
        });
        let temp = tempfile::tempdir().unwrap();
        let out = temp.path().join("out");
        let client = client(10);
        client.download_validated(url, &out).unwrap();
        assert_eq!(std::fs::read(&out).unwrap(), b"body");
        assert_eq!(client.requests(), 2);
        assert_eq!(std::fs::read_dir(temp.path()).unwrap().count(), 1);
        server.join().unwrap();
    }
    #[test]
    fn api_retry_is_finite_counted_and_respects_denial_and_budget() {
        let (url, server) = server(503, "bad", 3);
        let client = client(10);
        assert!(matches!(
            client.api_validated(url),
            Err(Error::Retryable(_))
        ));
        assert_eq!(client.requests(), 3);
        assert_eq!(client.metrics()["retries"], 2);
        server.join().unwrap();
        for status in [401, 403, 429, 302] {
            let (url, server) = self::server(status, "denied", 1);
            let client = self::client(10);
            assert!(matches!(client.api_validated(url), Err(Error::Stop(_))));
            assert_eq!(client.requests(), 1);
            assert_eq!(client.metrics()["retries"], 0);
            server.join().unwrap();
        }
        let (url, server) = self::server(503, "bad", 1);
        let client = self::client(1);
        assert!(matches!(client.api_validated(url), Err(Error::Budget)));
        assert_eq!(client.requests(), 1);
        assert_eq!(client.metrics()["retries"], 0);
        server.join().unwrap();
    }
    #[test]
    fn api_retry_discards_incomplete_json_before_recovering() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
        let server = std::thread::spawn(move || {
            for (attempt, stream) in listener.incoming().take(2).enumerate() {
                let mut stream = stream.unwrap();
                headers(&mut stream);
                if attempt == 0 {
                    stream
                        .write_all(
                            b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\nConnection: close\r\n\r\n{",
                        )
                        .unwrap();
                } else {
                    let body = br#"{"error":false,"body":{"ok":true}}"#;
                    write!(
                        stream,
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    )
                    .unwrap();
                    stream.write_all(body).unwrap();
                }
            }
        });
        let client = client(10);
        assert_eq!(client.api_validated(url).unwrap(), json!({"ok":true}));
        assert_eq!(client.requests(), 2);
        assert_eq!(client.metrics()["retries"], 1);
        server.join().unwrap();
    }
    #[test]
    fn a_waiting_api_does_not_block_a_ready_media_ticket() {
        let mut raw = client(10);
        raw.class_intervals[0] = Duration::from_secs(2);
        let client = Arc::new(raw);
        let first = client.admit(false).unwrap();
        let c = client.clone();
        let waiting = std::thread::spawn(move || c.admit(false).map(drop));
        let deadline = Instant::now() + Duration::from_secs(1);
        while client.gate.lock().unwrap().waiters.is_empty() {
            assert!(Instant::now() < deadline);
            std::thread::yield_now();
        }
        let media = client.admit(true).unwrap();
        assert_eq!(client.requests(), 2);
        client.cancel();
        drop(media);
        drop(first);
        assert!(matches!(waiting.join().unwrap(), Err(Error::Stop(_))));
    }
    #[test]
    fn spent_budget_does_not_count_an_unsent_retry() {
        let (url, server) = server(503, "bad", 1);
        let client = client(1);
        let temp = tempfile::tempdir().unwrap();
        assert!(matches!(
            client.download_validated(url, &temp.path().join("out")),
            Err(Error::Budget)
        ));
        assert_eq!(client.requests(), 1);
        assert_eq!(client.metrics()["retries"], 0);
        assert!(!client.stopped());
        server.join().unwrap();
    }
    #[test]
    fn missing_media_is_typed_but_missing_api_remains_unavailable() {
        for status in [404, 410] {
            let (url, server) = server(status, "gone", 1);
            let client = client(5);
            let temp = tempfile::tempdir().unwrap();
            assert!(
                matches!(client.download_validated(url, &temp.path().join("out")), Err(Error::Missing { status: value }) if value == status)
            );
            assert_eq!(client.requests(), 1);
            assert_eq!(client.metrics()["retries"], 0);
            assert!(!client.stopped());
            server.join().unwrap();
            let (url, server) = self::server(status, "gone", 1);
            let client = self::client(5);
            assert!(matches!(
                client.api_validated(url),
                Err(Error::Unavailable(_))
            ));
            assert_eq!(client.requests(), 1);
            assert!(!client.stopped());
            server.join().unwrap();
        }
    }
    #[test]
    fn custom_intervals_preserve_global_and_api_spacing() {
        let custom = Online::with_profile(0.1, 100, 1024, Arc::default(), true, 0.5).unwrap();
        let start = Instant::now();
        let gate = Admission {
            last: Some(start),
            class_last: [Some(start), None],
            ..Default::default()
        };
        assert!(!custom.eligible(&gate, 0, start + Duration::from_millis(499)));
        assert!(custom.eligible(&gate, 0, start + Duration::from_millis(500)));
        assert!(!custom.eligible(&gate, 1, start + Duration::from_millis(99)));
        assert!(custom.eligible(&gate, 1, start + Duration::from_millis(100)));
        for value in [0.0, -1.0, 0.09, 3601.0, f64::NAN, f64::INFINITY] {
            assert!(Online::with_profile(value, 100, 1024, Arc::default(), true, 2.0).is_err());
            assert!(Online::with_profile(1.0, 100, 1024, Arc::default(), true, value).is_err());
        }
    }
    #[test]
    fn strict_remote_origins_and_profiles() {
        for url in [
            "http://i.pximg.net/a.png",
            "https://pximg.net.evil/a.png",
            "https://u@i.pximg.net/a.png",
            "https://i.pximg.net:123/a.png",
        ] {
            assert!(validate_url(url, true).is_err());
        }
        assert!(validate_url("https://i.pximg.net/a.png", true).is_ok());
        assert!(validate_url("https://www.pixiv.net/ajax/illust/1", false).is_ok());
        assert!(Online::new(1.0, 500, 1024, Arc::default()).is_err());
        assert!(Online::with_profile(1.0, 20000, 1024, Arc::default(), true, 2.0).is_ok());
        assert!(
            Online::with_profile(1.0, MAX_REQUEST_BUDGET + 1, 1024, Arc::default(), true, 2.0)
                .is_err()
        );
        assert!(
            Online::with_profile(1.0, MAX_REQUEST_BUDGET, 1024, Arc::default(), true, 2.0).is_ok()
        );
    }
    #[test]
    fn fixture_cancel_query_encoding_and_atomic_budget() {
        let fixture = Fixture::with_budget(DEMO, Arc::default(), 1).unwrap();
        fixture
            .api(
                "/ajax/novel/1004/insert_illusts",
                &[("id[]", "1002-2".into())],
            )
            .unwrap();
        assert!(matches!(
            fixture.api("/ajax/illust/1001", &[]),
            Err(Error::Budget)
        ));
        assert!(!fixture.stopped());
        fixture.cancel();
        assert!(matches!(
            fixture.api("/ajax/illust/1001", &[]),
            Err(Error::Stop(_))
        ));
        let fixture = Arc::new(Fixture::with_budget(DEMO, Arc::default(), 3).unwrap());
        let threads: Vec<_> = (0..20)
            .map(|_| {
                let f = fixture.clone();
                std::thread::spawn(move || f.api("/ajax/illust/1001", &[]))
            })
            .collect();
        assert_eq!(
            threads
                .into_iter()
                .map(|t| t.join().unwrap())
                .filter(Result::is_ok)
                .count(),
            3
        );
        assert_eq!(fixture.requests(), 3);
    }
}
