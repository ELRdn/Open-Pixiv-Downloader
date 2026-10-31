use crate::{
    archive::{atomic, discover, now, safe_path, Archive, SharedArchive},
    collection,
    error::{invalid, Error, Result},
    target::Target,
    transport::{Fixture, Online, Transport, DEMO},
};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, HashSet},
    fs,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc,
    },
    thread,
    time::Instant,
};

#[derive(Clone, Debug)]
pub struct Options {
    pub targets: Vec<String>,
    pub input: Option<PathBuf>,
    pub out: PathBuf,
    pub jobs: usize,
    pub all: bool,
    pub limit: usize,
    pub types: String,
    pub online: bool,
    pub demo: bool,
    pub fixture: Option<PathBuf>,
    pub interval: f64,
    pub request_budget: usize,
    pub max_file_mib: u64,
    pub retry_unavailable: bool,
}
impl Default for Options {
    fn default() -> Self {
        Self {
            targets: vec![],
            input: None,
            out: "archive".into(),
            jobs: 2,
            all: false,
            limit: 100,
            types: "all".into(),
            online: false,
            demo: false,
            fixture: None,
            interval: 2.0,
            request_budget: 500,
            max_file_mib: 256,
            retry_unavailable: false,
        }
    }
}
#[derive(Clone, Debug)]
pub enum Event {
    Message(String),
    Selected {
        total: usize,
        already: usize,
        deferred: usize,
        excluded: usize,
    },
    Started(String),
    Progress {
        key: String,
        status: String,
        done: usize,
        total: usize,
    },
    Finished(Value),
    Failed(String),
    ArchiveOpened(PathBuf),
    Existing {
        key: String,
        status: String,
    },
}
pub fn targets(options: &Options) -> Result<Vec<Target>> {
    let mut values = options.targets.clone();
    if let Some(input) = &options.input {
        if fs::metadata(input)?.len() > 2 * 1024 * 1024 {
            return Err(invalid("Input list exceeds 2 MiB"));
        }
        values.extend(
            fs::read_to_string(input)?
                .trim_start_matches('\u{feff}')
                .lines()
                .map(str::trim)
                .filter(|s| !s.is_empty() && !s.starts_with('#'))
                .map(str::to_owned),
        );
    }
    if values.is_empty() && options.demo {
        values.push("user:9001".into());
    }
    if values.is_empty() || values.len() > 1000 {
        return Err(invalid("Supply between 1 and 1000 targets"));
    }
    let mut seen = HashSet::new();
    values
        .iter()
        .map(|s| Target::parse(s))
        .collect::<Result<Vec<_>>>()
        .map(|v| v.into_iter().filter(|t| seen.insert(t.clone())).collect())
}

