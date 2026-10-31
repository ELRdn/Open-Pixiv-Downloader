//! Bounded, run-local singleflight cache for original media.
//!
//! Cache files are private temporary files. Every destination receives a verified,
//! independent regular copy; edits to an archive never mutate the cache or another work.
use crate::{
    error::{invalid, Error, Result, StopReason},
    transport::{validate_url, Transport},
};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    fs::{self, File},
    io::{Read, Write},
    path::Path,
    sync::{Arc, Condvar, Mutex},
    time::Duration,
};

const CACHE_BYTES: u64 = 512 * 1024 * 1024;
const ACTIVE_BYTES: u64 = 1024 * 1024 * 1024;
const MAX_ENTRIES: usize = 4096;

#[derive(Clone)]
enum Failure {
    Budget,
    Corrupt,
    Stop(String),
    Unavailable(String),
    Retryable(String),
    Io(std::io::ErrorKind),
    Other,
}
impl Failure {
    fn from_error(error: &Error) -> Self {
        match error {
            Error::Budget => Self::Budget,
            Error::Stop(message) => Self::Stop(message.clone()),
            Error::Unavailable(message) => Self::Unavailable(message.clone()),
            Error::Retryable(message) => Self::Retryable(message.clone()),
            Error::Io(error) => Self::Io(error.kind()),
            _ => Self::Other,
        }
    }
    fn error(&self) -> Error {
        match self {
            Self::Budget => Error::Budget,
            Self::Corrupt => invalid("Shared media cache checksum changed"),
            Self::Stop(message) => Error::Stop(message.clone()),
            Self::Unavailable(message) => Error::Unavailable(message.clone()),
            Self::Retryable(message) => Error::Retryable(message.clone()),
            Self::Io(kind) => std::io::Error::new(*kind, "Shared media cache I/O failed").into(),
            Self::Other => invalid("Shared media acquisition failed; retry repair later"),
        }
    }
}
struct Cached {
    path: tempfile::TempPath,
    hash: String,
    bytes: u64,
}
enum Status {
    Loading,
    Ready(Cached),
    Failed(Failure),
}
struct Entry {
    status: Mutex<Status>,
    changed: Condvar,
}
struct Record {
    entry: Arc<Entry>,
    touched: u64,
    bytes: u64,
}
#[derive(Default)]
struct State {
    records: HashMap<[u8; 32], Record>,
    clock: u64,
    stored: u64,
    reserved: u64,
    peak_bytes: u64,
    hits: u64,
    misses: u64,
    failures: u64,
    evictions: u64,
    corruptions: u64,
}

