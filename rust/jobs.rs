use crate::{
    archive::{atomic, fingerprint, now, safe_path, Archive, SharedArchive},
    collection,
    error::{invalid, Error, Result, StopReason},
    media::SharedMedia,
    target::{Target, MAX_WORKS},
    transport::{Fixture, Online, Transport, DEMO, MAX_JOBS, MAX_REQUEST_BUDGET},
};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, HashSet, VecDeque},
    fs,
    path::{Path, PathBuf},
    sync::{atomic::AtomicBool, mpsc, Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

pub const MAX_SELECTION_BYTES: u64 = 512 * 1024 * 1024;

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
    pub api_interval: f64,
    pub request_budget: usize,
    pub max_file_mib: u64,
    pub retry_unavailable: bool,
    pub balanced: bool,
    pub resume: bool,
    pub text_only: bool,
    pub novel_scan: bool,
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
            api_interval: 2.0,
            request_budget: 20_000,
            max_file_mib: 256,
            retry_unavailable: false,
            balanced: false,
            resume: false,
            text_only: false,
            novel_scan: false,
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
    json!({"schema_version":1,"discovery_complete":true,"discovery_resumable":false,"created_at":now(),"mode":if options.online {"online"}else{"fixture"},"source_targets":source,"targets":refs.iter().map(Target::label).collect::<Vec<_>>(),"all":options.all,"limit":options.limit,"types":options.types,"jobs":options.jobs,"request_budget":options.request_budget,"balanced":options.balanced,"interval":options.interval,"api_interval":options.api_interval,"text_only":options.text_only,"novel_scan":options.novel_scan})
}
#[derive(Debug)]
struct SavedSelection {
    refs: Vec<Target>,
    sources: Vec<String>,
    pending: Option<PendingDiscovery>,
}
#[derive(Debug)]
struct PendingDiscovery {
    generation: String,
    all: bool,
    limit: usize,
    types: String,
}
pub(crate) fn selection_path(root: &Path) -> Result<PathBuf> {
    let job = safe_path(root, "job.json")?;
    let plan = safe_path(root, "plan.json")?;
    Ok(
        if plan.exists()
            && (!job.exists()
                || fs::metadata(&plan)?.modified()? > fs::metadata(&job)?.modified()?)
        {
            plan
        } else {
            job
        },
    )
}
fn resume_targets(options: &Options) -> Result<SavedSelection> {
    if !options.targets.is_empty() || options.input.is_some() {
        return Err(invalid(
            "--resume uses saved targets; do not supply URLs or --input",
        ));
    }
    let root = options.out.canonicalize()?;
    let path = selection_path(&root)?;
    if fs::metadata(&path)?.len() > MAX_SELECTION_BYTES {
        return Err(invalid("Saved job exceeds 512 MiB"));
    }
    let v: Value = serde_json::from_slice(&fs::read(path)?)?;
    if v["schema_version"] != 1 || v["mode"] != if options.online { "online" } else { "fixture" } {
        return Err(invalid("Saved job format or online/fixture mode differs"));
    }
    let complete = v["discovery_complete"] == true;
    if v["text_only"] == true && !options.text_only {
        return Err(invalid("本文専用の保存対象です。F10で「本文のみ」を選ぶか --text-only を付けて再開してください。"));
    }
    if !complete && (v["discovery_complete"] != false || v["discovery_resumable"] != true) {
        return Err(invalid("Saved selection is incomplete; run the source URLs again instead of resuming an older job"));
    }
    let items = v["targets"]
        .as_array()
        .ok_or_else(|| invalid("Invalid saved targets"))?;
    if items.len() > MAX_WORKS {
        return Err(invalid(&format!("Saved job exceeds {MAX_WORKS} works")));
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
    let sources = match v.get("source_targets") {
        None if complete => Vec::new(),
        Some(Value::Array(sources)) => sources
            .iter()
            .map(|source| {
                source
                    .as_str()
                    .map(str::to_owned)
                    .ok_or_else(|| invalid("Invalid saved source target"))
            })
            .collect::<Result<Vec<_>>>()?,
        _ => return Err(invalid("Invalid saved source targets")),
    };
    let pending = if complete {
        None
    } else {
        if !refs.is_empty() || !(1..=1000).contains(&sources.len()) {
            return Err(invalid(
                "Incomplete selection requires empty targets and 1–1000 saved source URLs",
            ));
        }
        for source in &sources {
            Target::parse(source)?;
        }
        let generation = v["discovery_generation"]
            .as_str()
            .filter(|generation| {
                (1..=128).contains(&generation.len())
                    && generation
                        .bytes()
                        .all(|c| c.is_ascii_alphanumeric() || b".:+-".contains(&c))
            })
            .ok_or_else(|| invalid("Invalid or missing incomplete discovery generation"))?;
        let types = v["types"]
            .as_str()
            .filter(|types| ["all", "art", "novel"].contains(types))
            .ok_or_else(|| invalid("Invalid incomplete discovery types"))?;
        let all = v["all"]
            .as_bool()
            .ok_or_else(|| invalid("Invalid incomplete discovery all flag"))?;
        let limit = v["limit"]
            .as_u64()
            .filter(|limit| (1..=1000).contains(limit))
            .ok_or_else(|| invalid("Invalid incomplete discovery limit"))?;
        Some(PendingDiscovery {
            generation: generation.into(),
            all,
            limit: limit as usize,
            types: types.into(),
        })
    };
    Ok(SavedSelection {
        refs,
        sources,
        pending,
    })
}

pub(crate) fn validate_options(options: &Options) -> Result<()> {
    if !(1..=MAX_JOBS).contains(&options.jobs)
        || !(1..=1000).contains(&options.limit)
        || !(1..=1024).contains(&options.max_file_mib)
        || options.request_budget > MAX_REQUEST_BUDGET
        || !["all", "art", "novel"].contains(&options.types.as_str())
    {
        return Err(invalid(
            "Use jobs 1–32, limit 1–1000, request budget 0–1000000 (0 = unlimited), file size 1–1024 MiB",
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
    Ok(())
}

pub fn run(
    mut options: Options,
    plan: bool,
    stop: Arc<AtomicBool>,
    emit: impl Fn(Event) + Sync,
) -> Result<Value> {
    let started = Instant::now();
    validate_options(&options)?;
    // Fresh input must be valid before creating an archive; saved input needs its lock.
    let mut input = if options.resume {
        if !options.targets.is_empty() || options.input.is_some() {
            return Err(invalid(
                "--resume uses saved targets; do not supply URLs or --input",
            ));
        }
        Vec::new()
    } else {
        targets(&options)?
    };
    if options.text_only && input.iter().any(|target| target.kind == "art") {
        return Err(invalid(
            "本文のみモードは小説専用です。小説・作者・タグURLを指定してください。",
        ));
    }
    if input.iter().any(|target| target.kind == "novel_scan") {
        if input.len() != 1 || !options.text_only {
            return Err(invalid(
                "小説トップのID走査は単独URL・本文のみで実行してください。",
            ));
        }
        if plan {
            return crate::novel_scan::preview(&options);
        }
        options.targets.clear();
        options.input = None;
        options.resume = true;
        return crate::novel_scan::run(options, None, None, stop, emit);
    }
    let inner: Arc<dyn Transport> = if options.online {
        Arc::new(Online::with_profile(
            options.interval,
            options.request_budget,
            options.max_file_mib * 1024 * 1024,
            stop.clone(),
            options.balanced,
            options.api_interval,
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
    let mut archive = Archive::open(&options.out, options.resume)?;
    archive.text_only = options.text_only;
    let archive = Arc::new(archive);
    if options.resume && !options.novel_scan && crate::novel_scan::owns_selection(&archive.root)? {
        if !options.text_only {
            return Err(invalid(
                "小説のID走査はF10の本文のみ、または --text-only で再開してください。",
            ));
        }
        drop(archive);
        return crate::novel_scan::run(options, None, None, stop, emit);
    }
    let resumed = if options.resume {
        Some(resume_targets(&options)?)
    } else {
        None
    };
    if let Some(saved) = &resumed {
        if let Some(pending) = &saved.pending {
            options.all = pending.all;
            options.limit = pending.limit;
            options.types = pending.types.clone();
            input = saved
                .sources
                .iter()
                .map(|source| Target::parse(source))
                .collect::<Result<Vec<_>>>()?;
        }
    }
    if options.text_only {
        options.types = "novel".into();
    }
    let discovery_complete = resumed
        .as_ref()
        .is_some_and(|saved| saved.pending.is_none());
    let generation = resumed
        .as_ref()
        .and_then(|saved| saved.pending.as_ref())
        .map(|pending| pending.generation.clone())
        .unwrap_or_else(|| format!("{}-{}", now(), std::process::id()));
    emit(Event::ArchiveOpened(archive.root.clone()));
    let sources: Vec<String> = resumed
        .as_ref()
        .map(|saved| saved.sources.clone())
        .unwrap_or_else(|| input.iter().map(Target::label).collect());
    let mut report = json!({"tool_version":env!("CARGO_PKG_VERSION"),"started_at":now(),"mode":if options.online{"anonymous_online_unvalidated"}else{"synthetic_offline"},"targets":sources,"target":if sources.len()==1 {sources[0].clone()}else{"batch".into()},"jobs":options.jobs,"limit":if options.all{Value::Null}else{json!(options.limit)},"resumed":options.resume,"request_budget":options.request_budget,"balanced":options.balanced,"status":"running","results":[],"body_ready_count":0,"full_complete_count":0,"stop_reason":Value::Null});
    report["discovery_complete"] = json!(discovery_complete);
    report["text_only"] = json!(options.text_only);
    report["discovery_resumable"] = json!(!discovery_complete);
    report["selected"] = json!(0);
    report["previous_exclusions"] = json!(0);
    report["deferred_by_limit"] = json!(0);
    let mut collected = collection::Report::default();
    emit(Event::Message(
        if options.resume && !discovery_complete {
            "保存済みの列挙結果を再利用して、対象一覧の列挙を続けます。"
        } else if options.resume {
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
        if !discovery_complete {
            let mut incomplete = snapshot(&options, &[], &sources);
            incomplete["discovery_complete"] = json!(false);
            incomplete["discovery_resumable"] = json!(true);
            incomplete["discovery_generation"] = json!(generation);
            atomic(&selection_path, &serde_json::to_vec_pretty(&incomplete)?)?;
            archive.begin_discovery(&generation)?;
        }
        let mut refs = Vec::new();
        let mut seen = HashSet::new();
        if discovery_complete {
            refs = resumed.unwrap().refs;
        } else {
            for target in &input {
                let mut progress = |message| {
                    emit(Event::Message(message));
                    emit(Event::Metrics(
                        json!({"requests":client.requests(),"request_budget":options.request_budget,"transport":client.metrics()}),
                    ));
                };
                let discovered = archive.discover_target(
                    client.as_ref(),
                    target,
                    &options.types,
                    options.online,
                    &generation,
                    &mut progress,
                )?;
                for r in discovered {
                    if seen.insert(r.clone()) {
                        refs.push(r);
                        if refs.len() > MAX_WORKS {
                            return Err(invalid(&format!(
                                "Discovered more than {MAX_WORKS} works; narrow selection"
                            )));
                        }
                    }
                }
            }
        }
        if refs.len() > MAX_WORKS {
            return Err(invalid(&format!(
                "Discovered more than {MAX_WORKS} works; narrow selection"
            )));
        }
        if options.text_only {
            refs.retain(|target| target.kind == "novel");
        }
        let saved_refs = discovery_complete.then(|| refs.clone());
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
        let intended = saved_refs.unwrap_or_else(|| {
            let mut intended = existing.clone();
            intended.extend(selected.iter().cloned());
            intended.extend(excluded_targets);
            intended
        });
        let record = snapshot(&options, &intended, &sources);
        let saved = serde_json::to_vec_pretty(&record)?;
        if saved.len() as u64 > MAX_SELECTION_BYTES {
            return Err(invalid("Saved job exceeds 512 MiB"));
        }
        atomic(&selection_path, &saved)?;
        report["discovery_complete"] = json!(true);
        report["discovery_resumable"] = json!(false);
        if !discovery_complete {
            archive.finish_discovery(&generation)?;
        }
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
            .map(|t| if options.text_only {
                1
            } else if t.kind == "novel" {
                2
            } else {
                3
            })
            .sum::<usize>());
        emit(Event::Selected {
            total: selected.len(),
            already: existing.len(),
            deferred,
            excluded,
        });
        emit(Event::Message(format!(
            "対象 {} / 要求予算 {} / {}の概算 {} 要求",
            selected.len(),
            options.request_budget,
            if options.text_only {
                "本文のみ"
            } else {
                "本文・表紙（挿絵・多ページは追加）"
            },
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
            Error::Invalid(_) if report["discovery_complete"] != true => {
                StopReason::SearchIncomplete
            }
            _ => StopReason::WorkerError,
        };
        report["stop_reason"] = json!(reason);
        report["status"] = json!(if matches!(e, Error::Budget | Error::Stop(_)) {
            "paused"
        } else {
            "failed"
        });
        report["reason"] = json!(e.to_string());
        if report["discovery_complete"] == false && !matches!(e, Error::Budget | Error::Stop(_)) {
            report["discovery_resumable"] = json!(false);
            let mut incomplete = snapshot(&options, &[], &sources);
            incomplete["discovery_complete"] = json!(false);
            incomplete["discovery_resumable"] = json!(false);
            incomplete["discovery_generation"] = json!(generation);
            atomic(
                &safe_path(&archive.root, if plan { "plan.json" } else { "job.json" })?,
                &serde_json::to_vec_pretty(&incomplete)?,
            )?;
        }
    }
    if !plan && !options.text_only {
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
    report["elapsed_seconds"] = json!(started.elapsed().as_secs_f64());
    if options.text_only {
        let results = report.as_object_mut().unwrap().remove("results").unwrap();
        let data = serde_json::to_vec(&report)?;
        report["results"] = results;
        atomic(&safe_path(&archive.root, "last-run.json")?, &data)?;
        return Ok(report);
    }
    report["catalog_entries"] = json!(archive.catalog()?);
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
    let existing_novels = existing
        .iter()
        .filter(|target| target.kind == "novel")
        .count();
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
            if let Some(target) = existing.get(existing_next).filter(|_| !archive.text_only) {
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
                        && ((archive.text_only && archive.complete(&target)?)
                            || (!archive.text_only
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
                                                .is_some_and(|(h, b)| {
                                                    e["sha256"] == h && e["bytes"] == b
                                                })
                                            })
                                    })
                                })));
                    if body_ready && body_keys.insert(target.key()) {
                        emit(Event::BodyReady { key: target.key() });
                        if !archive.text_only {
                            copies
                                .send((target.clone(), archive.manifest(&target)))
                                .map_err(|_| invalid("Collection worker terminated"))?;
                        }
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
                    json!({"requests":client.requests(),"request_budget":budget,"body_ready":body_keys.len()+existing_novels,"complete":full,"transport":client.metrics()}),
                ));
                last_metrics = Instant::now();
            }
        }
        drop(tasks);
        for target in existing[existing_next..]
            .iter()
            .filter(|_| !archive.text_only)
        {
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
        report["body_ready_count"] = json!(body_keys.len() + existing_novels);
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
    fn novel_minimum_size_uses_utf8_bytes_and_skips_persist_across_resume() {
        for (bytes, with_nul) in [
            (0, false),
            (1023, false),
            (1024, false),
            (0, true),
            (1023, true),
            (1024, true),
        ] {
            let temp = tempfile::tempdir().unwrap();
            let out = temp.path().join("archive");
            fs::create_dir(&out).unwrap();
            fs::write(
                out.join("novel-policy.json"),
                br#"{"schema_version":1,"min_bytes":1024}"#,
            )
            .unwrap();
            let mut fixture: Value = serde_json::from_str(DEMO).unwrap();
            let text = if bytes == 0 {
                String::new()
            } else {
                format!(
                    "{}{}",
                    "あ".repeat(341),
                    if bytes == 1024 { "a" } else { "" }
                )
            };
            assert_eq!(text.len(), bytes);
            fixture["responses"]["/ajax/novel/1004"]["content"] = json!(if with_nul {
                format!("\0{text}\0")
            } else {
                text
            });
            let fixture_path = temp.path().join("fixture.json");
            fs::write(&fixture_path, fixture.to_string()).unwrap();
            let options = Options {
                targets: vec!["novel:1004".into()],
                fixture: Some(fixture_path),
                text_only: true,
                all: true,
                out: out.clone(),
                ..Default::default()
            };
            let first = run(options.clone(), false, Arc::default(), |_| {}).unwrap();
            assert_eq!(first["requests"], 1);
            let path = out.join("novel/novel_1004.txt");
            if bytes < 1024 {
                assert_eq!(first["counts"]["skipped"], 1);
                assert!(!path.exists());
                let archive = Archive::open(&out, true).unwrap();
                assert!(archive
                    .reason(&Target::new("novel", "1004").unwrap())
                    .unwrap()
                    .unwrap()
                    .contains(&format!("{bytes} < 1024")));
            } else {
                assert_eq!(first["counts"]["complete"], 1);
                assert_eq!(fs::read(&path).unwrap().len(), 1024);
                assert_eq!(
                    fs::read_to_string(&path).unwrap(),
                    format!("{}a", "あ".repeat(341))
                );
            }
            let second = run(
                Options {
                    targets: vec![],
                    resume: true,
                    ..options
                },
                false,
                Arc::default(),
                |_| {},
            )
            .unwrap();
            assert_eq!(second["requests"], 0);
            assert_eq!(
                second["previous_exclusions"],
                if bytes < 1024 { 1 } else { 0 }
            );
            assert_eq!(
                second["already_complete"],
                if bytes == 1024 { 1 } else { 0 }
            );
        }
    }

    #[test]
    fn minimum_policy_excludes_a_known_deleted_short_txt_without_http() {
        let temp = tempfile::tempdir().unwrap();
        let options = Options {
            targets: vec!["novel:1004".into()],
            demo: true,
            text_only: true,
            all: true,
            out: temp.path().to_owned(),
            ..Default::default()
        };
        let first = run(options.clone(), false, Arc::default(), |_| {}).unwrap();
        assert_eq!(first["counts"]["complete"], 1);
        let path = temp.path().join("novel/novel_1004.txt");
        assert!(fs::metadata(&path).unwrap().len() < 1024);
        fs::remove_file(&path).unwrap();
        fs::write(
            temp.path().join("novel-policy.json"),
            br#"{"schema_version":1,"min_bytes":1024}"#,
        )
        .unwrap();
        let second = run(options, false, Arc::default(), |_| {}).unwrap();
        assert_eq!(second["requests"], 0);
        assert_eq!(second["counts"]["skipped"], 1);
        assert!(!path.exists());
    }

    #[test]
    fn invalid_novel_policy_is_rejected_before_fetch() {
        for policy in [
            json!({"schema_version":2,"min_bytes":1024}),
            json!({"schema_version":1,"min_bytes":-1}),
            json!({"schema_version":1,"min_bytes":"1024"}),
            json!({"schema_version":1,"min_bytes":67108865}),
        ] {
            let temp = tempfile::tempdir().unwrap();
            fs::write(temp.path().join("novel-policy.json"), policy.to_string()).unwrap();
            assert!(Archive::open(temp.path(), false).is_err());
            assert!(!temp.path().join(".run.lock").exists());
        }
    }

    #[test]
    fn text_only_budget_resume_and_existing_txt_use_the_flat_path() {
        let temp = tempfile::tempdir().unwrap();
        let mut fixture: Value = serde_json::from_str(DEMO).unwrap();
        let mut second = fixture["responses"]["/ajax/novel/1004"].clone();
        second["id"] = json!("1005");
        fixture["responses"]["/ajax/novel/1005"] = second;
        let fixture_path = temp.path().join("fixture.json");
        fs::write(&fixture_path, fixture.to_string()).unwrap();
        let out = temp.path().join("archive");
        let options = Options {
            targets: vec!["novel:1004".into(), "novel:1005".into()],
            fixture: Some(fixture_path),
            text_only: true,
            jobs: 1,
            request_budget: 1,
            all: true,
            out: out.clone(),
            ..Default::default()
        };
        let report = run(options.clone(), false, Arc::default(), |_| {}).unwrap();
        assert_eq!(report["status"], "paused");
        assert_eq!(report["requests"], 1);
        assert_eq!(report["body_ready_count"], 1);
        let resumed = Options {
            targets: vec![],
            resume: true,
            ..options
        };
        let report = run(resumed.clone(), false, Arc::default(), |_| {}).unwrap();
        assert_eq!(report["status"], "complete");
        assert_eq!(report["requests"], 1);
        assert_eq!(report["already_complete"], 1);
        assert_eq!(report["body_ready_count"], 2);
        let report = run(resumed.clone(), false, Arc::default(), |_| {}).unwrap();
        assert_eq!(report["requests"], 0);
        assert_eq!(report["already_complete"], 2);
        assert_eq!(report["selected"], 0);
        assert!(out.join("novel/novel_1004.txt").is_file());
        assert!(out.join("novel/novel_1005.txt").is_file());
        for name in ["works", "runs", "catalog.jsonl", "collection-report.json"] {
            assert!(!out.join(name).exists(), "unexpected file: {name}");
        }
        let saved: Value =
            serde_json::from_slice(&fs::read(out.join("last-run.json")).unwrap()).unwrap();
        assert!(saved.get("results").is_none());
        assert!(run(
            Options {
                text_only: false,
                ..resumed
            },
            false,
            Arc::default(),
            |_| {}
        )
        .is_err());

        let out = temp.path().join("preexisting");
        fs::create_dir_all(out.join("novel")).unwrap();
        fs::write(out.join("novel/novel_1004.txt"), "既存本文").unwrap();
        let report = run(
            Options {
                targets: vec!["novel:1004".into()],
                demo: true,
                text_only: true,
                out,
                request_budget: MAX_REQUEST_BUDGET,
                ..Default::default()
            },
            false,
            Arc::default(),
            |_| {},
        )
        .unwrap();
        assert_eq!(report["requests"], 0);
        assert_eq!(report["already_complete"], 1);
        assert_eq!(report["selected"], 0);
    }

    #[test]
    fn text_only_author_discovery_excludes_artworks() {
        let temp = tempfile::tempdir().unwrap();
        let report = run(
            Options {
                targets: vec!["user:9001".into()],
                demo: true,
                text_only: true,
                all: true,
                out: temp.path().to_path_buf(),
                ..Default::default()
            },
            false,
            Arc::default(),
            |_| {},
        )
        .unwrap();
        assert_eq!(report["discovered"], 1);
        assert_eq!(report["selected"], 1);
        assert_eq!(report["status"], "complete");
        assert!(!temp.path().join("works").exists());
    }
    #[test]
    fn selection_above_a_million_and_64_mib_is_saved_and_resumed_without_network() {
        let temp = tempfile::tempdir().unwrap();
        let refs = (1..=1_512_674)
            .map(|id| Target::new("novel", &id.to_string()).unwrap())
            .collect::<Vec<_>>();
        let options = Options {
            demo: true,
            out: temp.path().into(),
            resume: true,
            ..Default::default()
        };
        let record = snapshot(
            &options,
            &refs,
            &["https://www.pixiv.net/tags/test/novels".into()],
        );
        let path = temp.path().join("job.json");
        let mut bytes = serde_json::to_vec_pretty(&record).unwrap();
        bytes.resize(64 * 1024 * 1024 + 1, b' ');
        atomic(&path, &bytes).unwrap();
        drop(bytes);
        drop(record);
        let saved = resume_targets(&options).unwrap();
        assert!(saved.pending.is_none());
        assert_eq!(saved.refs.len(), 1_512_674);
        assert_eq!(saved.refs.first(), refs.first());
        assert_eq!(saved.refs.last(), refs.last());
        assert!(!crate::novel_scan::owns_selection(temp.path()).unwrap());
        fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_len(MAX_SELECTION_BYTES + 1)
            .unwrap();
        assert!(resume_targets(&options)
            .unwrap_err()
            .to_string()
            .contains("512 MiB"));
    }
    #[test]
    fn pending_discovery_rejects_malformed_scope_but_accepts_complete_legacy() {
        let temp = tempfile::tempdir().unwrap();
        let options = Options {
            demo: true,
            out: temp.path().into(),
            resume: true,
            ..Default::default()
        };
        let mut record = snapshot(&options, &[], &["user:9001".into()]);
        record["discovery_complete"] = json!(false);
        record["discovery_resumable"] = json!(true);
        record["discovery_generation"] = json!("2026-11-11T00:00:00.123+00:00-123");
        let path = temp.path().join("job.json");
        atomic(&path, &serde_json::to_vec(&record).unwrap()).unwrap();
        assert!(resume_targets(&options).unwrap().pending.is_some());
        for (field, value) in [
            ("source_targets", Value::Null),
            ("source_targets", json!("user:9001")),
            ("source_targets", json!([])),
            ("source_targets", json!(["user:9001", 1])),
            ("source_targets", json!(["https://evil.example/a"])),
            ("source_targets", json!(vec!["user:9001"; 1001])),
            ("targets", json!(["art:1001"])),
            ("discovery_resumable", json!(false)),
            ("discovery_resumable", json!("true")),
            ("discovery_complete", Value::Null),
            ("discovery_generation", Value::Null),
            ("discovery_generation", json!("")),
            ("discovery_generation", json!("a".repeat(129))),
            ("discovery_generation", json!("../old/job")),
            ("discovery_generation", json!("old_generation")),
            ("discovery_generation", json!("old generation")),
            ("discovery_generation", json!("世代1")),
            ("types", Value::Null),
            ("types", json!("video")),
            ("all", Value::Null),
            ("all", json!("true")),
            ("limit", Value::Null),
            ("limit", json!(0)),
            ("limit", json!(1001)),
            ("limit", json!(1.5)),
            ("limit", json!("1")),
        ] {
            let mut invalid = record.clone();
            invalid[field] = value;
            atomic(&path, &serde_json::to_vec(&invalid).unwrap()).unwrap();
            assert!(
                resume_targets(&options).is_err(),
                "accepted {field}: {invalid}"
            );
        }
        record
            .as_object_mut()
            .unwrap()
            .remove("discovery_generation");
        atomic(&path, &serde_json::to_vec(&record).unwrap()).unwrap();
        assert!(resume_targets(&options).is_err());
        record["discovery_complete"] = json!(true);
        record["targets"] = json!(["art:1001"]);
        for field in ["all", "limit", "discovery_resumable", "source_targets"] {
            record.as_object_mut().unwrap().remove(field);
        }
        atomic(&path, &serde_json::to_vec(&record).unwrap()).unwrap();
        let saved = resume_targets(&options).unwrap();
        assert!(saved.pending.is_none());
        assert_eq!(saved.refs, vec![Target::parse("art:1001").unwrap()]);
        record["source_targets"] = json!(["user:9001", false]);
        atomic(&path, &serde_json::to_vec(&record).unwrap()).unwrap();
        assert!(resume_targets(&options).is_err());
    }
    #[test]
    fn mixed_user_search_budget_one_resumes_with_original_scope() {
        for all in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            let mut data: Value = serde_json::from_str(DEMO).unwrap();
            let user = "/ajax/user/9001/profile/all";
            let art =
                "/ajax/search/artworks/test?word=test&order=date_d&mode=safe&s_mode=s_tag_full&p=1";
            let novel =
                "/ajax/search/novels/test?word=test&order=date_d&mode=safe&s_mode=s_tag_full&p=1";
            data["responses"][user] = json!({"illusts":{"1003":null},"manga":{},"novels":{}});
            data["responses"][art] = json!({"illustManga":{"data":[{"id":"1001"}],"total":1}});
            data["responses"][novel] = json!({"novel":{"data":[{"id":"1004"}],"total":1}});
            let fixture = temp.path().join("fixture.json");
            fs::write(&fixture, serde_json::to_vec(&data).unwrap()).unwrap();
            let options = Options {
                targets: vec!["user:9001".into(), "https://www.pixiv.net/tags/test".into()],
                fixture: Some(fixture.clone()),
                out: temp.path().join("archive"),
                all,
                limit: 1,
                request_budget: 1,
                ..Default::default()
            };
            let first = run(options.clone(), true, Arc::default(), |_| {}).unwrap();
            assert_eq!(first["status"], "paused");
            assert_eq!(first["requests"], 1);
            assert_eq!(first["discovery_resumable"], true);
            let path = options.out.join("plan.json");
            let pending: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
            let generation = pending["discovery_generation"].clone();
            assert_eq!(pending["targets"], json!([]));
            data["responses"][user]["illusts"] = json!({"1002":null});
            fs::write(&fixture, serde_json::to_vec(&data).unwrap()).unwrap();
            let resumed = Options {
                targets: vec![],
                resume: true,
                all: !all,
                limit: 1000,
                types: "novel".into(),
                balanced: true,
                interval: 0.4,
                api_interval: 0.7,
                ..options
            };
            let second = run(resumed.clone(), true, Arc::default(), |_| {}).unwrap();
            assert_eq!(second["status"], "paused");
            assert_eq!(second["requests"], 1);
            let pending: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
            assert_eq!(pending["discovery_generation"], generation);
            assert_eq!(pending["all"], all);
            assert_eq!(pending["limit"], 1);
            assert_eq!(pending["types"], "all");
            assert_eq!(pending["balanced"], true);
            assert_eq!(pending["interval"], 0.4);
            assert_eq!(pending["api_interval"], 0.7);
            data["responses"][art]["illustManga"]["data"][0]["id"] = json!("1002");
            fs::write(&fixture, serde_json::to_vec(&data).unwrap()).unwrap();
            let third = run(resumed, true, Arc::default(), |_| {}).unwrap();
            assert_eq!(third["requests"], 1);
            assert_eq!(third["status"], "planned");
            assert_eq!(third["discovery_complete"], true);
            assert_eq!(third["discovery_resumable"], false);
            assert_eq!(third["discovered"], 3);
            assert_eq!(third["limit"], if all { Value::Null } else { json!(1) });
            assert_eq!(
                third["selected_targets"],
                if all {
                    json!(["art:1003", "art:1001", "novel:1004"])
                } else {
                    json!(["art:1003"])
                }
            );
            assert_eq!(third["deferred_by_limit"], if all { 0 } else { 2 });
            let saved: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
            assert_eq!(saved["all"], all);
            assert_eq!(saved["limit"], 1);
            assert_eq!(saved["types"], "all");
        }
    }
    #[test]
    fn pending_discovery_does_not_reuse_stale_generation() {
        let temp = tempfile::tempdir().unwrap();
        let mut data: Value = serde_json::from_str(DEMO).unwrap();
        let art =
            "/ajax/search/artworks/test?word=test&order=date_d&mode=safe&s_mode=s_tag_full&p=1";
        let novel =
            "/ajax/search/novels/test?word=test&order=date_d&mode=safe&s_mode=s_tag_full&p=1";
        data["responses"][art] = json!({"illustManga":{"data":[{"id":"1001"}],"total":1}});
        data["responses"][novel] = json!({"novel":{"data":[{"id":"1004"}],"total":1}});
        let fixture = temp.path().join("fixture.json");
        fs::write(&fixture, serde_json::to_vec(&data).unwrap()).unwrap();
        let options = Options {
            targets: vec!["https://www.pixiv.net/tags/test".into()],
            fixture: Some(fixture.clone()),
            out: temp.path().join("archive"),
            all: true,
            request_budget: 1,
            ..Default::default()
        };
        assert_eq!(
            run(options.clone(), true, Arc::default(), |_| {}).unwrap()["status"],
            "paused"
        );
        let path = options.out.join("plan.json");
        let mut pending: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        pending["discovery_generation"] = json!("new-generation");
        atomic(&path, &serde_json::to_vec(&pending).unwrap()).unwrap();
        data["responses"][art]["illustManga"]["data"][0]["id"] = json!("1003");
        fs::write(&fixture, serde_json::to_vec(&data).unwrap()).unwrap();
        let resumed = Options {
            targets: vec![],
            resume: true,
            ..options
        };
        let second = run(resumed.clone(), true, Arc::default(), |_| {}).unwrap();
        assert_eq!(second["status"], "paused");
        assert_eq!(second["requests"], 1);
        let third = run(resumed, true, Arc::default(), |_| {}).unwrap();
        assert_eq!(third["status"], "planned");
        assert_eq!(third["requests"], 1);
        assert_eq!(third["selected_targets"], json!(["art:1003", "novel:1004"]));
    }
    #[test]
    fn fresh_search_discards_partial_cache_but_resume_reuses_it() {
        let temp = tempfile::tempdir().unwrap();
        let mut data: Value = serde_json::from_str(DEMO).unwrap();
        let art =
            "/ajax/search/artworks/test?word=test&order=date_d&mode=safe&s_mode=s_tag_full&p=1";
        let novel =
            "/ajax/search/novels/test?word=test&order=date_d&mode=safe&s_mode=s_tag_full&p=1";
        data["responses"][art] = json!({"illustManga":{"data":[{"id":"1001"}],"total":1}});
        data["responses"][novel] = json!({"novel":{"data":[{"id":"1004"}],"total":1}});
        let fixture = temp.path().join("fixture.json");
        fs::write(&fixture, serde_json::to_vec(&data).unwrap()).unwrap();
        let options = Options {
            targets: vec!["https://www.pixiv.net/tags/test".into()],
            fixture: Some(fixture.clone()),
            out: temp.path().join("archive"),
            all: true,
            request_budget: 1,
            ..Default::default()
        };
        let first = run(options.clone(), true, Arc::default(), |_| {}).unwrap();
        assert_eq!(first["status"], "paused");
        assert_eq!(first["discovery_complete"], false);
        data["responses"][art]["illustManga"]["data"][0]["id"] = json!("1003");
        fs::write(&fixture, serde_json::to_vec(&data).unwrap()).unwrap();
        let resumed = run(
            Options {
                targets: vec![],
                resume: true,
                ..options.clone()
            },
            true,
            Arc::default(),
            |_| {},
        )
        .unwrap();
        assert_eq!(resumed["requests"], 1);
        assert_eq!(resumed["status"], "planned");
        assert_eq!(
            resumed["selected_targets"],
            json!(["art:1001", "novel:1004"])
        );
        run(options.clone(), true, Arc::default(), |_| {}).unwrap();
        data["responses"][art]["illustManga"]["data"][0]["id"] = json!("1002");
        fs::write(&fixture, serde_json::to_vec(&data).unwrap()).unwrap();
        let fresh = run(
            Options {
                request_budget: 2,
                ..options
            },
            true,
            Arc::default(),
            |_| {},
        )
        .unwrap();
        assert_eq!(fresh["requests"], 2);
        assert_eq!(fresh["selected_targets"], json!(["art:1002", "novel:1004"]));
    }
    #[test]
    fn query_search_plan_fetch_and_resume_keep_source_and_body_only_mode() {
        let temp = tempfile::tempdir().unwrap();
        let source = "https://www.pixiv.net/search?q=R18&s_mode=tag_tc&type=novel";
        let mut fixture: Value = serde_json::from_str(DEMO).unwrap();
        fixture["responses"]
            ["/ajax/search/novels/R18?word=R18&order=date_d&mode=safe&s_mode=s_tag&p=1"] =
            json!({"novel":{"data":[{"id":"1004"}],"total":1}});
        let fixture_path = temp.path().join("fixture.json");
        fs::write(&fixture_path, fixture.to_string()).unwrap();
        let options = Options {
            targets: vec![source.into()],
            fixture: Some(fixture_path),
            out: temp.path().join("archive"),
            text_only: true,
            all: true,
            ..Default::default()
        };
        let plan = run(options.clone(), true, Arc::default(), |_| {}).unwrap();
        assert_eq!(plan["status"], "planned");
        assert_eq!(plan["requests"], 1);
        let resumed = Options {
            targets: vec![],
            resume: true,
            ..options
        };
        let fetched = run(resumed.clone(), false, Arc::default(), |_| {}).unwrap();
        assert_eq!(fetched["status"], "complete");
        assert_eq!(fetched["requests"], 1);
        assert_eq!(fetched["targets"], json!([source]));
        assert!(resumed.out.join("novel/novel_1004.txt").is_file());
        assert!(!resumed.out.join("works").exists());
        let again = run(resumed, false, Arc::default(), |_| {}).unwrap();
        assert_eq!(again["requests"], 0);
        assert_eq!(again["already_complete"], 1);
    }
    #[test]
    fn tag_search_plan_fetch_resume_and_incomplete_discovery() {
        let temp = tempfile::tempdir().unwrap();
        let mut data: Value = serde_json::from_str(DEMO).unwrap();
        data["responses"]
            ["/ajax/search/artworks/test?word=test&order=date_d&mode=safe&s_mode=s_tag_full&p=1"] =
            json!({"illustManga":{"data":[{"id":"1001"}],"total":1}});
        data["responses"]
            ["/ajax/search/novels/test?word=test&order=date_d&mode=safe&s_mode=s_tag_full&p=1"] =
            json!({"novel":{"data":[{"id":"1004"}],"total":1}});
        let fixture = temp.path().join("search.json");
        fs::write(&fixture, serde_json::to_vec(&data).unwrap()).unwrap();
        let options = Options {
            targets: vec!["https://www.pixiv.net/tags/test".into(), "art:1001".into()],
            fixture: Some(fixture),
            out: temp.path().join("archive"),
            all: true,
            ..Default::default()
        };
        let plan = run(options.clone(), true, Arc::default(), |_| {}).unwrap();
        assert_eq!(plan["status"], "planned");
        assert_eq!(plan["discovered"], 2);
        assert_eq!(plan["requests"], 2);
        let resumed = Options {
            targets: vec![],
            resume: true,
            ..options.clone()
        };
        let fetched = run(resumed.clone(), false, Arc::default(), |_| {}).unwrap();
        assert_eq!(fetched["status"], "complete");
        assert_eq!(fetched["counts"]["complete"], 2);
        let complete = run(resumed.clone(), false, Arc::default(), |_| {}).unwrap();
        assert_eq!(complete["requests"], 0);
        assert_eq!(complete["already_complete"], 2);
        let paused = run(
            Options {
                request_budget: 1,
                ..options
            },
            true,
            Arc::default(),
            |_| {},
        )
        .unwrap();
        assert_eq!(paused["stop_reason"]["kind"], "request_budget");
        let continued = run(resumed, false, Arc::default(), |_| {}).unwrap();
        assert_eq!(continued["status"], "complete");
        assert_eq!(continued["requests"], 1);
        assert_eq!(continued["already_complete"], 2);
        assert_eq!(continued["discovery_complete"], true);
    }
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
    fn resume_requires_existing_archive_and_lock_before_reading_selection() {
        let temp = tempfile::tempdir().unwrap();
        let options = Options {
            demo: true,
            resume: true,
            out: temp.path().join("never-created"),
            ..Default::default()
        };
        assert!(run(options.clone(), true, Arc::default(), |_| {}).is_err());
        assert!(!options.out.exists());
        let archive = Archive::open(&options.out, false).unwrap();
        fs::write(archive.root.join("job.json"), b"invalid json").unwrap();
        let error = run(options, true, Arc::default(), |_| {}).unwrap_err();
        assert!(error.to_string().contains("locked"), "{error}");
        assert_eq!(
            fs::read(archive.root.join("job.json")).unwrap(),
            b"invalid json"
        );
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
        let maximum = Options {
            jobs: MAX_JOBS,
            out: temp.path().join("maximum"),
            ..o.clone()
        };
        let maximum_report = run(maximum.clone(), false, Arc::default(), |_| {}).unwrap();
        assert_eq!(maximum_report["counts"]["complete"], 4);
        let invalid = Options {
            jobs: MAX_JOBS + 1,
            out: temp.path().join("invalid"),
            ..maximum
        };
        assert!(run(invalid.clone(), false, Arc::default(), |_| {}).is_err());
        assert!(!invalid.out.exists());
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
    fn completed_resume_plan_keeps_all_saved_refs_under_current_limit() {
        for (filename, legacy) in [("job.json", false), ("plan.json", true)] {
            let temp = tempfile::tempdir().unwrap();
            let options = Options {
                demo: true,
                resume: true,
                out: temp.path().join("archive"),
                ..Default::default()
            };
            drop(Archive::open(&options.out, false).unwrap());
            let refs: Vec<_> = (1..=150)
                .map(|id| Target::new("art", &id.to_string()).unwrap())
                .collect();
            let mut record = snapshot(
                &Options {
                    all: true,
                    ..options.clone()
                },
                &refs,
                &["user:9001".into()],
            );
            if legacy {
                for field in ["all", "limit", "discovery_resumable"] {
                    record.as_object_mut().unwrap().remove(field);
                }
            }
            atomic(
                &options.out.join(filename),
                &serde_json::to_vec(&record).unwrap(),
            )
            .unwrap();
            for _ in 0..2 {
                let report = run(options.clone(), true, Arc::default(), |_| {}).unwrap();
                assert_eq!(report["status"], "planned");
                assert_eq!(report["discovered"], 150);
                assert_eq!(report["selected"], 100);
                assert_eq!(report["deferred_by_limit"], 50);
                assert_eq!(report["limit"], 100);
                assert_eq!(report["requests"], 0);
                let saved: Value =
                    serde_json::from_slice(&fs::read(options.out.join("plan.json")).unwrap())
                        .unwrap();
                assert_eq!(saved["targets"], record["targets"]);
                assert_eq!(saved["all"], false);
                assert_eq!(saved["limit"], 100);
            }
        }
    }
    #[test]
    fn completed_resume_fetch_keeps_deferred_refs_with_small_budget() {
        let temp = tempfile::tempdir().unwrap();
        let options = Options {
            demo: true,
            targets: vec!["user:9001".into()],
            all: true,
            out: temp.path().join("archive"),
            ..Default::default()
        };
        let planned = run(options.clone(), true, Arc::default(), |_| {}).unwrap();
        assert_eq!(planned["discovered"], 4);
        let refs = planned["selected_targets"].clone();
        let options = Options {
            targets: vec![],
            resume: true,
            all: false,
            limit: 1,
            jobs: 1,
            request_budget: 1,
            ..options
        };
        for _ in 0..2 {
            let report = run(options.clone(), false, Arc::default(), |_| {}).unwrap();
            assert_eq!(report["status"], "paused");
            assert_eq!(report["discovered"], 4);
            assert_eq!(report["selected"], 1);
            assert_eq!(report["deferred_by_limit"], 3);
            assert_eq!(report["requests"], 1);
            let saved: Value =
                serde_json::from_slice(&fs::read(options.out.join("job.json")).unwrap()).unwrap();
            assert_eq!(saved["targets"], refs);
        }
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
        assert_eq!(failed["discovery_complete"], false);
        assert_eq!(failed["discovery_resumable"], false);
        let marker: Value =
            serde_json::from_slice(&fs::read(o.out.join("job.json")).unwrap()).unwrap();
        assert_eq!(marker["discovery_resumable"], false);
        assert!(marker["discovery_generation"].is_string());
        assert_eq!(marker["source_targets"], json!(["user:9999"]));
        assert_eq!(marker["all"], o.all);
        assert_eq!(marker["limit"], o.limit);
        assert_eq!(marker["types"], o.types);
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