pub fn run(
    options: Options,
    plan: bool,
    stop: Arc<AtomicBool>,
    emit: impl Fn(Event) + Sync,
) -> Result<Value> {
    let started = Instant::now();
    if !(1..=4).contains(&options.jobs)
        || !(1..=1000).contains(&options.limit)
        || !(1..=1024).contains(&options.max_file_mib)
        || !(1..=5000).contains(&options.request_budget)
    {
        return Err(invalid(
            "Use jobs 1–4, limit 1–1000, request budget 1–5000, file size 1–1024 MiB",
        ));
    }
    if usize::from(options.online)
        + usize::from(options.demo)
        + usize::from(options.fixture.is_some())
        != 1
    {
        return Err(invalid(
            "Choose exactly one of --online, --demo or --fixture",
        ));
    }
    let targets = targets(&options)?;
    let client: Arc<dyn Transport> = if options.online {
        Arc::new(Online::new(
            options.interval,
            options.request_budget,
            options.max_file_mib * 1024 * 1024,
            stop.clone(),
        )?)
    } else {
        let source = if let Some(path) = &options.fixture {
            fs::read_to_string(path)?
        } else {
            DEMO.into()
        };
        Arc::new(Fixture::new(&source, stop.clone())?)
    };
    let archive = Arc::new(Archive::open(&options.out, false)?);
    emit(Event::ArchiveOpened(archive.root.clone()));
    let mut report = json!({"tool_version":env!("CARGO_PKG_VERSION"),"started_at":now(),"mode":if options.online {"anonymous_online_unvalidated"} else {"synthetic_offline"},"targets":targets.iter().map(Target::label).collect::<Vec<_>>(),"target":if targets.len()==1 {targets[0].label()} else {"batch".into()},"jobs":options.jobs,"limit":if options.all {Value::Null} else {json!(options.limit)},"status":"running","results":[]});
    let mut collected = collection::Report::default();
    emit(Event::Message("対象作品を確認しています…".into()));
    let action = (|| -> Result<()> {
        let mut refs = Vec::new();
        let mut seen = HashSet::new();
        for target in targets {
            for r in discover(client.as_ref(), &target, &options.types)? {
                if seen.insert(r.clone()) {
                    refs.push(r);
                }
            }
        }
        report["discovered"] = json!(refs.len());
        let mut remaining = Vec::new();
        let mut already = 0;
        let mut excluded = 0;
        for r in refs {
            if archive.complete(&r)? {
                already += 1;
                emit(Event::Existing {
                    key: r.key(),
                    status: "already_complete".into(),
                });
                if !plan {
                    collected.merge(archive.collect_work(&r, &|| client.stopped()));
                    if collected.cancelled {
                        return Err(Error::Stop("集約を中断しました。".into()));
                    }
                }
            } else if !options.retry_unavailable
                && matches!(
                    archive.status(&r)?.as_deref(),
                    Some("skipped" | "unavailable")
                )
            {
                excluded += 1;
            } else {
                remaining.push((archive.status(&r)?.is_some(), r));
            }
        }
        remaining.sort_by_key(|(attempted, _)| *attempted);
        let deferred = if options.all {
            0
        } else {
            remaining.len().saturating_sub(options.limit)
        };
        let selected: Vec<_> = remaining
            .into_iter()
            .take(if options.all {
                usize::MAX
            } else {
                options.limit
            })
            .map(|(_, r)| r)
            .collect();
        report["already_complete"] = json!(already);
        report["previous_exclusions"] = json!(excluded);
        report["selected"] = json!(selected.len());
        report["deferred_by_limit"] = json!(deferred);
        emit(Event::Selected {
            total: selected.len(),
            already,
            deferred,
            excluded,
        });
        if plan {
            report["selected_targets"] =
                json!(selected.iter().map(Target::label).collect::<Vec<_>>());
            report["status"] = json!("planned");
            return Ok(());
        }
        schedule(
            &archive,
            &client,
            &selected,
            options.jobs,
            &mut report,
            &mut collected,
            &emit,
        )?;
        let complete = report["results"]
            .as_array()
            .unwrap()
            .iter()
            .all(|v| matches!(v["status"].as_str(), Some("complete" | "already_complete")));
        report["status"] = json!(if complete && deferred == 0 && excluded == 0 {
            "complete"
        } else {
            "partial"
        });
        Ok(())
    })();
    if let Err(e) = action {
        report["status"] = json!(if matches!(e, Error::Stop(_)) {
            if stop.load(Ordering::SeqCst) {
                "paused"
            } else {
                "failed"
            }
        } else {
            "failed"
        });
        report["reason"] = json!(e.to_string());
    }
    if !plan {
        if let Err(e) = collected.write(&archive.root) {
            collected.problem("", "collection-report.json", e);
        }
        report["collection"] = collected.value(&archive.root)?;
        if !collected.problems.is_empty() && report["status"] == "complete" {
            report["archive_status"] = json!("complete");
            report["status"] = json!("partial");
        }
    }
    let results = report["results"].as_array().unwrap();
    let result_count = results.len();
    let mut counts = BTreeMap::new();
    for r in results {
        *counts
            .entry(r["status"].as_str().unwrap_or("failed").to_owned())
            .or_insert(0) += 1;
    }
    report["counts"] = json!(counts);
    report["unattempted_selected"] = json!(report["selected"]
        .as_u64()
        .unwrap_or(0)
        .saturating_sub(result_count as u64));
    report["finished_at"] = json!(now());
    report["requests"] = json!(client.requests());
    report["rate_wait_seconds"] = json!(client.rate_wait().as_secs_f64());
    report["request_interval_seconds"] = json!(if options.online {
        options.interval
    } else {
        0.0
    });
    report["catalog_entries"] = json!(archive.catalog()?);
    report["elapsed_seconds"] = json!(started.elapsed().as_secs_f64());
    let data = serde_json::to_vec_pretty(&report)?;
    atomic(&safe_path(&archive.root, "last-run.json")?, &data)?;
    let name = format!(
        "runs/{}.json",
        report["started_at"].as_str().unwrap().replace(':', "-")
    );
    atomic(&safe_path(&archive.root, &name)?, &data)?;
    Ok(report)
}