pub struct SharedMedia {
    inner: Arc<dyn Transport>,
    directory: tempfile::TempDir,
    state: Mutex<State>,
    changed: Condvar,
    max_file: u64,
    cache_limit: u64,
    active_limit: u64,
    entries_limit: usize,
}
struct Lease<'a> {
    owner: &'a SharedMedia,
    entry: Option<Arc<Entry>>,
}
impl Drop for Lease<'_> {
    fn drop(&mut self) {
        // Release our pin before evicting. The file cannot disappear while a
        // consumer is reading it, including on Windows where open files pin names.
        self.entry.take();
        let mut state = self.owner.state.lock().unwrap();
        self.owner.trim(&mut state, false);
        self.owner.changed.notify_all();
    }
}
impl SharedMedia {
    pub fn new(inner: Arc<dyn Transport>, max_bytes: u64) -> Result<Self> {
        Self::with_limits(inner, max_bytes, CACHE_BYTES, ACTIVE_BYTES, MAX_ENTRIES)
    }
    pub fn with_limits(
        inner: Arc<dyn Transport>,
        max_bytes: u64,
        cache_bytes: u64,
        active_bytes: u64,
        entries: usize,
    ) -> Result<Self> {
        if max_bytes == 0
            || max_bytes > active_bytes
            || cache_bytes == 0
            || entries == 0
            || cache_bytes.checked_add(active_bytes).is_none()
        {
            return Err(invalid("Invalid shared media cache limits"));
        }
        Ok(Self {
            inner,
            directory: tempfile::tempdir()?,
            state: Mutex::new(State::default()),
            changed: Condvar::new(),
            max_file: max_bytes,
            cache_limit: cache_bytes,
            active_limit: active_bytes,
            entries_limit: entries,
        })
    }
    fn check_cancel(&self) -> Result<()> {
        if self.stopped() {
            Err(Error::Stop("Shared media operation stopped".into()))
        } else {
            Ok(())
        }
    }
    fn evict_one(&self, state: &mut State) -> bool {
        let key = state
            .records
            .iter()
            .filter(|(_, record)| {
                Arc::strong_count(&record.entry) == 1
                    && !matches!(*record.entry.status.lock().unwrap(), Status::Loading)
            })
            .min_by_key(|(_, record)| record.touched)
            .map(|(key, _)| *key);
        let Some(key) = key else { return false };
        if let Some(record) = state.records.remove(&key) {
            state.stored -= record.bytes;
            state.evictions += 1;
        }
        true
    }
    fn trim(&self, state: &mut State, one_slot: bool) {
        while state.stored > self.cache_limit
            || (one_slot && state.records.len() >= self.entries_limit)
        {
            if !self.evict_one(state) {
                break;
            }
        }
    }
    fn claim(&self, key: [u8; 32]) -> Result<(Lease<'_>, bool)> {
        let mut state = self.state.lock().unwrap();
        loop {
            self.check_cancel()?;
            state.clock = state.clock.wrapping_add(1);
            let tick = state.clock;
            if let Some(record) = state.records.get_mut(&key) {
                record.touched = tick;
                let entry = record.entry.clone();
                state.hits += 1;
                return Ok((
                    Lease {
                        owner: self,
                        entry: Some(entry),
                    },
                    false,
                ));
            }
            if self.inner.exhausted() {
                return Err(Error::Budget);
            }
            self.trim(&mut state, true);
            // Reserve the whole configured file limit before starting HTTP. The
            // combined retained + active bound also covers pinned ready files.
            let total_limit = self.cache_limit + self.active_limit;
            while state.stored + state.reserved + self.max_file > total_limit {
                if !self.evict_one(&mut state) {
                    break;
                }
            }
            if state.records.len() < self.entries_limit
                && state.reserved + self.max_file <= self.active_limit
                && state.stored + state.reserved + self.max_file <= total_limit
            {
                let entry = Arc::new(Entry {
                    status: Mutex::new(Status::Loading),
                    changed: Condvar::new(),
                });
                state.records.insert(
                    key,
                    Record {
                        entry: entry.clone(),
                        touched: tick,
                        bytes: 0,
                    },
                );
                state.reserved += self.max_file;
                state.peak_bytes = state.peak_bytes.max(state.stored + state.reserved);
                state.misses += 1;
                return Ok((
                    Lease {
                        owner: self,
                        entry: Some(entry),
                    },
                    true,
                ));
            }
            state = self
                .changed
                .wait_timeout(state, Duration::from_millis(50))
                .unwrap()
                .0;
        }
    }
    fn acquire(&self, url: &str) -> Result<Cached> {
        let path = tempfile::NamedTempFile::new_in(self.directory.path())?.into_temp_path();
        self.inner.download(url, &path)?;
        self.check_cancel()?;
        if fs::symlink_metadata(&path)?.file_type().is_symlink() {
            return Err(invalid("Shared media cache source is unsafe"));
        }
        let mut file = File::open(&path)?;
        let mut hash = Sha256::new();
        let mut bytes = 0_u64;
        let mut buffer = [0; 65536];
        loop {
            self.check_cancel()?;
            let size = file.read(&mut buffer)?;
            if size == 0 {
                break;
            }
            bytes += size as u64;
            if bytes > self.max_file {
                return Err(invalid("Shared media exceeds file limit"));
            }
            hash.update(&buffer[..size]);
        }
        if bytes == 0 {
            return Err(invalid("Shared media is empty"));
        }
        Ok(Cached {
            path,
            hash: format!("{:x}", hash.finalize()),
            bytes,
        })
    }
    fn publish(&self, key: [u8; 32], entry: &Entry, acquired: Result<Cached>) {
        let mut state = self.state.lock().unwrap();
        state.reserved -= self.max_file;
        let status = match acquired {
            Ok(cached) => {
                state.stored += cached.bytes;
                if let Some(record) = state.records.get_mut(&key) {
                    record.bytes = cached.bytes;
                }
                Status::Ready(cached)
            }
            Err(error) => {
                state.failures += 1;
                Status::Failed(Failure::from_error(&error))
            }
        };
        *entry.status.lock().unwrap() = status;
        entry.changed.notify_all();
        self.changed.notify_all();
    }
    fn copy(&self, cached: &Cached, destination: &Path) -> Result<bool> {
        // Returns false for cache corruption. A corrupt cached file is never
        // published into the archive, and its next acquisition can re-download.
        self.check_cancel()?;
        match fs::symlink_metadata(&cached.path) {
            Ok(metadata) if metadata.file_type().is_file() => {}
            Ok(_) => return Ok(false),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error.into()),
        }
        if fs::symlink_metadata(destination).is_ok_and(|metadata| metadata.file_type().is_symlink())
        {
            return Err(invalid("Shared media destination is unsafe"));
        }
        let parent = destination
            .parent()
            .ok_or_else(|| invalid("Missing media parent directory"))?;
        fs::create_dir_all(parent)?;
        let mut source = File::open(&cached.path)?;
        let mut output = tempfile::NamedTempFile::new_in(parent)?;
        let mut hash = Sha256::new();
        let mut bytes = 0_u64;
        let mut buffer = [0; 65536];
        loop {
            self.check_cancel()?;
            let size = source.read(&mut buffer)?;
            if size == 0 {
                break;
            }
            bytes += size as u64;
            if bytes > cached.bytes {
                return Ok(false);
            }
            hash.update(&buffer[..size]);
            output.write_all(&buffer[..size])?;
        }
        if bytes != cached.bytes || format!("{:x}", hash.finalize()) != cached.hash {
            return Ok(false);
        }
        output.as_file().sync_all()?;
        self.check_cancel()?;
        output
            .persist(destination)
            .map_err(|e| Error::Io(e.error))?;
        Ok(true)
    }
    fn invalidate(&self, key: [u8; 32], entry: &Arc<Entry>) {
        let mut state = self.state.lock().unwrap();
        if state
            .records
            .get(&key)
            .is_some_and(|r| Arc::ptr_eq(&r.entry, entry))
        {
            let record = state.records.remove(&key).unwrap();
            // All copies take this entry mutex, so replacing the status removes
            // its corrupt file only after existing readers have closed it.
            *entry.status.lock().unwrap() = Status::Failed(Failure::Corrupt);
            state.stored -= record.bytes;
            state.corruptions += 1;
        }
        self.changed.notify_all();
    }
}
impl Transport for SharedMedia {
    fn api(&self, path: &str, query: &[(&str, String)]) -> Result<Value> {
        self.inner.api(path, query)
    }
    fn download(&self, url: &str, destination: &Path) -> Result<()> {
        validate_url(url, true)?;
        let key: [u8; 32] = Sha256::digest(url.as_bytes()).into();
        // A cache checksum failure gets one fresh acquisition, not an unbounded
        // recursion. Network retry policy belongs to the underlying transport.
        for _ in 0..2 {
            let (lease, creator) = self.claim(key)?;
            let entry = lease.entry.as_ref().unwrap();
            if creator {
                let acquired =
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.acquire(url)))
                        .unwrap_or_else(|_| {
                            Err(invalid("Shared media worker terminated unexpectedly"))
                        });
                self.publish(key, entry, acquired);
            }
            let mut status = entry.status.lock().unwrap();
            loop {
                self.check_cancel()?;
                match &*status {
                    Status::Loading => {
                        status = entry
                            .changed
                            .wait_timeout(status, Duration::from_millis(50))
                            .unwrap()
                            .0;
                    }
                    Status::Failed(Failure::Corrupt) => {
                        drop(status);
                        break;
                    }
                    Status::Failed(failure) => return Err(failure.error()),
                    Status::Ready(cached) => {
                        // The entry pin prevents LRU eviction. Holding its state
                        // mutex also prevents concurrent use of corrupt cache data.
                        let copied = self.copy(cached, destination);
                        drop(status);
                        match copied {
                            Ok(true) => return Ok(()),
                            Ok(false) => {
                                self.invalidate(key, entry);
                                break;
                            }
                            Err(error) => return Err(error),
                        }
                    }
                }
            }
        }
        Err(invalid("Shared media cache failed checksum verification"))
    }
    fn requests(&self) -> usize {
        self.inner.requests()
    }
    fn stopped(&self) -> bool {
        self.inner.stopped()
    }
    fn cancel(&self) {
        self.inner.cancel();
        self.changed.notify_all();
    }
    fn exhausted(&self) -> bool {
        self.inner.exhausted()
    }
    fn stop_reason(&self) -> Option<StopReason> {
        self.inner.stop_reason()
    }
    fn rate_wait(&self) -> Duration {
        self.inner.rate_wait()
    }
    fn metrics(&self) -> Value {
        let mut metrics = self.inner.metrics();
        if !metrics.is_object() {
            metrics = json!({"transport":metrics});
        }
        let state = self.state.lock().unwrap();
        metrics["shared_media"] = json!({"hits":state.hits,"misses":state.misses,"failures":state.failures,"evictions":state.evictions,"corruptions":state.corruptions,"cache_bytes":state.stored,"reserved_bytes":state.reserved,"peak_reserved_and_cached_bytes":state.peak_bytes,"entries":state.records.len(),"cache_limit_bytes":self.cache_limit,"active_limit_bytes":self.active_limit});
        metrics
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::Barrier;
    use std::thread;

    struct Counting {
        count: AtomicUsize,
        stop: AtomicBool,
        fail: AtomicBool,
        io_fail: AtomicBool,
        panic: AtomicBool,
        limit: usize,
        delay: Duration,
        bytes: Vec<u8>,
    }
    impl Counting {
        fn new() -> Self {
            Self {
                count: 0.into(),
                stop: false.into(),
                fail: false.into(),
                io_fail: false.into(),
                panic: false.into(),
                limit: usize::MAX,
                delay: Duration::from_millis(30),
                bytes: b"same original media bytes".to_vec(),
            }
        }
    }
    impl Transport for Counting {
        fn api(&self, _: &str, _: &[(&str, String)]) -> Result<Value> {
            Ok(json!({}))
        }
        fn download(&self, _: &str, destination: &Path) -> Result<()> {
            if self.exhausted() {
                return Err(Error::Budget);
            }
            self.count.fetch_add(1, Ordering::SeqCst);
            thread::sleep(self.delay);
            assert!(!self.panic.load(Ordering::SeqCst), "synthetic media panic");
            if self.io_fail.load(Ordering::SeqCst) {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "synthetic cache I/O failure",
                )
                .into());
            }
            if self.fail.load(Ordering::SeqCst) {
                return Err(Error::Unavailable("Synthetic missing media".into()));
            }
            if self.stopped() {
                return Err(Error::Stop("Synthetic user cancellation".into()));
            }
            fs::write(destination, &self.bytes)?;
            Ok(())
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
            self.requests() >= self.limit
        }
    }
    fn run_four(cache: Arc<SharedMedia>, directory: &Path) -> Vec<Result<()>> {
        let barrier = Arc::new(Barrier::new(5));
        let handles: Vec<_> = (0..4)
            .map(|n| {
                let cache = cache.clone();
                let barrier = barrier.clone();
                let destination = directory.join(format!("copy-{n}.jpg"));
                thread::spawn(move || {
                    barrier.wait();
                    cache.download("https://i.pximg.net/same.jpg?exact=1", &destination)
                })
            })
            .collect();
        barrier.wait();
        handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect()
    }
    #[test]
    fn simultaneous_same_url_is_once_and_outputs_are_independent() {
        let directory = tempfile::tempdir().unwrap();
        let inner = Arc::new(Counting::new());
        let cache = Arc::new(SharedMedia::new(inner.clone(), 1024).unwrap());
        for result in run_four(cache.clone(), directory.path()) {
            result.unwrap();
        }
        assert_eq!(inner.requests(), 1);
        assert_eq!(cache.metrics()["shared_media"]["hits"], 3);
        for n in 0..4 {
            assert_eq!(
                fs::read(directory.path().join(format!("copy-{n}.jpg"))).unwrap(),
                inner.bytes
            );
        }
        fs::write(directory.path().join("copy-0.jpg"), b"user edit").unwrap();
        cache
            .download(
                "https://i.pximg.net/same.jpg?exact=1",
                &directory.path().join("another.jpg"),
            )
            .unwrap();
        assert_eq!(inner.requests(), 1);
        assert_eq!(
            fs::read(directory.path().join("another.jpg")).unwrap(),
            inner.bytes
        );
        assert_eq!(
            fs::read(directory.path().join("copy-1.jpg")).unwrap(),
            inner.bytes
        );
        cache
            .download(
                "https://i.pximg.net/same.jpg?exact=2",
                &directory.path().join("different-query.jpg"),
            )
            .unwrap();
        assert_eq!(inner.requests(), 2);
    }
    #[test]
    fn concurrent_corrupt_cache_consumers_share_the_repair() {
        let directory = tempfile::tempdir().unwrap();
        let inner = Arc::new(Counting::new());
        let cache = Arc::new(SharedMedia::new(inner.clone(), 1024).unwrap());
        cache
            .download(
                "https://i.pximg.net/same.jpg?exact=1",
                &directory.path().join("initial.jpg"),
            )
            .unwrap();
        {
            let state = cache.state.lock().unwrap();
            let record = state.records.values().next().unwrap();
            let status = record.entry.status.lock().unwrap();
            let Status::Ready(cached) = &*status else {
                panic!("cache not ready")
            };
            fs::write(&cached.path, b"corrupt cached bytes").unwrap();
        }
        for result in run_four(cache.clone(), directory.path()) {
            result.unwrap();
        }
        assert_eq!(inner.requests(), 2);
        for n in 0..4 {
            assert_eq!(
                fs::read(directory.path().join(format!("copy-{n}.jpg"))).unwrap(),
                inner.bytes
            );
        }
    }
    #[test]
    fn cache_hits_survive_budget_without_new_request() {
        let directory = tempfile::tempdir().unwrap();
        let mut raw = Counting::new();
        raw.limit = 1;
        let inner = Arc::new(raw);
        let cache = SharedMedia::new(inner.clone(), 1024).unwrap();
        cache
            .download(
                "https://i.pximg.net/one.jpg",
                &directory.path().join("first.jpg"),
            )
            .unwrap();
        assert!(cache.exhausted());
        assert!(!cache.stopped());
        cache
            .download(
                "https://i.pximg.net/one.jpg",
                &directory.path().join("second.jpg"),
            )
            .unwrap();
        assert!(matches!(
            cache.download(
                "https://i.pximg.net/two.jpg",
                &directory.path().join("third.jpg")
            ),
            Err(Error::Budget)
        ));
        assert_eq!(inner.requests(), 1);
    }
    #[test]
    fn failure_and_panic_release_all_waiters_without_request_storm() {
        for panic in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let inner = Arc::new(Counting::new());
            inner.fail.store(!panic, Ordering::SeqCst);
            inner.panic.store(panic, Ordering::SeqCst);
            let cache = Arc::new(SharedMedia::new(inner.clone(), 1024).unwrap());
            assert!(run_four(cache.clone(), directory.path())
                .into_iter()
                .all(|result| result.is_err()));
            assert_eq!(inner.requests(), 1);
            assert_eq!(cache.metrics()["shared_media"]["reserved_bytes"], 0);
            assert_eq!(fs::read_dir(cache.directory.path()).unwrap().count(), 0);
            inner.fail.store(false, Ordering::SeqCst);
            inner.panic.store(false, Ordering::SeqCst);
            let next_run = SharedMedia::new(inner.clone(), 1024).unwrap();
            next_run
                .download(
                    "https://i.pximg.net/same.jpg?exact=1",
                    &directory.path().join("fixed.jpg"),
                )
                .unwrap();
            assert_eq!(inner.requests(), 2);
        }
    }
    #[test]
    fn producer_io_failure_retains_type_for_every_waiter() {
        let directory = tempfile::tempdir().unwrap();
        let inner = Arc::new(Counting::new());
        inner.io_fail.store(true, Ordering::SeqCst);
        let cache = Arc::new(SharedMedia::new(inner.clone(), 1024).unwrap());
        for result in run_four(cache.clone(), directory.path()) {
            assert!(
                matches!(result, Err(Error::Io(error)) if error.kind() == std::io::ErrorKind::PermissionDenied)
            );
        }
        assert_eq!(inner.requests(), 1);
        assert_eq!(cache.metrics()["shared_media"]["reserved_bytes"], 0);
    }
    #[test]
    fn corruption_repairs_before_publishing_and_metadata_is_bounded() {
        let directory = tempfile::tempdir().unwrap();
        let inner = Arc::new(Counting::new());
        let cache = SharedMedia::with_limits(inner.clone(), 64, 64, 64, 2).unwrap();
        cache
            .download(
                "https://i.pximg.net/one.jpg",
                &directory.path().join("one.jpg"),
            )
            .unwrap();
        {
            let state = cache.state.lock().unwrap();
            let record = state.records.values().next().unwrap();
            let status = record.entry.status.lock().unwrap();
            let Status::Ready(cached) = &*status else {
                panic!("cache not ready")
            };
            fs::write(&cached.path, b"corrupted bytes").unwrap();
        }
        cache
            .download(
                "https://i.pximg.net/one.jpg",
                &directory.path().join("repaired.jpg"),
            )
            .unwrap();
        assert_eq!(inner.requests(), 2);
        assert_eq!(
            fs::read(directory.path().join("repaired.jpg")).unwrap(),
            inner.bytes
        );
        assert_eq!(cache.metrics()["shared_media"]["corruptions"], 1);
        for n in 0..10 {
            cache
                .download(
                    &format!("https://i.pximg.net/{n}.jpg"),
                    &directory.path().join(format!("{n}.jpg")),
                )
                .unwrap();
        }
        let metrics = cache.metrics();
        assert!(metrics["shared_media"]["entries"].as_u64().unwrap() <= 2);
        assert!(metrics["shared_media"]["cache_bytes"].as_u64().unwrap() <= 64);
        assert!(
            metrics["shared_media"]["peak_reserved_and_cached_bytes"]
                .as_u64()
                .unwrap()
                <= 128
        );
        assert!(metrics["shared_media"]["evictions"].as_u64().unwrap() > 0);
    }
    #[test]
    fn cancellation_wakes_waiters_and_does_not_publish_partial_files() {
        let directory = tempfile::tempdir().unwrap();
        let mut raw = Counting::new();
        raw.delay = Duration::from_millis(200);
        let inner = Arc::new(raw);
        let cache = Arc::new(SharedMedia::new(inner.clone(), 1024).unwrap());
        let runner = {
            let cache = cache.clone();
            let destination = directory.path().to_owned();
            thread::spawn(move || run_four(cache, &destination))
        };
        while inner.requests() == 0 {
            thread::yield_now();
        }
        cache.cancel();
        assert!(runner
            .join()
            .unwrap()
            .into_iter()
            .all(|r| matches!(r, Err(Error::Stop(_)))));
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 0);
        assert_eq!(cache.metrics()["shared_media"]["reserved_bytes"], 0);
    }
    #[test]
    fn destination_io_failure_does_not_destroy_shared_original() {
        let directory = tempfile::tempdir().unwrap();
        let inner = Arc::new(Counting::new());
        let cache = SharedMedia::new(inner.clone(), 1024).unwrap();
        let occupied = directory.path().join("occupied");
        fs::create_dir(&occupied).unwrap();
        assert!(cache
            .download("https://i.pximg.net/same.jpg", &occupied)
            .is_err());
        cache
            .download(
                "https://i.pximg.net/same.jpg",
                &directory.path().join("good.jpg"),
            )
            .unwrap();
        assert_eq!(inner.requests(), 1);
        assert_eq!(
            fs::read(directory.path().join("good.jpg")).unwrap(),
            inner.bytes
        );
    }
}
