use crate::{
    archive::{atomic, discover, fingerprint, now, safe_path, Archive, SharedArchive},
    collection,
    error::{invalid, Error, Result, StopReason},
    media::SharedMedia,
    target::Target,
    transport::{Fixture, Online, Transport, DEMO},
};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, HashSet, VecDeque},
    fs,
    path::PathBuf,
    sync::{atomic::AtomicBool, mpsc, Arc, Mutex},
    thread,
    time::{Duration, Instant},
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
    pub balanced: bool,
    pub resume: bool,
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
            balanced: false,
            resume: false,
        }
    }
}
#[derive(Clone, Debug)]
pub enum Event {
    Message(String),
    BodyReady {
        key: String,
    },
    Phase {
        key: String,
        status: String,
    },
    Metrics(Value),
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

/// A target snapshot is separate from per-work checkpoints: a crash cannot erase
/// the remaining IDs, and a resume never silently refreshes an author's list.
fn snapshot(options: &Options, refs: &[Target], source: &[String]) -> Value {
    json!({"schema_version":1,"discovery_complete":true,"created_at":now(),"mode":if options.online {"online"}else{"fixture"},"source_targets":source,"targets":refs.iter().map(Target::label).collect::<Vec<_>>(),"types":options.types,"jobs":options.jobs,"request_budget":options.request_budget,"balanced":options.balanced,"interval":options.interval})
}
fn resume_targets(options: &Options) -> Result<(Vec<Target>, Vec<String>)> {
    if !options.targets.is_empty() || options.input.is_some() {
        return Err(invalid(
            "--resume uses saved targets; do not supply URLs or --input",
        ));
    }
    let root = options.out.canonicalize()?;
    let job = safe_path(&root, "job.json")?;
    let plan = safe_path(&root, "plan.json")?;
    let path = if plan.exists()
        && (!job.exists() || fs::metadata(&plan)?.modified()? > fs::metadata(&job)?.modified()?)
    {
        plan
    } else {
        job
    };
    if fs::metadata(&path)?.len() > 16 * 1024 * 1024 {
        return Err(invalid("Saved job exceeds 16 MiB"));
    }
    let v: Value = serde_json::from_slice(&fs::read(path)?)?;
    if v["schema_version"] != 1 || v["mode"] != if options.online { "online" } else { "fixture" } {
        return Err(invalid("Saved job format or online/fixture mode differs"));
    }
    if v["discovery_complete"] != true {
        return Err(invalid("Saved selection is incomplete; run the source URLs again instead of resuming an older job"));
    }
    let items = v["targets"]
        .as_array()
        .ok_or_else(|| invalid("Invalid saved targets"))?;
    if items.len() > 100_000 {
        return Err(invalid("Saved job exceeds 100000 works"));
    }
    let refs = items
        .iter()
        .map(|v| Target::parse(v.as_str().ok_or_else(|| invalid("Invalid saved target"))?))
        .collect::<Result<Vec<_>>>()?;
    if refs
        .iter()
        .any(|t| !matches!(t.kind.as_str(), "art" | "novel"))
    {
        return Err(invalid("Saved job must contain concrete work IDs"));
    }
    let mut seen = HashSet::new();
    if refs.iter().any(|t| !seen.insert(t.clone())) {
        return Err(invalid("Duplicate saved target"));
    }
    let sources = v["source_targets"]
        .as_array()
        .map(|v| {
            v.iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();
    Ok((refs, sources))
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
        || !(1..=100_000).contains(&options.request_budget)
        || !["all", "art", "novel"].contains(&options.types.as_str())
    {
        return Err(invalid(
            "Use jobs 1–4, limit 1–1000, request budget 1–100000, file size 1–1024 MiB",
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
    // Validate all external input before creating any archive files.
    let resumed = if options.resume {
        Some(resume_targets(&options)?)
    } else {
        None
    };
    let input = if options.resume {
        Vec::new()
    } else {
        targets(&options)?
    };
    let inner: Arc<dyn Transport> = if options.online {
        Arc::new(Online::with_profile(
            options.interval,
            options.request_budget,
            options.max_file_mib * 1024 * 1024,
            stop.clone(),
            options.balanced,
        )?)
    } else {
        let source = if let Some(path) = &options.fixture {
            fs::read_to_string(path)?
        } else {
            DEMO.into()
        };
        Arc::new(Fixture::with_budget(
            &source,
            stop.clone(),
            options.request_budget,
        )?)
    };
    let client: Arc<dyn Transport> =
        Arc::new(SharedMedia::new(inner, options.max_file_mib * 1024 * 1024)?);
    let archive = Arc::new(Archive::open(&options.out, false)?);
    emit(Event::ArchiveOpened(archive.root.clone()));
    let sources: Vec<String> = resumed
        .as_ref()
        .map(|(_, s)| s.clone())
        .unwrap_or_else(|| input.iter().map(Target::label).collect());
    let mut report = json!({"tool_version":env!("CARGO_PKG_VERSION"),"started_at":now(),"mode":if options.online{"anonymous_online_unvalidated"}else{"synthetic_offline"},"targets":sources,"target":if sources.len()==1 {sources[0].clone()}else{"batch".into()},"jobs":options.jobs,"limit":if options.all{Value::Null}else{json!(options.limit)},"resumed":options.resume,"request_budget":options.request_budget,"balanced":options.balanced,"status":"running","results":[],"body_ready_count":0,"full_complete_count":0,"stop_reason":Value::Null});
    let mut collected = collection::Report::default();
    emit(Event::Message(
        if options.resume {
            "保存した対象一覧から再開します。作者一覧は再取得しません。"
        } else {
            "対象作品を確認しています…"
        }
        .into(),
    ));
    let action = (|| -> Result<()> {
        // Supersede old work immediately. A failed new discovery must never
        // silently resume an unrelated old job or plan.
        let selection_path = safe_path(&archive.root, if plan { "plan.json" } else { "job.json" })?;
        if !options.resume {
            let mut incomplete = snapshot(&options, &[], &sources);
            incomplete["discovery_complete"] = json!(false);
            atomic(&selection_path, &serde_json::to_vec_pretty(&incomplete)?)?;
        }
        let mut refs = Vec::new();
        let mut seen = HashSet::new();
        if let Some((saved, _)) = resumed {
            refs = saved;
        } else {
            for target in &input {
                for r in discover(client.as_ref(), target, &options.types)? {
                    if seen.insert(r.clone()) {
                        refs.push(r);
                    }
                }
            }
        }
        if refs.len() > 100_000 {
            return Err(invalid(
                "Discovered more than 100000 works; narrow selection",
            ));
        }
        report["discovered"] = json!(refs.len());
        let mut remaining = Vec::new();
        let mut existing = Vec::new();
        let mut excluded = 0;
        let mut excluded_targets = Vec::new();
        for r in refs {
            if client.stopped() {
                return Err(Error::Stop("Stopped while checking saved works".into()));
            }
            if archive.complete(&r)? {
                emit(Event::Existing {
                    key: r.key(),
                    status: "already_complete".into(),
                });
                existing.push(r);
            } else if !options.retry_unavailable
                && matches!(
                    archive.status(&r)?.as_deref(),
                    Some("skipped" | "unavailable")
                )
            {
                excluded += 1;
                excluded_targets.push(r);
            } else {
                remaining.push((archive.status(&r)?.is_some(), r));
            }
        }
        // Restore incomplete snapshots before adding new work, without refreshing them.
        remaining.sort_by_key(|(attempted, _)| !*attempted);
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
        let mut intended = existing.clone();
        intended.extend(selected.iter().cloned());
        intended.extend(excluded_targets);
        let record = snapshot(&options, &intended, &sources);
        atomic(&selection_path, &serde_json::to_vec_pretty(&record)?)?;
        report["already_complete"] = json!(existing.len());
        report["previous_exclusions"] = json!(excluded);
        report["selected"] = json!(selected.len());
        report["deferred_by_limit"] = json!(deferred);
        report["full_complete_count"] = json!(existing.len());
        report["body_ready_count"] = json!(existing.iter().filter(|t| t.kind == "novel").count());
        // Bounds are deliberately labeled estimates: covers and inserted assets are unknown.
        report["estimated_minimum_requests_remaining"] = json!(selected
            .iter()
            .filter(|t| archive.manifest(t).is_none())
            .count());
        report["estimated_typical_requests_remaining"] = json!(selected
            .iter()
            .map(|t| if t.kind == "novel" { 2 } else { 3 })
            .sum::<usize>());
        emit(Event::Selected {
            total: selected.len(),
            already: existing.len(),
            deferred,
            excluded,
        });
        emit(Event::Message(format!(
            "対象 {} / 要求予算 {} / 本文・表紙の概算 {} 要求（挿絵・多ページは追加）",
            selected.len(),
            options.request_budget,
            report["estimated_typical_requests_remaining"]
        )));
        if plan {
            report["selected_targets"] =
                json!(selected.iter().map(Target::label).collect::<Vec<_>>());
            report["status"] = json!("planned");
            return Ok(());
        }
        schedule_core(
            &archive,
            &client,
            &selected,
            &existing,
            options.jobs,
            options.request_budget,
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
        let reason = match &e {
            Error::Budget => StopReason::RequestBudget,
            Error::Stop(_) => client.stop_reason().unwrap_or(StopReason::UserCancel),
            Error::Io(_) | Error::Sql(_) => StopReason::DiskError,
            _ => StopReason::WorkerError,
        };
        report["stop_reason"] = json!(reason);
        report["status"] = json!(if matches!(e, Error::Budget | Error::Stop(_)) {
            "paused"
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
    report["transport_metrics"] = client.metrics();
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

#[derive(Clone, Copy)]
enum Stage {
    Prepare,
    Finish,
}

#[cfg(test)]
fn schedule(
    archive: &SharedArchive,
    client: &Arc<dyn Transport>,
    selected: &[Target],
    jobs: usize,
    report: &mut Value,
    collected: &mut collection::Report,
    emit: &(impl Fn(Event) + Sync),
) -> Result<()> {
    schedule_core(
        archive,
        client,
        selected,
        &[],
        jobs,
        100_000,
        report,
        collected,
        emit,
    )
}

#[allow(clippy::too_many_arguments)]
fn schedule_core(
    archive: &SharedArchive,
    client: &Arc<dyn Transport>,
    selected: &[Target],
    existing: &[Target],
    jobs: usize,
    budget: usize,
    report: &mut Value,
    collected: &mut collection::Report,
    emit: &(impl Fn(Event) + Sync),
) -> Result<()> {
    let (tasks, work) = mpsc::sync_channel::<(Target, Stage)>(jobs * 2);
    let work = Arc::new(Mutex::new(work));
    let (done_tx, done_rx) = mpsc::channel();
    let (copies, copy_rx) = mpsc::sync_channel::<(Target, Option<Value>)>(8);
    let (copy_done, copy_results) = mpsc::channel();
    let mut pending = VecDeque::new();
    let mut next = 0;
    let mut active = 0;
    let mut failure: Option<Error> = None;
    let mut body_keys = HashSet::new();
    let mut full = existing.len();
    let mut last_metrics = Instant::now();
    let mut prepare_seconds = 0.0;
    let mut assets_seconds = 0.0;
    let mut collection_seconds = 0.0;
    // A bounded, independent collection stage avoids occupying network work slots.
    // Only one copy for each novel is scheduled, after its body checkpoint.
    thread::scope(|scope| -> Result<()> {
        let a = archive.clone();
        let c = client.clone();
        let copier = scope.spawn(move || {
            for (target, manifest) in copy_rx {
                let started = Instant::now();
                let r = collection::work(&a.root, &target, manifest, &|| c.stopped());
                if r.fatal_io {
                    c.cancel();
                }
                let _ = copy_done.send((r, started.elapsed().as_secs_f64()));
            }
        });
        let mut workers = Vec::new();
        for _ in 0..jobs {
            let work = work.clone();
            let a = archive.clone();
            let c = client.clone();
            let tx = done_tx.clone();
            workers.push(scope.spawn(move || loop {
                let task = work.lock().unwrap().recv();
                let Ok((target, stage)) = task else { break };
                let started = Instant::now();
                let result =
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| match stage {
                        Stage::Prepare => a.prepare(&target, c.as_ref()),
                        Stage::Finish => a.finish(&target, c.as_ref()),
                    }))
                    .unwrap_or_else(|_| {
                        c.cancel();
                        Err(invalid("Worker terminated unexpectedly"))
                    });
                if tx
                    .send((target, stage, result, started.elapsed().as_secs_f64()))
                    .is_err()
                {
                    break;
                }
            }));
        }
        drop(done_tx);
        let mut existing_next = 0;
        let mut prefer_finish = false;
        loop {
            while !client.stopped() && active < jobs {
                let allow_prepare = failure.is_none() && next < selected.len();
                let task = if !pending.is_empty()
                    && (!allow_prepare || prefer_finish || pending.len() >= jobs * 2)
                {
                    prefer_finish = false;
                    pending.pop_front().map(|t| (t, Stage::Finish))
                } else if allow_prepare {
                    let target = selected[next].clone();
                    next += 1;
                    emit(Event::Started(target.key()));
                    prefer_finish = true;
                    Some((target, Stage::Prepare))
                } else {
                    None
                };
                let Some(task) = task else { break };
                emit(Event::Phase {
                    key: task.0.key(),
                    status: match task.1 {
                        Stage::Prepare => "metadata",
                        Stage::Finish => "assets_pending",
                    }
                    .into(),
                });
                tasks
                    .send(task)
                    .map_err(|_| invalid("Worker queue closed"))?;
                active += 1;
            }
            if let Some(target) = existing.get(existing_next) {
                match copies.try_send((target.clone(), archive.manifest(target))) {
                    Ok(()) => existing_next += 1,
                    Err(mpsc::TrySendError::Full(_)) => {}
                    Err(mpsc::TrySendError::Disconnected(_)) => {
                        client.cancel();
                        return Err(invalid("Collection worker terminated"));
                    }
                }
            }
            if active == 0 {
                break;
            }
            match done_rx.recv_timeout(Duration::from_millis(100)) {
                Ok((target, stage, result, elapsed)) => {
                    match stage {
                        Stage::Prepare => prepare_seconds += elapsed,
                        Stage::Finish => assets_seconds += elapsed,
                    }
                    active -= 1;
                    let body_ready = target.kind == "novel"
                        && archive.manifest(&target).is_some_and(|m| {
                            ["source.txt", "plain.txt"].iter().all(|name| {
                                m["files"]
                                    .as_array()
                                    .and_then(|f| f.iter().find(|e| e["path"] == *name))
                                    .is_some_and(|e| {
                                        safe_path(
                                            &archive.root,
                                            &format!("works/{}/{}", target.key(), name),
                                        )
                                        .ok()
                                        .and_then(|p| fingerprint(&p).ok())
                                        .is_some_and(|(h, b)| e["sha256"] == h && e["bytes"] == b)
                                    })
                            })
                        });
                    if body_ready && body_keys.insert(target.key()) {
                        emit(Event::BodyReady { key: target.key() });
                        copies
                            .send((target.clone(), archive.manifest(&target)))
                            .map_err(|_| invalid("Collection worker terminated"))?;
                    }
                    if matches!(stage, Stage::Prepare)
                        && result.as_ref().is_ok_and(|s| s == "assets_pending")
                    {
                        pending.push_back(target);
                    } else {
                        let status = match result {
                            Ok(s) => s,
                            Err(e) => {
                                let status = if matches!(e, Error::Budget | Error::Stop(_)) {
                                    "paused"
                                } else {
                                    "failed"
                                };
                                // Budget exhaustion drains ready/local finishing stages.
                                if !matches!(e, Error::Budget) {
                                    client.cancel();
                                }
                                if failure.is_none()
                                    || matches!(failure, Some(Error::Budget))
                                    || matches!(e, Error::Io(_) | Error::Sql(_))
                                {
                                    failure = Some(e);
                                }
                                status.into()
                            }
                        };
                        if matches!(status.as_str(), "complete" | "already_complete") {
                            full += 1;
                        }
                        if target.kind == "art"
                            && matches!(
                                status.as_str(),
                                "complete" | "partial" | "already_complete"
                            )
                            && archive.manifest(&target).is_some_and(|m| {
                                m["files"].as_array().is_some_and(|f| {
                                    f.iter().any(|e| {
                                        e["path"].as_str().is_some_and(|p| {
                                            p.starts_with("images/") || p == "frames.zip"
                                        })
                                    })
                                })
                            })
                        {
                            copies
                                .send((target.clone(), archive.manifest(&target)))
                                .map_err(|_| invalid("Collection worker terminated"))?;
                        }
                        let reason = archive.reason(&target)?;
                        let omissions = archive
                            .manifest(&target)
                            .map(|m| m["omissions"].clone())
                            .unwrap_or(json!([]));
                        report["results"].as_array_mut().unwrap().push(json!({"key":target.key(),"status":status,"reason":reason,"omissions":omissions,"body_ready":body_ready}));
                        emit(Event::Progress {
                            key: target.key(),
                            status,
                            done: report["results"].as_array().unwrap().len(),
                            total: selected.len(),
                        });
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(_) => {
                    failure = Some(invalid("Worker channel closed"));
                    client.cancel();
                    break;
                }
            }
            for (r, elapsed) in copy_results.try_iter() {
                collection_seconds += elapsed;
                if r.fatal_io {
                    client.cancel();
                    failure = Some(Error::Io(std::io::Error::other(
                        "Collection filesystem operation failed",
                    )));
                }
                collected.merge(r);
            }
            if last_metrics.elapsed() >= Duration::from_millis(200) {
                emit(Event::Metrics(
                    json!({"requests":client.requests(),"request_budget":budget,"body_ready":body_keys.len()+existing.iter().filter(|t|t.kind=="novel").count(),"complete":full,"transport":client.metrics()}),
                ));
                last_metrics = Instant::now();
            }
        }
        drop(tasks);
        for target in &existing[existing_next..] {
            copies
                .send((target.clone(), archive.manifest(target)))
                .map_err(|_| invalid("Collection worker terminated"))?;
        }
        drop(copies);
        for worker in workers {
            if worker.join().is_err() {
                client.cancel();
                failure = Some(invalid("Worker terminated unexpectedly"));
            }
        }
        if copier.join().is_err() {
            failure = Some(invalid("Collection worker terminated unexpectedly"));
        }
        for (r, elapsed) in copy_results.try_iter() {
            collection_seconds += elapsed;
            if r.fatal_io {
                client.cancel();
                failure = Some(Error::Io(std::io::Error::other(
                    "Collection filesystem operation failed",
                )));
            }
            collected.merge(r);
        }
        report["stage_seconds"] = json!({"prepare_sum":prepare_seconds,"assets_sum":assets_seconds,"collection_sum":collection_seconds});
        report["body_ready_count"] =
            json!(body_keys.len() + existing.iter().filter(|t| t.kind == "novel").count());
        report["full_complete_count"] = json!(full);
        emit(Event::Metrics(
            json!({"requests":client.requests(),"request_budget":budget,"body_ready":report["body_ready_count"],"complete":full,"transport":client.metrics()}),
        ));
        // Prepared-but-not-finished snapshots stay in the durable manifest and are
        // not counted as finished results. The next resume checks them locally.
        if let Some(e) = failure {
            return Err(e);
        }
        if client.stopped() {
            return Err(Error::Stop("Stopped; completed works remain saved".into()));
        }
        if next < selected.len() || !pending.is_empty() {
            return Err(Error::Budget);
        }
        Ok(())
    })
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
            .position(|e| matches!(e, Event::Phase { status, .. } if status=="assets_pending"))
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

    #[test]
    fn saved_selection_resumes_without_rediscovery_or_widening_limit() {
        let temp = tempfile::tempdir().unwrap();
        let o = Options {
            demo: true,
            targets: vec!["user:9001".into()],
            limit: 1,
            out: temp.path().join("archive"),
            ..Default::default()
        };
        let first = run(o.clone(), false, Arc::default(), |_| {}).unwrap();
        assert_eq!(first["selected"], 1);
        let resumed = run(
            Options {
                targets: vec![],
                resume: true,
                all: true,
                ..o
            },
            false,
            Arc::default(),
            |_| {},
        )
        .unwrap();
        assert_eq!(resumed["discovered"], 1);
        assert_eq!(resumed["already_complete"], 1);
        assert_eq!(resumed["requests"], 0);
        assert_eq!(resumed["status"], "complete");
    }
    #[test]
    fn failed_new_discovery_never_resumes_unrelated_old_selection() {
        let temp = tempfile::tempdir().unwrap();
        let o = Options {
            demo: true,
            targets: vec!["art:1001".into()],
            out: temp.path().join("archive"),
            ..Default::default()
        };
        run(o.clone(), false, Arc::default(), |_| {}).unwrap();
        let failed = run(
            Options {
                targets: vec!["user:9999".into()],
                ..o.clone()
            },
            false,
            Arc::default(),
            |_| {},
        )
        .unwrap();
        assert_eq!(failed["status"], "failed");
        let error = run(
            Options {
                targets: vec![],
                resume: true,
                ..o
            },
            false,
            Arc::default(),
            |_| {},
        )
        .unwrap_err();
        assert!(error.to_string().contains("incomplete"));
    }
    #[test]
    fn cancelled_resume_preserves_the_saved_selection() {
        let temp = tempfile::tempdir().unwrap();
        let o = Options {
            demo: true,
            targets: vec!["art:1001".into()],
            out: temp.path().join("archive"),
            ..Default::default()
        };
        run(o.clone(), false, Arc::default(), |_| {}).unwrap();
        let before = fs::read(o.out.join("job.json")).unwrap();
        let r = run(
            Options {
                targets: vec![],
                resume: true,
                ..o.clone()
            },
            false,
            Arc::new(AtomicBool::new(true)),
            |_| {},
        )
        .unwrap();
        assert_eq!(r["stop_reason"]["kind"], "user_cancel");
        assert_eq!(fs::read(o.out.join("job.json")).unwrap(), before);
        let r = run(
            Options {
                targets: vec![],
                resume: true,
                ..o
            },
            false,
            Arc::default(),
            |_| {},
        )
        .unwrap();
        assert_eq!(r["requests"], 0);
    }
    #[test]
    fn latest_reviewed_plan_is_used_instead_of_older_job() {
        let temp = tempfile::tempdir().unwrap();
        let o = Options {
            demo: true,
            targets: vec!["art:1001".into()],
            out: temp.path().join("archive"),
            ..Default::default()
        };
        run(o.clone(), false, Arc::default(), |_| {}).unwrap();
        run(
            Options {
                targets: vec!["novel:1004".into()],
                ..o.clone()
            },
            true,
            Arc::default(),
            |_| {},
        )
        .unwrap();
        let file = fs::OpenOptions::new()
            .write(true)
            .open(o.out.join("plan.json"))
            .unwrap();
        file.set_times(
            fs::FileTimes::new()
                .set_modified(std::time::SystemTime::now() + Duration::from_secs(2)),
        )
        .unwrap();
        let r = run(
            Options {
                targets: vec![],
                resume: true,
                ..o
            },
            false,
            Arc::default(),
            |_| {},
        )
        .unwrap();
        assert_eq!(r["discovered"], 1);
        assert_eq!(r["results"][0]["key"], "novel_1004");
        assert_eq!(r["counts"]["complete"], 1);
    }

    #[test]
    fn collection_io_stops_with_disk_reason_while_original_remains_valid() {
        let temp = tempfile::tempdir().unwrap();
        let o = Options {
            demo: true,
            targets: vec!["art:1001".into()],
            out: temp.path().join("archive"),
            ..Default::default()
        };
        run(o.clone(), false, Arc::default(), |_| {}).unwrap();
        fs::remove_dir_all(o.out.join("works/image")).unwrap();
        fs::write(o.out.join("works/image"), b"blocking directory creation").unwrap();
        let r = run(o.clone(), false, Arc::default(), |_| {}).unwrap();
        assert_eq!(r["status"], "failed");
        assert_eq!(r["stop_reason"]["kind"], "disk_error");
        assert_eq!(r["requests"], 0);
        assert_eq!(
            Archive::open(&o.out, true).unwrap().verify().unwrap()["valid"],
            1
        );
    }
    use std::sync::{atomic::Ordering, Mutex};
}