fn schedule(
    archive: &SharedArchive,
    client: &Arc<dyn Transport>,
    selected: &[Target],
    jobs: usize,
    report: &mut Value,
    collected: &mut collection::Report,
    emit: &(impl Fn(Event) + Sync),
) -> Result<()> {
    let (tx, rx) = mpsc::channel();
    let mut next = 0;
    let mut active = 0;
    let mut handles = Vec::new();
    let mut failure = None;
    loop {
        while failure.is_none() && !client.stopped() && next < selected.len() && active < jobs {
            let target = selected[next].clone();
            next += 1;
            active += 1;
            emit(Event::Started(target.key()));
            let a = archive.clone();
            let c = client.clone();
            let tx = tx.clone();
            handles.push(thread::spawn(move || {
                let result = a.save(&target, c.as_ref());
                let collected = if result.as_ref().is_ok_and(|s| {
                    matches!(s.as_str(), "complete" | "partial" | "already_complete")
                }) {
                    Some(a.collect_work(&target, &|| c.stopped()))
                } else {
                    None
                };
                let _ = tx.send((target, result, collected));
            }));
        }
        if active == 0 {
            break;
        }
        // A worker panic must close the channel rather than leave the receiver blocked.
        let (target, result, work_collection) = loop {
            match rx.recv_timeout(std::time::Duration::from_millis(100)) {
                Ok(v) => break v,
                Err(mpsc::RecvTimeoutError::Timeout) if handles.iter().any(|h| h.is_finished()) => {
                    if handles.iter().filter(|h| h.is_finished()).count() == handles.len() {
                        if let Ok(v) = rx.try_recv() {
                            break v;
                        }
                        client.cancel();
                        for h in handles {
                            let _ = h.join();
                        }
                        return Err(invalid(
                            "Worker terminated unexpectedly; completed works remain saved",
                        ));
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(_) => {
                    client.cancel();
                    return Err(invalid("Worker channel closed"));
                }
            }
        };
        active -= 1;
        if let Some(work_collection) = work_collection {
            collected.merge(work_collection);
        }
        let status = match result {
            Ok(s) => s,
            Err(e) => {
                client.cancel();
                let status = if matches!(e, Error::Stop(_)) {
                    "paused"
                } else {
                    "failed"
                };
                if failure.is_none() {
                    failure = Some(e);
                }
                status.into()
            }
        };
        let reason = archive.reason(&target)?;
        let omissions = if status == "partial" {
            archive
                .manifest(&target)
                .map(|m| m["omissions"].clone())
                .unwrap_or(json!([]))
        } else {
            json!([])
        };
        report["results"].as_array_mut().unwrap().push(
            json!({"key":target.key(),"status":status,"reason":reason,"omissions":omissions}),
        );
        emit(Event::Progress {
            key: target.key(),
            status,
            done: report["results"].as_array().unwrap().len(),
            total: selected.len(),
        });
    }
    for h in handles {
        if h.join().is_err() {
            return Err(invalid("Worker terminated unexpectedly"));
        }
    }
    if let Some(e) = failure {
        return Err(e);
    }
    if client.stopped() {
        return Err(Error::Stop("Stopped; completed works remain saved".into()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn automatic_collection_backfills_without_redownload_and_preserves_edited_copies() {
        let temp = tempfile::tempdir().unwrap();
        let out = temp.path().join("archive");
        let options = Options {
            demo: true,
            targets: vec!["art:1001".into()],
            out: out.clone(),
            ..Default::default()
        };
        let first = run(options.clone(), false, Arc::default(), |_| {}).unwrap();
        assert_eq!(first["collection"]["copied"], 1);
        let output = out.join("works/image/art_1001_p0000.png");
        fs::remove_file(&output).unwrap();
        let second = run(options.clone(), false, Arc::default(), |_| {}).unwrap();
        assert_eq!(second["requests"], 0);
        assert_eq!(second["already_complete"], 1);
        assert_eq!(second["collection"]["copied"], 1);
        fs::write(&output, b"my edited copy").unwrap();
        let third = run(options, false, Arc::default(), |_| {}).unwrap();
        assert_eq!(third["status"], "partial");
        assert_eq!(third["archive_status"], "complete");
        assert_eq!(third["collection"]["problems"].as_array().unwrap().len(), 1);
        assert_eq!(fs::read(output).unwrap(), b"my edited copy");
        assert_eq!(
            Archive::open(&out, true).unwrap().verify().unwrap()["valid"],
            1
        );
        let plan_out = temp.path().join("plan");
        run(
            Options {
                demo: true,
                out: plan_out.clone(),
                ..Default::default()
            },
            true,
            Arc::default(),
            |_| {},
        )
        .unwrap();
        assert!(!plan_out.join("works/image").exists());
    }
    struct OverlapTransport {
        fixture: Fixture,
        active: std::sync::atomic::AtomicUsize,
        peak: std::sync::atomic::AtomicUsize,
        arrived: (Mutex<usize>, std::sync::Condvar),
        denied: bool,
    }
    impl Transport for OverlapTransport {
        fn api(&self, path: &str, query: &[(&str, String)]) -> Result<Value> {
            if self.denied {
                self.cancel();
                return Err(Error::Stop("Synthetic HTTP 403".into()));
            }
            self.fixture.api(path, query)
        }
        fn download(&self, url: &str, path: &std::path::Path) -> Result<()> {
            let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
            self.peak.fetch_max(active, Ordering::SeqCst);
            let (mutex, cond) = &self.arrived;
            let mut arrived = mutex.lock().unwrap();
            *arrived += 1;
            cond.notify_all();
            let (arrived, timeout) = cond
                .wait_timeout_while(arrived, std::time::Duration::from_secs(3), |n| *n < 2)
                .unwrap();
            assert!(!timeout.timed_out(), "Media transfers failed to overlap");
            drop(arrived);
            let result = self.fixture.download(url, path);
            self.active.fetch_sub(1, Ordering::SeqCst);
            result
        }
        fn requests(&self) -> usize {
            self.fixture.requests()
        }
        fn stopped(&self) -> bool {
            self.fixture.stopped()
        }
        fn cancel(&self) {
            self.fixture.cancel();
        }
    }
    #[test]
    fn overlapping_media_and_no_replenishment_after_remote_stop() {
        for denied in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            let archive = Arc::new(Archive::open(temp.path(), false).unwrap());
            let t = Arc::new(OverlapTransport {
                fixture: Fixture::new(DEMO, Arc::default()).unwrap(),
                active: 0.into(),
                peak: 0.into(),
                arrived: (Mutex::new(0), std::sync::Condvar::new()),
                denied,
            });
            let client: Arc<dyn Transport> = t.clone();
            let selected = vec![
                Target::parse("art:1001").unwrap(),
                Target::parse("art:1002").unwrap(),
                Target::parse("novel:1004").unwrap(),
            ];
            let mut report = json!({"results":[]});
            let events = Mutex::new(Vec::new());
            let result = schedule(
                &archive,
                &client,
                &selected,
                2,
                &mut report,
                &mut collection::Report::default(),
                &|e| events.lock().unwrap().push(e),
            );
            if denied {
                assert!(matches!(result, Err(Error::Stop(_))));
                assert!(
                    events
                        .lock()
                        .unwrap()
                        .iter()
                        .filter(|e| matches!(e, Event::Started(_)))
                        .count()
                        <= 2
                );
                assert_eq!(archive.status(&selected[2]).unwrap(), None);
            } else {
                result.unwrap();
                assert_eq!(t.peak.load(Ordering::SeqCst), 2);
                assert_eq!(archive.verify().unwrap()["valid"], 3);
            }
        }
    }
    #[test]
    fn batch_dedup_global_limit_and_resume() {
        let temp = tempfile::tempdir().unwrap();
        let mut o = Options {
            demo: true,
            targets: vec!["user:9001".into(), "art:1001".into()],
            out: temp.path().join("archive"),
            limit: 2,
            ..Default::default()
        };
        let r = run(o.clone(), false, Arc::default(), |_| {}).unwrap();
        assert_eq!(r["discovered"], 4);
        assert_eq!(r["selected"], 2);
        assert_eq!(r["deferred_by_limit"], 2);
        assert_eq!(r["status"], "partial");
        o.all = true;
        let r = run(o.clone(), false, Arc::default(), |_| {}).unwrap();
        assert_eq!(r["already_complete"], 2);
        assert_eq!(r["selected"], 2);
        assert_eq!(r["status"], "complete");
        let r = run(o, false, Arc::default(), |_| {}).unwrap();
        assert_eq!(r["selected"], 0);
        assert_eq!(r["already_complete"], 4);
    }
    #[test]
    fn invalid_batch_validates_before_archive_creation() {
        let temp = tempfile::tempdir().unwrap();
        let out = temp.path().join("never-created");
        let o = Options {
            demo: true,
            targets: vec!["art:1".into(), "https://evil.example/a".into()],
            out: out.clone(),
            ..Default::default()
        };
        assert!(run(o, false, Arc::default(), |_| {}).is_err());
        assert!(!out.exists());
    }
    #[test]
    fn bounded_initial_workers_and_pre_cancelled_run() {
        let temp = tempfile::tempdir().unwrap();
        let o = Options {
            demo: true,
            jobs: 2,
            all: true,
            out: temp.path().join("archive"),
            ..Default::default()
        };
        let events = Mutex::new(Vec::new());
        let r = run(o.clone(), false, Arc::default(), |e| {
            events.lock().unwrap().push(e)
        })
        .unwrap();
        assert_eq!(r["counts"]["complete"], 4);
        let events = events.into_inner().unwrap();
        let first_progress = events
            .iter()
            .position(|e| matches!(e, Event::Progress { .. }))
            .unwrap();
        assert_eq!(
            events[..first_progress]
                .iter()
                .filter(|e| matches!(e, Event::Started(_)))
                .count(),
            2
        );
        let stop = Arc::new(AtomicBool::new(true));
        let r = run(o, false, stop, |_| {}).unwrap();
        assert_eq!(r["status"], "paused");
        assert_eq!(r["requests"], 0);
    }
    use std::sync::Mutex;
}
