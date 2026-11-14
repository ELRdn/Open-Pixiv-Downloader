use crate::{
    archive::{atomic, now, safe_path, Archive},
    error::{invalid, Result},
    jobs::{self, Event, Options},
    transport::validate_url,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    fs::{self, OpenOptions},
    path::Path,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

const SOURCE: &str = "https://www.pixiv.net/novel";
const CHECKPOINT: &str = "novel-scan.json";
const BATCH: u64 = 1000;

pub fn describe_selection(options: &Options) -> Result<Option<crate::i18n::Message>> {
    let scan = if options.resume {
        owns_selection(&options.out)?
    } else {
        jobs::targets(options)?
            .iter()
            .any(|target| target.kind == "novel_scan")
    };
    if !scan {
        return Ok(None);
    }
    let state = read_state(&safe_path(&options.out, CHECKPOINT)?, options.online)?;
    Ok(Some(message!(
        "Fixed IDs {}..{} ({} IDs remaining, not a work count)",
        "固定ID {}〜{}（残り {} ID・作品数ではありません）",
        state.first_id,
        state.last_id,
        state.total() - state.processed()
    )))
}

pub fn owns_selection(root: &Path) -> Result<bool> {
    let path = jobs::selection_path(root)?;
    if !path.exists() {
        return Ok(false);
    }
    if fs::metadata(&path)?.len() > jobs::MAX_SELECTION_BYTES {
        return Err(invalid("Saved job exceeds 512 MiB"));
    }
    let record: Value = serde_json::from_slice(&fs::read(path)?)?;
    Ok(record["novel_scan"] == true)
}

pub fn preview(options: &Options) -> Result<Value> {
    let archive = Archive::open(&options.out, true)?;
    let path = safe_path(&archive.root, CHECKPOINT)?;
    let state = read_state(&path, options.online)?;
    let mut report = state.report(0, options.request_budget)?;
    report["status"] = json!("planned");
    atomic(
        &safe_path(&archive.root, "plan.json")?,
        &serde_json::to_vec(&json!({
            "schema_version":1,"novel_scan":true,"mode":if options.online {"online"} else {"fixture"}
        }))?,
    )?;
    Ok(report)
}

fn read_state(path: &Path, online: bool) -> Result<State> {
    if !path.is_file() {
        return Err(invalid("Initialize the pinned range first: opixiv scan-novels --last-id ID --online --out PATH"));
    }
    if fs::metadata(path)?.len() > 128 * 1024 {
        return Err(invalid("Novel scan checkpoint exceeds 128 KiB"));
    }
    let state: State = serde_json::from_slice(&fs::read(path)?)?;
    state.validate()?;
    if state.online != online {
        return Err(invalid(
            "Cannot resume a scan in a different online/offline mode",
        ));
    }
    Ok(state)
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct State {
    schema_version: u32,
    source: String,
    online: bool,
    first_id: u64,
    last_id: u64,
    next_id: Option<u64>,
    status: String,
    counts: BTreeMap<String, u64>,
    requests: u64,
    elapsed_seconds: f64,
    updated_at: String,
    last_batch: Value,
}

impl State {
    fn total(&self) -> u64 {
        self.last_id - self.first_id + 1
    }
    fn processed(&self) -> u64 {
        self.next_id
            .map_or_else(|| self.total(), |next| next - self.first_id)
    }
    fn validate(&self) -> Result<()> {
        if self.schema_version != 1
            || self.source != SOURCE
            || self.first_id == 0
            || self.first_id > self.last_id
            || self
                .next_id
                .is_some_and(|id| id < self.first_id || id > self.last_id)
            || !self.elapsed_seconds.is_finite()
            || self.elapsed_seconds < 0.0
            || ![
                "running",
                "paused",
                "interrupted",
                "needs_review",
                "prepared",
                "complete",
            ]
            .contains(&self.status.as_str())
            || ((self.status == "complete") != self.next_id.is_none())
            || (self.status == "prepared"
                && (self.next_id != Some(self.first_id) || self.requests != 0))
            || self.counts.keys().any(|key| {
                !["saved", "previous_exclusions", "skipped", "unavailable"].contains(&key.as_str())
            })
        {
            return Err(invalid("Invalid novel scan checkpoint"));
        }
        let counted = self
            .counts
            .values()
            .try_fold(0u64, |sum, count| sum.checked_add(*count));
        if counted != Some(self.processed()) {
            return Err(invalid("Novel scan cursor and counts disagree"));
        }
        Ok(())
    }
    fn save(&mut self, root: &Path) -> Result<()> {
        self.updated_at = now();
        self.validate()?;
        atomic(
            &safe_path(root, CHECKPOINT)?,
            &serde_json::to_vec_pretty(self)?,
        )
    }
    fn report(&self, invocation_requests: usize, budget: usize) -> Result<Value> {
        let mut report = serde_json::to_value(self)?;
        report["scope"] = json!("inclusive_novel_id_range; not a count of all Pixiv works");
        report["processed_ids"] = json!(self.processed());
        report["remaining_ids"] = json!(self.total() - self.processed());
        report["invocation_requests"] = json!(invocation_requests);
        report["total_requests"] = json!(self.requests);
        report["requests"] = json!(invocation_requests);
        report["request_budget"] = json!(budget);
        report["novel_scan"] = json!(true);
        report["text_only"] = json!(true);
        report["body_ready_count"] = json!(self.counts.get("saved").copied().unwrap_or(0));
        report["full_complete_count"] = report["body_ready_count"].clone();
        report["stop_reason"] = match self.status.as_str() {
            "paused" => json!({"kind":"request_budget"}),
            "interrupted" => json!({"kind":"user_cancel"}),
            "needs_review" => self.last_batch["stop_reason"]
                .as_object()
                .map_or_else(|| json!({"kind":"worker_error"}), |reason| json!(reason)),
            _ => Value::Null,
        };
        Ok(report)
    }
}

/// Cursor commits only when every ID in the batch has a terminal outcome.
pub fn run(
    options: Options,
    first_id: Option<u64>,
    last_id: Option<u64>,
    stop: Arc<AtomicBool>,
    emit: impl Fn(Event) + Sync,
) -> Result<Value> {
    // Erase the callback type before the mutually recursive jobs/scan dispatch.
    run_inner(options, first_id, last_id, stop, &emit, false)
}

pub fn prepare(options: Options, first_id: Option<u64>, last_id: Option<u64>) -> Result<Value> {
    if options.resume {
        return Err(invalid("Prepare requires a new, explicit range"));
    }
    run_inner(options, first_id, last_id, Arc::default(), &|_| {}, true)
}

fn run_inner(
    options: Options,
    first_id: Option<u64>,
    last_id: Option<u64>,
    stop: Arc<AtomicBool>,
    emit: &(dyn Fn(Event) + Sync),
    prepare_only: bool,
) -> Result<Value> {
    jobs::validate_options(&options)?;
    if options.input.is_some() || options.retry_unavailable || options.types == "art" {
        return Err(invalid(
            "Novel scans do not accept --input, --retry-unavailable or --types art",
        ));
    }
    if options.resume {
        if first_id.is_some() || last_id.is_some() || !options.targets.is_empty() {
            return Err(invalid(
                "Resume uses the pinned scan; do not supply URLs or ID bounds",
            ));
        }
    } else {
        let source = match options.targets.as_slice() {
            [] => SOURCE,
            [source] => source,
            _ => return Err(invalid("Supply only the Pixiv novel top URL")),
        };
        let url = validate_url(source, false)?;
        if !matches!(url.path(), "/novel" | "/novel/")
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err(invalid("Use https://www.pixiv.net/novel without filters"));
        }
        if first_id.unwrap_or(1) == 0 || last_id.is_none_or(|end| first_id.unwrap_or(1) > end) {
            return Err(invalid(
                "Specify a positive inclusive --last-id at or after --first-id",
            ));
        }
    }
    if options.resume && !options.out.join(CHECKPOINT).is_file() {
        return Err(invalid("Novel scan checkpoint does not exist"));
    }
    fs::create_dir_all(&options.out)?;
    let root = options.out.canonicalize()?;
    let guard = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(safe_path(&root, ".novel-scan.lock")?)?;
    guard
        .try_lock()
        .map_err(|_| invalid("Another novel scan is running"))?;
    let checkpoint = safe_path(&root, CHECKPOINT)?;
    let mut state = if options.resume {
        read_state(&checkpoint, options.online)?
    } else {
        // shortcut: one pinned range per archive, add explicit range extension when needed.
        if checkpoint.exists() {
            return Err(invalid(
                "A novel scan already exists; use scan-novels --resume",
            ));
        }
        State {
            schema_version: 1,
            source: SOURCE.into(),
            online: options.online,
            first_id: first_id.unwrap_or(1),
            last_id: last_id.unwrap(),
            next_id: Some(first_id.unwrap_or(1)),
            status: "running".into(),
            counts: BTreeMap::new(),
            requests: 0,
            elapsed_seconds: 0.0,
            updated_at: now(),
            last_batch: Value::Null,
        }
    };
    if prepare_only {
        state.status = "prepared".into();
        state.save(&root)?;
        let mut report = state.report(0, options.request_budget)?;
        report["status"] = json!("planned");
        return Ok(report);
    }
    state.save(&root)?;
    let mut requests = 0;
    while let Some(first) = state.next_id {
        if stop.load(Ordering::SeqCst) {
            state.status = "interrupted".into();
            state.save(&root)?;
            break;
        }
        if options.request_budget != 0 && requests >= options.request_budget {
            state.status = "paused".into();
            state.save(&root)?;
            break;
        }
        let last = first.saturating_add(BATCH - 1).min(state.last_id);
        let mut batch = options.clone();
        batch.targets = (first..=last).map(|id| format!("novel:{id}")).collect();
        batch.resume = false;
        batch.novel_scan = true;
        batch.all = true;
        batch.text_only = true;
        batch.types = "novel".into();
        if batch.request_budget != 0 {
            batch.request_budget -= requests;
        }
        state.status = "running".into();
        state.last_batch = json!({"first_id":first,"last_id":last,"status":"running"});
        state.save(&root)?;
        let saved = state.counts.get("saved").copied().unwrap_or(0);
        let batch_events = |event| match event {
            Event::Metrics(mut metrics) => {
                metrics["requests"] =
                    json!(requests as u64 + metrics["requests"].as_u64().unwrap_or(0));
                metrics["request_budget"] = json!(options.request_budget);
                for key in ["body_ready", "complete"] {
                    if let Some(count) = metrics[key].as_u64() {
                        metrics[key] = json!(saved + count);
                    }
                }
                emit(Event::Metrics(metrics));
            }
            Event::Message(message) => emit(Event::Message(message)),
            Event::ArchiveOpened(root) => emit(Event::ArchiveOpened(root)),
            // Keep TUI memory bounded instead of retaining millions of work rows.
            _ => {}
        };
        let result = match jobs::run(batch, false, stop.clone(), batch_events) {
            Ok(result) => result,
            Err(error) => {
                state.status = "needs_review".into();
                state.last_batch["reason"] = json!(error.to_string());
                state.save(&root)?;
                return Err(error);
            }
        };
        let used = result["requests"]
            .as_u64()
            .ok_or_else(|| invalid("Missing batch request count"))?;
        if options.request_budget != 0 && used > (options.request_budget - requests) as u64 {
            return Err(invalid("Novel scan exceeded its request budget"));
        }
        requests += used as usize;
        state.requests = state
            .requests
            .checked_add(used)
            .ok_or_else(|| invalid("Scan request count overflow"))?;
        state.elapsed_seconds += result["elapsed_seconds"].as_f64().unwrap_or(0.0);
        state.last_batch = json!({"first_id":first,"last_id":last,"status":result["status"],
            "requests":used,"counts":result["counts"],"already_complete":result["already_complete"],
            "previous_exclusions":result["previous_exclusions"],"stop_reason":result["stop_reason"],
            "elapsed_seconds":result["elapsed_seconds"],"reason":result["reason"]});
        let get = |name: &str| result["counts"][name].as_u64().unwrap_or(0);
        let outcomes = [
            (
                "saved",
                get("complete")
                    + get("already_complete")
                    + result["already_complete"].as_u64().unwrap_or(0),
            ),
            (
                "previous_exclusions",
                result["previous_exclusions"].as_u64().unwrap_or(0),
            ),
            ("skipped", get("skipped")),
            ("unavailable", get("unavailable")),
        ];
        if result["stop_reason"].is_null()
            && outcomes.iter().map(|(_, count)| count).sum::<u64>() == last - first + 1
        {
            for (name, count) in outcomes {
                *state.counts.entry(name.into()).or_default() += count;
            }
            state.next_id = (last < state.last_id).then(|| last + 1);
            state.status = if state.next_id.is_none() {
                "complete"
            } else {
                "running"
            }
            .into();
            state.save(&root)?;
            emit(Event::Message(message!(
                "ID range checked: {} / {} (saved, excluded, and unavailable included)",
                "ID範囲の確認: {} / {}（保存・除外・取得不可を含む）",
                state.processed(),
                state.total()
            )));
        } else {
            state.status = if result["stop_reason"]["kind"] == "request_budget" {
                "paused"
            } else if result["stop_reason"]["kind"] == "user_cancel" {
                "interrupted"
            } else {
                "needs_review"
            }
            .into();
            state.save(&root)?;
            break;
        }
    }
    state.report(requests, options.request_budget)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::DEMO;

    fn fixture(root: &Path) -> Options {
        let mut fixture: Value = serde_json::from_str(DEMO).unwrap();
        fixture["responses"]["/ajax/novel/1004"]["content"] = json!("a".repeat(1024));
        let mut short = fixture["responses"]["/ajax/novel/1004"].clone();
        short["id"] = json!("1005");
        short["content"] = json!("a".repeat(1023));
        fixture["responses"]["/ajax/novel/1005"] = short;
        let path = root.join("fixture.json");
        fs::write(&path, fixture.to_string()).unwrap();
        let out = root.join("archive");
        fs::create_dir(&out).unwrap();
        fs::write(
            out.join("novel-policy.json"),
            br#"{"schema_version":1,"min_bytes":1024}"#,
        )
        .unwrap();
        Options {
            fixture: Some(path),
            out,
            jobs: 1,
            request_budget: 1,
            ..Default::default()
        }
    }

    #[test]
    fn prepare_only_makes_no_requests_and_does_not_create_an_archive_database() {
        let temp = tempfile::tempdir().unwrap();
        let options = fixture(temp.path());
        let report = prepare(options.clone(), Some(1004), Some(1006)).unwrap();
        assert_eq!(report["status"], "planned");
        assert_eq!(report["requests"], 0);
        assert_eq!(report["remaining_ids"], 3);
        assert!(!options.out.join("state.sqlite3").exists());
        let state = read_state(&options.out.join(CHECKPOINT), false).unwrap();
        assert_eq!(state.status, "prepared");
        assert!(describe_selection(&Options {
            targets: vec![SOURCE.into()],
            ..options
        })
        .unwrap()
        .unwrap()
        .render(crate::i18n::Language::Japanese)
        .contains("1004〜1006"));
    }

    #[test]
    fn budget_resume_keeps_bodies_and_minimum_size_policy_without_refetching() {
        let temp = tempfile::tempdir().unwrap();
        let options = fixture(temp.path());
        let report = run(
            options.clone(),
            Some(1004),
            Some(1006),
            Arc::default(),
            |_| {},
        )
        .unwrap();
        assert_eq!(report["status"], "paused");
        assert_eq!(report["invocation_requests"], 1);
        assert_eq!(report["next_id"], 1004);
        assert_eq!(report["processed_ids"], 0);
        assert_eq!(
            fs::metadata(options.out.join("novel/novel_1004.txt"))
                .unwrap()
                .len(),
            1024
        );
        let resumed = Options {
            resume: true,
            request_budget: 3,
            ..options.clone()
        };
        let report = run(resumed.clone(), None, None, Arc::default(), |_| {}).unwrap();
        assert_eq!(report["status"], "complete");
        assert_eq!(report["invocation_requests"], 2);
        assert_eq!(report["counts"]["saved"], 1);
        assert_eq!(report["counts"]["skipped"], 1);
        assert_eq!(report["counts"]["unavailable"], 1);
        assert_eq!(report["remaining_ids"], 0);
        assert!(!options.out.join("novel/novel_1005.txt").exists());
        assert!(!options.out.join("works").exists());
        let report = run(resumed, None, None, Arc::default(), |_| {}).unwrap();
        assert_eq!(report["invocation_requests"], 0);
    }

    #[test]
    fn zero_budget_runs_across_batches_and_resumes_without_repeating_requests() {
        let temp = tempfile::tempdir().unwrap();
        let options = Options {
            request_budget: 0,
            ..fixture(temp.path())
        };
        let report = run(
            options.clone(),
            Some(1004),
            Some(2004),
            Arc::default(),
            |_| {},
        )
        .unwrap();
        assert_eq!(report["status"], "complete");
        assert_eq!(report["invocation_requests"], 1001);
        assert_eq!(report["request_budget"], 0);
        assert_eq!(report["counts"]["saved"], 1);
        assert_eq!(report["counts"]["skipped"], 1);
        assert_eq!(report["counts"]["unavailable"], 999);
        assert_eq!(report["remaining_ids"], 0);
        assert!(report["stop_reason"].is_null());
        let again = run(
            Options {
                resume: true,
                ..options
            },
            None,
            None,
            Arc::default(),
            |_| {},
        )
        .unwrap();
        assert_eq!(again["invocation_requests"], 0);
        assert_eq!(again["status"], "complete");
    }
    #[test]
    fn large_ranges_are_lazy_and_cancellation_does_not_build_a_million_targets() {
        let temp = tempfile::tempdir().unwrap();
        let options = fixture(temp.path());
        let report = run(
            options,
            None,
            Some(30_000_000),
            Arc::new(AtomicBool::new(true)),
            |_| {},
        )
        .unwrap();
        assert_eq!(report["status"], "interrupted");
        assert_eq!(report["requests"], 0);
        assert_eq!(report["remaining_ids"], 30_000_000);
    }

    #[test]
    fn corrupt_progress_or_changed_scope_cannot_silently_skip_ids() {
        let temp = tempfile::tempdir().unwrap();
        let options = fixture(temp.path());
        run(
            options.clone(),
            Some(1004),
            Some(1006),
            Arc::default(),
            |_| {},
        )
        .unwrap();
        let path = options.out.join(CHECKPOINT);
        let original = fs::read(&path).unwrap();
        let mut corrupt: Value = serde_json::from_slice(&original).unwrap();
        corrupt["next_id"] = json!(1005);
        fs::write(&path, corrupt.to_string()).unwrap();
        let resumed = Options {
            resume: true,
            ..options.clone()
        };
        assert!(run(resumed.clone(), None, None, Arc::default(), |_| {}).is_err());
        assert_eq!(fs::read(&path).unwrap(), corrupt.to_string().as_bytes());
        fs::write(&path, original).unwrap();
        assert!(run(resumed, None, Some(2000), Arc::default(), |_| {}).is_err());
        assert!(run(options, Some(1004), Some(1006), Arc::default(), |_| {}).is_err());
    }

    #[test]
    fn invalid_bound_or_source_does_not_create_an_archive() {
        let temp = tempfile::tempdir().unwrap();
        let options = Options {
            demo: true,
            out: temp.path().join("no-archive"),
            ..Default::default()
        };
        assert!(run(options.clone(), Some(0), Some(100), Arc::default(), |_| {}).is_err());
        for source in [
            "https://www.pixiv.net.evil/novel",
            "https://u:p@www.pixiv.net/novel",
            "https://www.pixiv.net/novel?r18=1",
        ] {
            let options = Options {
                targets: vec![source.into()],
                ..options.clone()
            };
            assert!(run(options, None, Some(100), Arc::default(), |_| {}).is_err());
        }
        assert!(!options.out.exists());
    }

    #[test]
    fn top_url_preview_and_resume_use_scan_but_unrelated_new_jobs_keep_their_scope() {
        let temp = tempfile::tempdir().unwrap();
        let options = fixture(temp.path());
        run(
            options.clone(),
            Some(1004),
            Some(1006),
            Arc::default(),
            |_| {},
        )
        .unwrap();
        let top = Options {
            targets: vec![SOURCE.into()],
            text_only: true,
            request_budget: 3,
            ..options.clone()
        };
        let report = jobs::run(top.clone(), true, Arc::default(), |_| {}).unwrap();
        assert_eq!(report["status"], "planned");
        assert_eq!(report["remaining_ids"], 3);
        assert!(owns_selection(&options.out).unwrap());
        let resume = Options {
            targets: vec![],
            resume: true,
            ..top.clone()
        };
        let events = std::sync::Mutex::new(Vec::new());
        let report = jobs::run(resume, false, Arc::default(), |event| {
            events.lock().unwrap().push(event)
        })
        .unwrap();
        assert_eq!(report["status"], "complete");
        assert_eq!(report["novel_scan"], true);
        assert!(events.lock().unwrap().iter().all(|event| matches!(
            event,
            Event::Metrics(_) | Event::ArchiveOpened(_) | Event::Message(_)
        )));
        let report = jobs::run(top.clone(), false, Arc::default(), |_| {}).unwrap();
        assert_eq!(report["requests"], 0);
        let checkpoint = fs::read(options.out.join(CHECKPOINT)).unwrap();
        let ordinary = Options {
            targets: vec!["novel:1004".into()],
            ..top
        };
        jobs::run(ordinary.clone(), false, Arc::default(), |_| {}).unwrap();
        assert!(!owns_selection(&options.out).unwrap());
        let report = jobs::run(
            Options {
                targets: vec![],
                resume: true,
                ..ordinary
            },
            false,
            Arc::default(),
            |_| {},
        )
        .unwrap();
        assert_eq!(report["discovered"], 1);
        assert_ne!(report["novel_scan"], true);
        assert_eq!(fs::read(options.out.join(CHECKPOINT)).unwrap(), checkpoint);
    }

    #[test]
    fn scan_lock_and_io_failure_never_advance_the_cursor() {
        let temp = tempfile::tempdir().unwrap();
        let options = fixture(temp.path());
        let guard = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(options.out.join(".novel-scan.lock"))
            .unwrap();
        guard.try_lock().unwrap();
        assert!(run(
            options.clone(),
            Some(1004),
            Some(1004),
            Arc::default(),
            |_| {}
        )
        .is_err());
        assert!(!options.out.join(CHECKPOINT).exists());
        drop(guard);
        fs::create_dir_all(options.out.join("novel/novel_1004.txt")).unwrap();
        let result = run(
            options.clone(),
            Some(1004),
            Some(1004),
            Arc::default(),
            |_| {},
        );
        assert!(result.is_err() || result.unwrap()["status"] == "needs_review");
        let state: State =
            serde_json::from_slice(&fs::read(options.out.join(CHECKPOINT)).unwrap()).unwrap();
        assert_eq!(state.next_id, Some(1004));
        assert_eq!(state.processed(), 0);
        assert_eq!(state.status, "needs_review");
    }
}
