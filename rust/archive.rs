use crate::{
    collection,
    error::{invalid, Error, Result},
    novel,
    target::Target,
    transport::Transport,
};
use chrono::Utc;
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, HashSet},
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Component, Path, PathBuf},
    sync::{Arc, Mutex},
};

#[path = "novel_text.rs"]
mod text;

pub fn now() -> String {
    Utc::now().to_rfc3339()
}
pub fn atomic(path: &Path, data: &[u8]) -> Result<()> {
    fs::create_dir_all(path.parent().unwrap())?;
    let mut file = tempfile::NamedTempFile::new_in(path.parent().unwrap())?;
    file.write_all(data)?;
    file.as_file().sync_all()?;
    file.persist(path).map_err(|e| Error::Io(e.error))?;
    Ok(())
}
pub fn safe_path(root: &Path, relative: &str) -> Result<PathBuf> {
    if relative.is_empty()
        || relative.contains(['\\', ':'])
        || relative.chars().any(char::is_control)
        || relative.split('/').any(|s| matches!(s, "" | "." | ".."))
        || Path::new(relative)
            .components()
            .any(|c| !matches!(c, Component::Normal(_)))
    {
        return Err(invalid("Unsafe archive path"));
    }
    let mut path = root.to_owned();
    for part in relative.split('/') {
        path.push(part);
        if fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_symlink()) {
            return Err(invalid("Symlinks are not accepted inside an archive"));
        }
    }
    Ok(path)
}
pub fn fingerprint(path: &Path) -> Result<(String, u64)> {
    let mut f = File::open(path)?;
    let mut hash = Sha256::new();
    let mut total = 0;
    let mut buf = [0_u8; 65536];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        total += n as u64;
        hash.update(&buf[..n]);
    }
    Ok((format!("{:x}", hash.finalize()), total))
}
pub fn extension(value: &str) -> Result<String> {
    let path = value.split('?').next().unwrap_or(value);
    let suffix = Path::new(path)
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if !["jpg", "jpeg", "png", "gif", "webp", "avif"].contains(&suffix.as_str()) {
        return Err(invalid("Unsupported image suffix"));
    }
    Ok(format!(".{suffix}"))
}
fn public_general(body: &Value) -> bool {
    matches!(body.get("xRestrict"), Some(Value::Number(n)) if n.as_u64() == Some(0))
        || body["xRestrict"] == "0"
}
fn eligible(body: &Value) -> bool {
    public_general(body)
        && (body["restrict"].is_null() || body["restrict"] == 0 || body["restrict"] == "0")
        && (body["isUnlisted"].is_null() || body["isUnlisted"] == false)
}
fn snapshot_identity(body: &Value) -> String {
    let keys = [
        "id",
        "userId",
        "content",
        "coverUrl",
        "textEmbeddedImages",
        "illustType",
        "pageCount",
        "urls",
        "createDate",
        "uploadDate",
    ];
    let value: serde_json::Map<String, Value> = keys
        .iter()
        .map(|key| ((*key).into(), body[*key].clone()))
        .collect();
    format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(&value).expect("JSON values serialize"))
    )
}
fn str_at<'a>(value: &'a Value, key: &str) -> Result<&'a str> {
    value[key]
        .as_str()
        .ok_or_else(|| invalid(&format!("Missing or invalid field: {key}")))
}

pub fn discover(client: &dyn Transport, target: &Target, types: &str) -> Result<Vec<Target>> {
    if target.kind == "search" {
        return crate::search::discover(client, &target.id, types);
    }
    let include = |kind: &str| types == "all" || types == kind;
    if matches!(target.kind.as_str(), "art" | "novel") {
        return Ok(if include(&target.kind) {
            vec![target.clone()]
        } else {
            vec![]
        });
    }
    let id = &target.id;
    if target.kind == "user" {
        let body = client.api(&format!("/ajax/user/{id}/profile/all"), &[])?;
        let mut result = HashSet::new();
        for category in ["illusts", "manga", "novels"] {
            let value = body
                .get(category)
                .ok_or_else(|| invalid("Author list schema changed"))?;
            let kind = if category == "novels" { "novel" } else { "art" };
            if !include(kind) {
                continue;
            }
            if value.as_array().is_some_and(Vec::is_empty) {
                continue;
            }
            for id in value
                .as_object()
                .ok_or_else(|| invalid("Author ID map schema changed"))?
                .keys()
            {
                result.insert(Target::new(kind, id)?);
            }
        }
        let mut refs: Vec<_> = result.into_iter().collect();
        refs.sort_by(|a, b| {
            a.kind
                .cmp(&b.kind)
                .then_with(|| b.id.len().cmp(&a.id.len()).then_with(|| b.id.cmp(&a.id)))
        });
        return Ok(refs);
    }
    if !include("novel") {
        return Ok(vec![]);
    }
    let detail = client.api(&format!("/ajax/novel/series/{id}"), &[])?;
    if !eligible(&detail) {
        return Err(Error::Unavailable(
            "Series is not confirmed public/general-audience".into(),
        ));
    }
    let mut refs = Vec::new();
    let mut seen = HashSet::new();
    let mut cursor = 0;
    for _ in 0..100 {
        let page = client.api(
            &format!("/ajax/novel/series_content/{id}"),
            &[
                ("limit", "30".into()),
                ("last_order", cursor.to_string()),
                ("order_by", "asc".into()),
            ],
        )?;
        let entries = page["page"]["seriesContents"]
            .as_array()
            .ok_or_else(|| invalid("Novel series pagination schema changed"))?;
        let before = refs.len();
        for item in entries {
            let id = item["id"]
                .as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| item["id"].to_string());
            let target = Target::new("novel", &id)?;
            if seen.insert(target.clone()) {
                refs.push(target);
            }
        }
        if entries.len() < 30 {
            return Ok(refs);
        }
        if refs.len() == before {
            return Err(invalid("Series pagination made no progress"));
        }
        cursor += entries.len();
    }
    client.cancel();
    Err(Error::Stop("Series discovery exceeded 100 pages".into()))
}

pub struct Archive {
    pub root: PathBuf,
    pub text_only: bool,
    db: Mutex<Connection>,
    lock: PathBuf,
    _guard: File,
}
type StateRow = (Target, String, String, Option<String>);
impl Archive {
    pub fn open(root: &Path, existing: bool) -> Result<Self> {
        if existing && !root.join("state.sqlite3").is_file() {
            return Err(invalid("Archive database does not exist"));
        }
        fs::create_dir_all(root)?;
        let root = root.canonicalize()?;
        // The OS lock survives as a file but releases automatically on process death.
        // Keep a separate create-new sentinel so older Python/Rust binaries also refuse
        // concurrent writes; only a V2 sentinel can be recovered automatically.
        let guard = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(safe_path(&root, ".archive.lock")?)?;
        guard.try_lock().map_err(|error| {
            invalid(&format!(
                "Archive is locked or filesystem locking is unavailable: {error}"
            ))
        })?;
        let lock = safe_path(&root, ".run.lock")?;
        const MARKER: &str = "OpenPixivDownloader archive lock v2\n";
        if lock.exists() {
            let mut marker = String::new();
            File::open(&lock)?.take(128).read_to_string(&mut marker)?;
            if !marker.starts_with(MARKER) {
                return Err(invalid("Legacy archive lock found. Confirm all older downloader processes are closed before removing .run.lock; automatic legacy recovery is unsafe"));
            }
            // Holding the persistent OS guard proves no V2 owner is still active.
            fs::remove_file(&lock)?;
        }
        let mut sentinel = tempfile::NamedTempFile::new_in(&root)?;
        writeln!(sentinel, "{MARKER}{}", std::process::id())?;
        sentinel.as_file().sync_all()?;
        let sentinel = sentinel.persist_noclobber(&lock).map_err(|error| {
            if error.error.kind() == std::io::ErrorKind::AlreadyExists {
                invalid("Archive was locked by another downloader before initialization completed")
            } else {
                Error::Io(error.error)
            }
        })?;
        drop(sentinel);
        let result = (|| {
            let db = Connection::open(safe_path(&root, "state.sqlite3")?)?;
            db.execute_batch("CREATE TABLE IF NOT EXISTS works(kind TEXT NOT NULL,id TEXT NOT NULL,status TEXT NOT NULL,updated_at TEXT NOT NULL,reason TEXT,PRIMARY KEY(kind,id)); CREATE TABLE IF NOT EXISTS search_pages(scope TEXT NOT NULL,request TEXT NOT NULL,body TEXT NOT NULL,PRIMARY KEY(scope,request)); CREATE TABLE IF NOT EXISTS novel_text(id TEXT PRIMARY KEY,sha256 TEXT NOT NULL,bytes INTEGER NOT NULL);")?;
            Ok(Self {
                root,
                text_only: false,
                db: Mutex::new(db),
                lock: lock.clone(),
                _guard: guard,
            })
        })();
        if result.is_err() {
            let _ = fs::remove_file(lock);
        }
        result
    }
    fn path(&self, target: &Target, name: &str) -> Result<PathBuf> {
        safe_path(&self.root, &format!("works/{}/{name}", target.key()))
    }
    pub fn discover_target(
        &self,
        client: &dyn Transport,
        target: &Target,
        types: &str,
        online: bool,
        generation: &str,
        progress: impl FnMut(String),
    ) -> Result<Vec<Target>> {
        let scope = serde_json::to_string(&(generation, online, types, target.label()))?;
        if target.kind == "search" {
            crate::search::discover_with(
                client,
                &target.id,
                types,
                |path, query, node| {
                    self.cached_discovery_api(client, &scope, path, query, Some(node))
                },
                progress,
            )
        } else {
            discover(
                &DiscoveryCache {
                    archive: self,
                    client,
                    scope,
                },
                target,
                types,
            )
        }
    }
    fn cached_discovery_api(
        &self,
        client: &dyn Transport,
        scope: &str,
        path: &str,
        query: &[(&str, String)],
        node: Option<&str>,
    ) -> Result<Value> {
        if client.stopped() {
            return Err(Error::Stop("Stopped while listing targets".into()));
        }
        let request = serde_json::to_string(&(path, query))?;
        let cached: Option<String> = self
            .db
            .lock()
            .unwrap()
            .query_row(
                "SELECT body FROM search_pages WHERE scope=?1 AND request=?2",
                params![scope, request],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(cached) = cached {
            if cached.len() > 16 * 1024 * 1024 {
                return Err(invalid("Saved search page exceeds 16 MiB"));
            }
            return Ok(serde_json::from_str(&cached)?);
        }
        let body = client.api(path, query)?;
        let compact = if let Some(node) = node {
            let page = &body[node];
            let entries = page["data"]
                .as_array()
                .ok_or_else(|| invalid("Tag-search result schema changed"))?;
            let total = page["total"]
                .as_u64()
                .ok_or_else(|| invalid("Missing tag-search total"))?;
            // Resume needs only IDs and dates, not search captions or user profiles.
            json!({node:{"total":total,"lastPage":page["lastPage"],"data":entries.iter().map(|item|
                json!({"id":item["id"],"createDate":item["createDate"],"isAdContainer":item["isAdContainer"]})).collect::<Vec<_>>()}})
        } else {
            body
        };
        self.db.lock().unwrap().execute(
            "INSERT INTO search_pages VALUES (?1,?2,?3)",
            params![scope, request, serde_json::to_string(&compact)?],
        )?;
        Ok(compact)
    }
    pub fn begin_discovery(&self, generation: &str) -> Result<()> {
        let prefix = format!("[{},", serde_json::to_string(generation)?);
        self.db.lock().unwrap().execute(
            "DELETE FROM search_pages WHERE substr(scope,1,?1) != ?2",
            params![prefix.len(), prefix],
        )?;
        Ok(())
    }
    pub fn finish_discovery(&self, generation: &str) -> Result<()> {
        let prefix = format!("[{},", serde_json::to_string(generation)?);
        self.db.lock().unwrap().execute(
            "DELETE FROM search_pages WHERE substr(scope,1,?1) = ?2",
            params![prefix.len(), prefix],
        )?;
        Ok(())
    }
    pub fn state(&self, target: &Target, status: &str, reason: Option<&str>) -> Result<()> {
        self.db.lock().unwrap().execute("INSERT INTO works VALUES (?1,?2,?3,?4,?5) ON CONFLICT(kind,id) DO UPDATE SET status=excluded.status,updated_at=excluded.updated_at,reason=excluded.reason", params![target.kind, target.id, status, now(), reason])?;
        Ok(())
    }
    pub fn status(&self, target: &Target) -> Result<Option<String>> {
        Ok(self
            .db
            .lock()
            .unwrap()
            .query_row(
                "SELECT status FROM works WHERE kind=?1 AND id=?2",
                params![target.kind, target.id],
                |r| r.get(0),
            )
            .optional()?)
    }
    pub fn reason(&self, target: &Target) -> Result<Option<String>> {
        Ok(self
            .db
            .lock()
            .unwrap()
            .query_row(
                "SELECT reason FROM works WHERE kind=?1 AND id=?2",
                params![target.kind, target.id],
                |r| r.get(0),
            )
            .optional()?
            .flatten())
    }
    pub fn manifest(&self, target: &Target) -> Option<Value> {
        serde_json::from_slice(&fs::read(self.path(target, "manifest.json").ok()?).ok()?).ok()
    }
    pub fn check(&self, target: &Target) -> Vec<String> {
        let Some(manifest) = self.manifest(target) else {
            return vec!["missing or invalid manifest".into()];
        };
        if manifest["key"] != target.key() {
            return vec!["manifest identity does not match work".into()];
        }
        let Some(files) = manifest["files"].as_array().filter(|v| !v.is_empty()) else {
            return vec!["missing or invalid manifest files".into()];
        };
        let mut errors: Vec<String> = files
            .iter()
            .filter_map(|entry| {
                let ok = entry["path"]
                    .as_str()
                    .and_then(|s| self.path(target, s).ok())
                    .and_then(|p| fingerprint(&p).ok())
                    .is_some_and(|(hash, bytes)| {
                        entry["sha256"] == hash && entry["bytes"] == bytes
                    });
                if ok {
                    None
                } else {
                    Some(format!(
                        "missing, unsafe or changed file: {}",
                        entry["path"].as_str().unwrap_or("invalid entry")
                    ))
                }
            })
            .collect();
        if manifest["status"] == "complete" {
            let mut required = vec!["raw.json", "metadata.json"];
            if target.kind == "novel" {
                required.extend(["source.txt", "plain.txt", "index.html"]);
            } else if let Some(body) = self.cached_json(target, "raw.json") {
                if body["illustType"] == 2 {
                    required.extend(["frames.json", "frames.zip"]);
                } else {
                    required.push("pages.json");
                }
            }
            for name in required {
                if !files.iter().any(|entry| entry["path"] == name) {
                    errors.push(format!("required file is not declared: {name}"));
                }
            }
            if let Some(resources) = manifest["resources"].as_array() {
                for resource in resources {
                    if resource["status"] != "complete" {
                        errors.push("complete manifest contains an unfinished material".into());
                    }
                    if !resource["path"]
                        .as_str()
                        .is_some_and(|path| files.iter().any(|entry| entry["path"] == path))
                    {
                        errors.push(
                            "complete manifest does not declare an expected material file".into(),
                        );
                    }
                }
            }
        }
        errors
    }
    pub fn complete(&self, target: &Target) -> Result<bool> {
        if self.text_only {
            return self.text_complete(target);
        }
        Ok(self.status(target)?.as_deref() == Some("complete")
            && self
                .manifest(target)
                .is_some_and(|m| m["status"] == "complete")
            && self.check(target).is_empty())
    }
    /// Compatibility helper; the pipeline calls prepare/finish independently.
    #[allow(dead_code)]
    pub fn save(&self, target: &Target, client: &dyn Transport) -> Result<String> {
        let status = self.prepare(target, client)?;
        if status != "assets_pending" {
            return Ok(status);
        }
        self.finish(target, client)
    }

    /// Commit usable local text before any cover or inserted-image request.
    pub fn prepare(&self, target: &Target, client: &dyn Transport) -> Result<String> {
        if self.text_only {
            return match self.prepare_text(target, client) {
                Ok(status) if matches!(status.as_str(), "complete" | "already_complete") => {
                    Ok(status)
                }
                result => self.stage_result(target, client, result),
            };
        }
        if self.complete(target)? {
            return Ok("already_complete".into());
        }
        self.stage_result(target, client, self.prepare_body(target, client))
    }

    /// Complete a previously committed snapshot without fetching its detail again.
    pub fn finish(&self, target: &Target, client: &dyn Transport) -> Result<String> {
        self.stage_result(target, client, self.finish_body(target, client))
    }

    fn stage_result(
        &self,
        target: &Target,
        client: &dyn Transport,
        result: Result<String>,
    ) -> Result<String> {
        match result {
            Ok(status) => {
                let stored = if status == "assets_pending" {
                    "partial"
                } else {
                    &status
                };
                self.state(target, stored, None)?;
                Ok(status)
            }
            Err(error @ Error::Budget) => {
                // Budget only closes admission. Do not cancel other in-flight work.
                self.state(target, "paused", Some(&error.to_string()))?;
                Err(error)
            }
            Err(error @ Error::Stop(_)) => {
                client.cancel();
                self.state(target, "paused", Some(&error.to_string()))?;
                Err(error)
            }
            Err(error @ (Error::Unavailable(_) | Error::Missing { .. })) => {
                let status = if self.manifest(target).is_some() {
                    "partial"
                } else {
                    "unavailable"
                };
                self.state(target, status, Some(&error.to_string()))?;
                Ok(status.into())
            }
            Err(error @ (Error::Io(_) | Error::Sql(_))) => {
                // Preserve the primary disk/DB failure even if recording it also fails.
                let _ = self.state(target, "failed", Some(&error.to_string()));
                Err(error)
            }
            Err(error) => {
                self.state(target, "failed", Some(&error.to_string()))?;
                Ok("failed".into())
            }
        }
    }

    fn cached_json(&self, target: &Target, name: &str) -> Option<Value> {
        let manifest = self.manifest(target)?;
        if manifest["key"] != target.key() {
            return None;
        }
        let entry = manifest["files"]
            .as_array()?
            .iter()
            .find(|e| e["path"] == name)?;
        let path = self.path(target, name).ok()?;
        let (hash, bytes) = fingerprint(&path).ok()?;
        if entry["sha256"] != hash || entry["bytes"] != bytes {
            return None;
        }
        serde_json::from_slice(&fs::read(path).ok()?).ok()
    }

    /// Explicit repair of a previous typed 404/410 may recheck source metadata once.
    /// Changed references are never spliced into the saved snapshot.
    fn refresh_stale_snapshot(
        &self,
        target: &Target,
        client: &dyn Transport,
        body: &Value,
    ) -> Result<()> {
        let Some(manifest) = self.manifest(target) else {
            return Ok(());
        };
        let stale: Vec<_> = manifest["resources"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|resource| {
                resource["status"] != "complete"
                    && matches!(resource["last_http_status"].as_u64(), Some(404 | 410))
            })
            .collect();
        if stale.is_empty() {
            return Ok(());
        }
        let endpoint = if target.kind == "art" {
            "illust"
        } else {
            "novel"
        };
        let fresh = client.api(&format!("/ajax/{endpoint}/{}", target.id), &[])?;
        if fresh["id"] != target.id
            || !eligible(&fresh)
            || snapshot_identity(body) != snapshot_identity(&fresh)
        {
            return Err(invalid("update_needed: stale material metadata now describes a different or inaccessible snapshot; original files were preserved"));
        }
        if target.kind == "art" {
            let motion = body["illustType"] == 2;
            let name = if motion { "frames.json" } else { "pages.json" };
            let path = if motion {
                format!("/ajax/illust/{}/ugoira_meta", target.id)
            } else {
                format!("/ajax/illust/{}/pages", target.id)
            };
            let saved = self.cached_json(target, name).ok_or_else(|| {
                invalid("update_needed: saved material metadata cannot prove snapshot consistency")
            })?;
            let fresh = client.api(&path, &[])?;
            if fresh != saved {
                return Err(invalid("update_needed: stale artwork material references changed; original files were preserved"));
            }
        } else {
            let mut seen = HashSet::new();
            for resource in stale {
                let Some(reference) = resource["reference"]
                    .as_str()
                    .and_then(|value| value.strip_prefix("pixivimage:"))
                else {
                    continue;
                };
                if !seen.insert(reference) {
                    continue;
                }
                let relation = client.api(
                    &format!("/ajax/novel/{}/insert_illusts", target.id),
                    &[("id[]", reference.into())],
                )?;
                let item = &relation[reference];
                if item["visible"] != true
                    || !eligible(&item["illust"])
                    || item["illust"]["images"]["original"] != resource["source_url"]
                {
                    return Err(invalid("update_needed: stale inserted-image relation changed; original files were preserved"));
                }
            }
        }
        Ok(())
    }
    fn prepare_body(&self, target: &Target, client: &dyn Transport) -> Result<String> {
        let body = match self.cached_json(target, "raw.json") {
            Some(body) => {
                self.refresh_stale_snapshot(target, client, &body)?;
                body
            }
            None => {
                if self.manifest(target).is_some_and(|manifest| {
                    manifest["key"] == target.key()
                        && manifest["snapshot_identity"].as_str().is_none()
                        && manifest["files"].as_array().is_some_and(|files| {
                            files.iter().any(|entry| entry["path"] == "raw.json")
                        })
                }) {
                    return Err(invalid("update_needed: damaged legacy raw data cannot prove the original snapshot identity; original files were preserved"));
                }
                let endpoint = if target.kind == "art" {
                    "illust"
                } else {
                    "novel"
                };
                let body = client.api(&format!("/ajax/{endpoint}/{}", target.id), &[])?;
                if let Some(manifest) = self.manifest(target) {
                    if let Some(identity) = manifest["snapshot_identity"].as_str() {
                        if identity != snapshot_identity(&body) {
                            return Err(invalid("update_needed: remote payload differs from saved snapshot; original files were preserved"));
                        }
                    } else if let Some(metadata) = self.cached_json(target, "metadata.json") {
                        if metadata["updated_at"] != body["uploadDate"] {
                            return Err(invalid("update_needed: remote modification date differs from saved snapshot; original files were preserved"));
                        }
                    }
                }
                // A repaired raw file must still describe the saved source snapshot.
                // Never silently mix a newer remote novel body with old material.
                if target.kind == "novel" {
                    if let Some(manifest) = self.manifest(target) {
                        if let Some(entry) = manifest["files"].as_array().and_then(|files| {
                            files.iter().find(|entry| entry["path"] == "source.txt")
                        }) {
                            if let Some(content) = body["content"].as_str() {
                                let hash = format!("{:x}", Sha256::digest(content.as_bytes()));
                                if entry["sha256"] != hash || entry["bytes"] != content.len() as u64
                                {
                                    return Err(invalid("update_needed: remote novel differs from saved snapshot; original files were preserved"));
                                }
                            }
                        }
                    }
                }
                body
            }
        };
        if !eligible(&body) {
            return Ok("skipped".into());
        }
        if body["id"] != target.id {
            return Err(invalid("Returned work ID does not match request"));
        }
        let kind = if target.kind == "novel" {
            "novel"
        } else {
            match body["illustType"].as_u64() {
                Some(0) => "illustration",
                Some(1) => "manga",
                Some(2) => "ugoira",
                _ => return Err(invalid("Unknown artwork type")),
            }
        };
        let mut writer = WorkWriter::new(self, target, client);
        writer.json("raw.json", &body)?;
        if self.cached_json(target, "metadata.json").is_none() {
            let metadata = json!({"schema_version":1,"id":target.id,"kind":kind,"title":body["title"],"author_id":body["userId"],"author_name":body["userName"],"source_url":target.url(),"created_at":body["createDate"],"updated_at":body["uploadDate"],"tags":body["tags"],"series":body["seriesNavData"],"archived_at":now(),"source_visibility":"public_general","rights":{"copyright_holder":"original author","license":"unknown","ai_training_permission":"unknown","redistribution_permission":"unknown"}});
            writer.json("metadata.json", &metadata)?;
        }
        if target.kind == "novel" {
            let content = str_at(&body, "content")?;
            if content.is_empty() && body["textCount"].as_u64().unwrap_or(0) > 0 {
                return Err(invalid("Novel body is empty despite declared text count"));
            }
            if !body["textEmbeddedImages"].is_null() && !body["textEmbeddedImages"].is_object() {
                return Err(invalid("Novel embedded-image schema changed"));
            }
            writer.write("source.txt", content.as_bytes())?;
            writer.write("plain.txt", novel::plain_text(content).as_bytes())?;
            let images = writer.healthy_novel_images(content);
            writer.write(
                "index.html",
                novel::render(
                    content,
                    body["title"].as_str().unwrap_or(""),
                    body["userName"].as_str().unwrap_or(""),
                    &images,
                )
                .as_bytes(),
            )?;
        }
        writer.checkpoint("partial", true)?;
        if !self.check(target).is_empty() {
            return Err(invalid("Prepared snapshot checksum verification failed"));
        }
        Ok("assets_pending".into())
    }

    fn finish_body(&self, target: &Target, client: &dyn Transport) -> Result<String> {
        let body = self.cached_json(target, "raw.json").ok_or_else(|| {
            invalid("Prepared raw snapshot is missing or changed; prepare the work again")
        })?;
        if body["id"] != target.id || !eligible(&body) {
            return Err(invalid("Prepared snapshot identity or visibility changed"));
        }
        let art_type = body["illustType"].as_u64();
        let mut writer = WorkWriter::new(self, target, client);
        if writer.local_json("metadata.json").is_none() {
            return Err(invalid(
                "Prepared metadata is missing or changed; prepare the work again",
            ));
        }
        if target.kind == "novel" {
            let content = str_at(&body, "content")?;
            writer.write("source.txt", content.as_bytes())?;
            writer.write("plain.txt", novel::plain_text(content).as_bytes())?;
        }
        if target.kind == "art" && art_type != Some(2) {
            let pages = writer.api_json(
                "pages.json",
                &format!("/ajax/illust/{}/pages", target.id),
                &[],
            )?;
            let Some(list) = pages.as_array() else {
                writer.invalidate("pages.json", "Image page schema changed")?;
                return Err(invalid("Image page schema changed"));
            };
            if list.is_empty() || body["pageCount"].as_u64() != Some(list.len() as u64) {
                writer.invalidate("pages.json", "Image-page count does not match metadata")?;
                return Err(invalid("Image-page count does not match metadata"));
            }
            let validated = list
                .iter()
                .map(|page| {
                    let url = str_at(&page["urls"], "original")?;
                    crate::transport::validate_url(url, true)?;
                    Ok((url, extension(url)?))
                })
                .collect::<Result<Vec<_>>>();
            let validated = match validated {
                Ok(pages) => pages,
                Err(error) => {
                    writer.invalidate("pages.json", &error.to_string())?;
                    return Err(error);
                }
            };
            for (i, (url, suffix)) in validated.iter().enumerate() {
                writer.asset(url, &format!("images/{i:04}{suffix}"), false)?;
            }
        } else if target.kind == "art" {
            let motion = writer.api_json(
                "frames.json",
                &format!("/ajax/illust/{}/ugoira_meta", target.id),
                &[],
            )?;
            let names = match frame_names(&motion) {
                Ok(names) => names,
                Err(error) => {
                    writer.invalidate("frames.json", &error.to_string())?;
                    return Err(error);
                }
            };
            writer.asset(str_at(&motion, "originalSrc")?, "frames.zip", false)?;
            if let Err(error) = validate_frames(&self.path(target, "frames.zip")?, &names) {
                writer.invalidate("frames.zip", &error.to_string())?;
                return Err(error);
            }
            if let Some(entry) = writer.expected.get_mut("frames.zip") {
                entry["payload_verified"] = json!(true);
            }
            if let Some(entry) = writer
                .files
                .iter_mut()
                .find(|entry| entry["path"] == "frames.zip")
            {
                entry["payload_verified"] = json!(true);
            }
            writer.json("frames.json", &motion)?;
        } else {
            let content = str_at(&body, "content")?;
            if content.is_empty() && body["textCount"].as_u64().unwrap_or(0) > 0 {
                return Err(invalid("Novel body is empty despite declared text count"));
            }
            if let Some(cover) = body["coverUrl"].as_str().filter(|s| !s.is_empty()) {
                match extension(cover) {
                    Ok(ext) => {
                        writer.asset(cover, &format!("cover{ext}"), true)?;
                    }
                    Err(e) => writer.omit("cover", &e.to_string()),
                }
            }
            let mut images = HashMap::new();
            if !body["textEmbeddedImages"].is_null() && !body["textEmbeddedImages"].is_object() {
                return Err(invalid("Novel embedded-image schema changed"));
            }
            for key in novel::image_refs(content) {
                let (tag, image_id) = key.split_once(':').unwrap();
                if let Some(relative) = writer.healthy_novel_images(content).get(&key).cloned() {
                    images.insert(key, relative);
                    continue;
                }
                let url = if tag == "uploadedimage" {
                    let item = &body["textEmbeddedImages"][image_id];
                    if !item.is_object()
                        || matches!(item["sl"].as_i64(), Some(4 | 6))
                        || item["sl"] == "4"
                        || item["sl"] == "6"
                    {
                        writer.omit(&key, "No eligible embedded-image metadata");
                        continue;
                    }
                    item["urls"]["original"].as_str().map(str::to_owned)
                } else {
                    let response = match writer.relation(image_id) {
                        Ok(v) => v,
                        Err(Error::Unavailable(_)) => {
                            writer.omit(&key, "Insertion relation unavailable");
                            continue;
                        }
                        Err(e) => return Err(e),
                    };
                    let item = &response[image_id];
                    if item["visible"] != true || !eligible(&item["illust"]) {
                        writer.omit(&key, "No visible/general-audience insertion relation");
                        continue;
                    }
                    item["illust"]["images"]["original"]
                        .as_str()
                        .map(str::to_owned)
                };
                let Some(url) = url else {
                    writer.omit(&key, "No original image URL");
                    continue;
                };
                let ext = match extension(&url) {
                    Ok(e) => e,
                    Err(e) => {
                        writer.omit(&key, &e.to_string());
                        continue;
                    }
                };
                let relative = format!("images/{tag}_{image_id}{ext}");
                if writer.asset(&url, &relative, true)? {
                    images.insert(key, relative);
                }
            }
            writer.write(
                "index.html",
                novel::render(
                    content,
                    body["title"].as_str().unwrap_or(""),
                    body["userName"].as_str().unwrap_or(""),
                    &images,
                )
                .as_bytes(),
            )?;
        }
        let status = if writer.omissions.is_empty() {
            "complete"
        } else {
            "partial"
        };
        writer.checkpoint(status, false)?;
        if !self.check(target).is_empty() {
            return Err(invalid("Post-write checksum verification failed"));
        }
        Ok(status.into())
    }
    fn rows(&self) -> Result<Vec<StateRow>> {
        let db = self.db.lock().unwrap();
        let mut stmt =
            db.prepare("SELECT kind,id,status,updated_at,reason FROM works ORDER BY kind,id")?;
        let rows = stmt
            .query_map([], |r| {
                Ok((
                    Target {
                        kind: r.get(0)?,
                        id: r.get(1)?,
                    },
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        for (t, _, _, _) in &rows {
            if !matches!(t.kind.as_str(), "art" | "novel") {
                return Err(invalid("Invalid archive work kind"));
            }
            Target::new(&t.kind, &t.id)?;
        }
        Ok(rows)
    }
    pub fn catalog(&self) -> Result<usize> {
        let rows = self.rows()?;
        let mut content = String::new();
        for (t, status, updated, reason) in &rows {
            let mut row = json!({"key":t.key(),"source_url":t.url(),"status":status,"state_updated_at":updated,"reason":reason});
            if matches!(status.as_str(), "complete" | "partial") {
                row["metadata"] = self
                    .path(t, "metadata.json")
                    .ok()
                    .and_then(|p| fs::read(p).ok())
                    .and_then(|b| serde_json::from_slice(&b).ok())
                    .unwrap_or(Value::Null);
            }
            content.push_str(&serde_json::to_string(&row)?);
            content.push('\n');
        }
        atomic(&safe_path(&self.root, "catalog.jsonl")?, content.as_bytes())?;
        Ok(rows.len())
    }
    pub fn collect_work(&self, target: &Target, cancel: &dyn Fn() -> bool) -> collection::Report {
        collection::work(&self.root, target, self.manifest(target), cancel)
    }
    pub fn collect(&self, cancel: &dyn Fn() -> bool) -> Result<Value> {
        collection::prepare(&self.root)?;
        let mut report = collection::Report::default();
        for (target, status, _, _) in self.rows()? {
            if !matches!(status.as_str(), "complete" | "partial")
                && !self.manifest(&target).is_some_and(|manifest| {
                    matches!(manifest["status"].as_str(), Some("complete" | "partial"))
                })
            {
                continue;
            }
            report.merge(self.collect_work(&target, cancel));
            if report.cancelled {
                break;
            }
        }
        let value = report.write(&self.root)?;
        Ok(json!({"status":value["status"],"collection":value}))
    }
    pub fn verify(&self) -> Result<Value> {
        let mut checked = 0;
        let mut valid = 0;
        let mut problems = Vec::new();
        for (t, status, _, _) in self.rows()? {
            if !matches!(status.as_str(), "complete" | "partial")
                && !self.manifest(&t).is_some_and(|manifest| {
                    matches!(manifest["status"].as_str(), Some("complete" | "partial"))
                })
            {
                continue;
            }
            checked += 1;
            let errors = self.check(&t);
            if errors.is_empty() {
                valid += 1;
            } else {
                problems.push(json!({"key":t.key(),"errors":errors}));
            }
        }
        for target in self.text_targets()? {
            checked += 1;
            if self.text_matches(&target)? {
                valid += 1;
            } else {
                problems.push(json!({"key":target.key(),"errors":["本文TXTが見つからないか変更されています"]}));
            }
        }
        Ok(json!({"checked":checked,"valid":valid,"problems":problems}))
    }
    pub fn pack(&self, destination: &Path) -> Result<Value> {
        if fs::symlink_metadata(destination).is_ok_and(|m| m.file_type().is_symlink()) {
            return Err(invalid("ZIP destination must not be a symlink"));
        }
        let parent = destination
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        fs::create_dir_all(parent)?;
        let destination = parent.canonicalize()?.join(
            destination
                .file_name()
                .ok_or_else(|| invalid("Invalid ZIP destination"))?,
        );
        if destination.starts_with(&self.root) {
            return Err(invalid("Place exported ZIP outside the archive directory"));
        }
        if !self.verify()?["problems"].as_array().unwrap().is_empty() {
            return Err(invalid("Archive integrity check failed; export cancelled"));
        }
        let mut file = tempfile::NamedTempFile::new_in(destination.parent().unwrap())?;
        let mut included = Vec::new();
        let mut included_keys = HashSet::new();
        {
            let mut zip = zip::ZipWriter::new(file.as_file_mut());
            let options = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated)
                .large_file(true);
            for (t, status, _, _) in self.rows()? {
                if status != "complete" || !self.complete(&t)? {
                    continue;
                }
                let manifest = self.manifest(&t).unwrap();
                let mut names: Vec<_> = manifest["files"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter_map(|f| f["path"].as_str())
                    .collect();
                names.push("manifest.json");
                for name in names {
                    zip.start_file(format!("works/{}/{name}", t.key()), options)?;
                    std::io::copy(&mut File::open(self.path(&t, name)?)?, &mut zip)?;
                }
                included_keys.insert(t.key());
                included.push(t.key());
            }
            for target in self.text_targets()? {
                zip.start_file(format!("novel/{}.txt", target.key()), options)?;
                std::io::copy(&mut File::open(self.text_path(&target)?)?, &mut zip)?;
                if included_keys.insert(target.key()) {
                    included.push(target.key());
                }
            }
            if included.is_empty() {
                return Err(invalid("There are no complete works to export"));
            }
            zip.start_file("export.json", options)?;
            zip.write_all(&serde_json::to_vec_pretty(&json!({"tool_version":env!("CARGO_PKG_VERSION"),"created_at":now(),"included_complete_works":included,"excluded":"partial, paused, failed, unavailable and skipped works"}))?)?;
            zip.finish()?;
        }
        file.as_file().sync_all()?;
        file.persist(&destination).map_err(|e| Error::Io(e.error))?;
        Ok(json!({"zip":destination,"complete_works":included.len()}))
    }
}
impl Drop for Archive {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.lock);
    }
}

struct DiscoveryCache<'a> {
    archive: &'a Archive,
    client: &'a dyn Transport,
    scope: String,
}
impl Transport for DiscoveryCache<'_> {
    fn api(&self, path: &str, query: &[(&str, String)]) -> Result<Value> {
        self.archive
            .cached_discovery_api(self.client, &self.scope, path, query, None)
    }
    fn download(&self, url: &str, path: &Path) -> Result<()> {
        self.client.download(url, path)
    }
    fn requests(&self) -> usize {
        self.client.requests()
    }
    fn stopped(&self) -> bool {
        self.client.stopped()
    }
    fn cancel(&self) {
        self.client.cancel();
    }
}

struct WorkWriter<'a> {
    archive: &'a Archive,
    target: &'a Target,
    client: &'a dyn Transport,
    files: Vec<Value>,
    expected: HashMap<String, Value>,
    omissions: Vec<Value>,
    relations: serde_json::Map<String, Value>,
    http_statuses: HashMap<String, u16>,
}
impl<'a> WorkWriter<'a> {
    fn new(archive: &'a Archive, target: &'a Target, client: &'a dyn Transport) -> Self {
        let old = archive
            .manifest(target)
            .filter(|m| m["key"] == target.key());
        let files = old
            .as_ref()
            .and_then(|m| m["files"].as_array())
            .map(|files| {
                files
                    .iter()
                    .filter(|entry| {
                        entry["path"]
                            .as_str()
                            .and_then(|name| archive.path(target, name).ok())
                            .and_then(|path| fingerprint(&path).ok())
                            .is_some_and(|(hash, bytes)| {
                                entry["sha256"] == hash && entry["bytes"] == bytes
                            })
                    })
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        // Keep historical expected bytes even when the current local file is
        // missing or corrupt and therefore excluded from the verified files list.
        let mut expected: HashMap<String, Value> = old
            .as_ref()
            .and_then(|manifest| manifest["files"].as_array())
            .into_iter()
            .flatten()
            .filter_map(|entry| {
                let path = entry["path"].as_str()?;
                entry["source_url"].as_str()?;
                Some((path.to_owned(), entry.clone()))
            })
            .collect();
        for resource in old
            .as_ref()
            .and_then(|manifest| manifest["resources"].as_array())
            .into_iter()
            .flatten()
        {
            let Some(path) = resource["path"].as_str() else {
                continue;
            };
            let Some(hash) = resource["expected_sha256"].as_str() else {
                continue;
            };
            let Some(bytes) = resource["expected_bytes"].as_u64() else {
                continue;
            };
            let Some(url) = resource["expected_source_url"].as_str() else {
                continue;
            };
            expected.entry(path.to_owned()).or_insert_with(
                || json!({"path":path,"sha256":hash,"bytes":bytes,"source_url":url,"payload_verified":resource["expected_payload_verified"].as_bool().unwrap_or(true)}),
            );
        }
        let relations = old
            .as_ref()
            .and_then(|m| m["relations"].as_object())
            .cloned()
            .unwrap_or_default();
        let http_statuses = old
            .as_ref()
            .and_then(|manifest| manifest["resources"].as_array())
            .into_iter()
            .flatten()
            .filter_map(|resource| {
                let path = resource["path"].as_str()?;
                let status = resource["last_http_status"].as_u64()?;
                matches!(status, 404 | 410).then(|| (path.to_owned(), status as u16))
            })
            .collect();
        Self {
            archive,
            target,
            client,
            files,
            expected,
            omissions: Vec::new(),
            relations,
            http_statuses,
        }
    }
    fn record(&mut self, relative: &str, url: Option<&str>) -> Result<()> {
        let (hash, bytes) = fingerprint(&self.archive.path(self.target, relative)?)?;
        let mut entry = json!({"path":relative,"sha256":hash,"bytes":bytes});
        if let Some(url) = url {
            entry["source_url"] = json!(url);
            if relative == "frames.zip" {
                // A digest of an unvalidated transfer is not a trusted original.
                let verified = self.expected.get(relative).is_some_and(|old| {
                    old["sha256"] == hash
                        && old["bytes"] == bytes
                        && old["payload_verified"].as_bool().unwrap_or(true)
                });
                entry["payload_verified"] = json!(verified);
            }
            self.expected.insert(relative.into(), entry.clone());
        }
        self.files.retain(|entry| entry["path"] != relative);
        self.files.push(entry);
        Ok(())
    }
    fn write(&mut self, relative: &str, data: &[u8]) -> Result<()> {
        let hash = format!("{:x}", Sha256::digest(data));
        if self.files.iter().any(|entry| {
            entry["path"] == relative
                && entry["sha256"] == hash
                && entry["bytes"] == data.len() as u64
        }) {
            return Ok(());
        }
        atomic(&self.archive.path(self.target, relative)?, data)?;
        self.record(relative, None)
    }
    fn json(&mut self, relative: &str, value: &Value) -> Result<()> {
        self.write(relative, &serde_json::to_vec_pretty(value)?)
    }
    fn local_json(&self, relative: &str) -> Option<Value> {
        self.files.iter().find(|entry| entry["path"] == relative)?;
        serde_json::from_slice(&fs::read(self.archive.path(self.target, relative).ok()?).ok()?).ok()
    }
    fn api_json(&mut self, relative: &str, path: &str, query: &[(&str, String)]) -> Result<Value> {
        if let Some(value) = self.local_json(relative) {
            return Ok(value);
        }
        let value = self.client.api(path, query)?;
        self.json(relative, &value)?;
        self.checkpoint("partial", true)?;
        Ok(value)
    }
    fn relation(&mut self, image_id: &str) -> Result<Value> {
        if let Some(entry) = self.relations.get(image_id) {
            if let Some(value) = entry.get("value") {
                let bytes = serde_json::to_vec(value)?;
                if entry["sha256"] == format!("{:x}", Sha256::digest(&bytes)) {
                    return Ok(value.clone());
                }
            }
        }
        let value = self.client.api(
            &format!("/ajax/novel/{}/insert_illusts", self.target.id),
            &[("id[]", image_id.into())],
        )?;
        let hash = format!("{:x}", Sha256::digest(serde_json::to_vec(&value)?));
        let item = &value[image_id];
        let cacheable = item["visible"] == true
            && eligible(&item["illust"])
            && item["illust"]["images"]["original"]
                .as_str()
                .is_some_and(|url| {
                    crate::transport::validate_url(url, true).is_ok() && extension(url).is_ok()
                });
        if cacheable {
            self.relations
                .insert(image_id.into(), json!({"value":value,"sha256":hash}));
        } else {
            self.relations.remove(image_id);
        }
        self.checkpoint("partial", true)?;
        Ok(value)
    }
    fn omit(&mut self, resource: &str, reason: &str) {
        self.omissions
            .push(json!({"resource":resource,"reason":reason}));
    }
    fn invalidate(&mut self, relative: &str, reason: &str) -> Result<()> {
        // Only an explicitly unverified first transfer can lose its expectation
        // after semantic validation fails. A previously verified original remains
        // protected even if refreshed frame metadata no longer agrees with it.
        if self
            .expected
            .get(relative)
            .is_some_and(|entry| entry["payload_verified"] == false)
        {
            self.expected.remove(relative);
        }
        self.files.retain(|entry| entry["path"] != relative);
        self.omit(relative, reason);
        self.checkpoint("partial", false)
    }
    fn asset(&mut self, url: &str, relative: &str, optional: bool) -> Result<bool> {
        let path = self.archive.path(self.target, relative)?;
        // files contains only entries verified when this writer was created.
        if self
            .files
            .iter()
            .any(|entry| entry["path"] == relative && entry["source_url"] == url)
        {
            return Ok(true);
        }
        let expected = self.expected.get(relative).cloned();
        if expected
            .as_ref()
            .is_some_and(|entry| entry["source_url"] != url)
        {
            let error = invalid("update_needed: material source differs from the saved expected identity; original files were preserved");
            self.omit(relative, &error.to_string());
            self.checkpoint("partial", false)?;
            return Err(error);
        }
        // A user may have restored an original after an earlier checkpoint. It is
        // reusable only when it matches the retained historical digest and size.
        if expected.as_ref().is_some_and(|entry| {
            fingerprint(&path)
                .is_ok_and(|(hash, bytes)| entry["sha256"] == hash && entry["bytes"] == bytes)
        }) {
            self.record(relative, Some(url))?;
            self.checkpoint("partial", true)?;
            return Ok(true);
        }
        // Close the temporary file handle before the transport atomically replaces
        // its path (required on Windows). A rejected new payload never touches path.
        fs::create_dir_all(path.parent().unwrap())?;
        let staging = tempfile::NamedTempFile::new_in(path.parent().unwrap())?.into_temp_path();
        match self.client.download(url, &staging) {
            Ok(()) => {
                let (hash, bytes) = fingerprint(&staging)?;
                if expected
                    .as_ref()
                    .is_some_and(|entry| entry["sha256"] != hash || entry["bytes"] != bytes)
                {
                    let error = invalid("update_needed: downloaded bytes differ from the saved expected hash or size; original files were preserved");
                    self.omit(relative, &error.to_string());
                    self.checkpoint("partial", false)?;
                    return Err(error);
                }
                OpenOptions::new().write(true).open(&staging)?.sync_all()?;
                fs::rename(&staging, &path)?;
                self.http_statuses.remove(relative);
                self.record(relative, Some(url))?;
                self.checkpoint("partial", true)?;
                Ok(true)
            }
            Err(error @ (Error::Stop(_) | Error::Budget)) => {
                self.omit(relative, &error.to_string());
                self.checkpoint("partial", true)?;
                Err(error)
            }
            Err(error @ Error::Missing { status }) => {
                self.http_statuses.insert(relative.into(), status);
                self.omit(relative, &error.to_string());
                self.checkpoint("partial", false)?;
                if optional {
                    Ok(false)
                } else {
                    Err(error)
                }
            }
            Err(error @ (Error::Io(_) | Error::Sql(_))) => {
                self.http_statuses.remove(relative);
                self.omit(relative, &error.to_string());
                let _ = self.checkpoint("partial", false);
                Err(error)
            }
            Err(error) if optional => {
                self.http_statuses.remove(relative);
                self.omit(relative, &error.to_string());
                self.checkpoint("partial", false)?;
                Ok(false)
            }
            Err(error) => {
                self.http_statuses.remove(relative);
                self.omit(relative, &error.to_string());
                self.checkpoint("partial", false)?;
                Err(error)
            }
        }
    }
    fn healthy_novel_images(&self, content: &str) -> HashMap<String, String> {
        let mut images = HashMap::new();
        for key in novel::image_refs(content) {
            let (tag, id) = key.split_once(':').unwrap();
            let prefix = format!("images/{tag}_{id}.");
            if let Some(name) = self
                .files
                .iter()
                .filter_map(|entry| entry["path"].as_str())
                .find(|name| name.starts_with(&prefix))
            {
                images.insert(key, name.into());
            }
        }
        images
    }
    fn resources(&self) -> Vec<Value> {
        let mut resources = Vec::new();
        let Some(body) = self.local_json("raw.json") else {
            return resources;
        };
        if self.target.kind == "novel" {
            if let Some(url) = body["coverUrl"].as_str().filter(|url| !url.is_empty()) {
                let path = extension(url)
                    .map(|ext| format!("cover{ext}"))
                    .unwrap_or_else(|_| "cover".into());
                resources.push(json!({"role":"cover","path":path,"source_url":url}));
            }
            for key in novel::image_refs(body["content"].as_str().unwrap_or("")) {
                let (tag, id) = key.split_once(':').unwrap();
                let url = if tag == "uploadedimage" {
                    body["textEmbeddedImages"][id]["urls"]["original"]
                        .as_str()
                        .map(str::to_owned)
                } else {
                    self.relations
                        .get(id)
                        .and_then(|entry| {
                            entry["value"][id]["illust"]["images"]["original"].as_str()
                        })
                        .map(str::to_owned)
                };
                let prefix = format!("images/{tag}_{id}.");
                let url = url.or_else(|| {
                    self.expected
                        .iter()
                        .find(|(path, _)| path.starts_with(&prefix))
                        .and_then(|(_, entry)| entry["source_url"].as_str())
                        .map(str::to_owned)
                });
                let path = url
                    .as_deref()
                    .and_then(|url| extension(url).ok())
                    .map(|ext| format!("images/{tag}_{id}{ext}"))
                    .or_else(|| {
                        self.files
                            .iter()
                            .filter_map(|entry| entry["path"].as_str())
                            .find(|path| path.starts_with(&prefix))
                            .map(str::to_owned)
                    });
                resources.push(
                    json!({"role":"inserted_image","reference":key,"path":path,"source_url":url}),
                );
            }
        } else if body["illustType"] == 2 {
            resources.push(json!({"role":"frames_metadata","path":"frames.json"}));
            resources.push(json!({"role":"frames_zip","path":"frames.zip","source_url":self.local_json("frames.json").map(|motion| motion["originalSrc"].clone())}));
        } else {
            resources.push(json!({"role":"pages_metadata","path":"pages.json"}));
            if let Some(pages) = self
                .local_json("pages.json")
                .and_then(|value| value.as_array().cloned())
            {
                for (i, page) in pages.iter().enumerate() {
                    if let Some(url) = page["urls"]["original"].as_str() {
                        let path = extension(url).map(|ext| format!("images/{i:04}{ext}")).ok();
                        resources.push(json!({"role":"page","path":path,"source_url":url}));
                    }
                }
            }
        }
        // Corrupt/missing pages or relation metadata must not make us forget a
        // previously trusted original while its references are being rebuilt.
        for (path, expected) in &self.expected {
            if !resources.iter().any(|resource| resource["path"] == *path) {
                resources.push(json!({"role":"saved_material","path":path,"source_url":expected["source_url"]}));
            }
        }
        for resource in &mut resources {
            let path = resource["path"].as_str();
            let complete =
                path.is_some_and(|path| self.files.iter().any(|entry| entry["path"] == path));
            let failed = self.omissions.iter().any(|omission| {
                path.is_some_and(|path| omission["resource"] == path)
                    || omission["resource"] == resource["reference"]
            });
            let last_status = (!complete)
                .then(|| path.and_then(|path| self.http_statuses.get(path)).copied())
                .flatten();
            let expected = path.and_then(|path| self.expected.get(path)).cloned();
            if let Some(expected) = expected {
                resource["expected_sha256"] = expected["sha256"].clone();
                resource["expected_bytes"] = expected["bytes"].clone();
                resource["expected_source_url"] = expected["source_url"].clone();
                resource["expected_payload_verified"] =
                    json!(expected["payload_verified"].as_bool().unwrap_or(true));
            }
            resource["status"] = json!(if complete {
                "complete"
            } else if failed {
                "missing"
            } else {
                "pending"
            });
            if let Some(status) = last_status {
                resource["last_http_status"] = json!(status);
            }
        }
        resources
    }
    fn checkpoint(&self, status: &str, preparing: bool) -> Result<()> {
        let resources = self.resources();
        let pending = resources
            .iter()
            .filter(|resource| resource["status"] != "complete")
            .count();
        let body_ready = self.target.kind == "novel"
            && ["raw.json", "source.txt", "plain.txt"]
                .iter()
                .all(|path| self.files.iter().any(|entry| entry["path"] == *path));
        let metadata_ready = ["raw.json", "metadata.json"]
            .iter()
            .all(|path| self.files.iter().any(|entry| entry["path"] == *path));
        let mut omissions = self.omissions.clone();
        if preparing && omissions.is_empty() {
            omissions
                .push(json!({"resource":"materials","reason":"Material verification is pending"}));
        }
        let manifest = json!({"schema_version":1,"tool_version":env!("CARGO_PKG_VERSION"),"key":self.target.key(),"source_url":self.target.url(),"archived_at":now(),"status":status,"completeness_scope":"supported work payload; excludes comments, likes and account data","files":self.files,"omissions":omissions,"stages":{"metadata_ready":metadata_ready,"body_ready":body_ready,"assets_pending":preparing || pending>0,"verified":status=="complete"},"resources":resources,"relations":self.relations,"snapshot_identity":self.local_json("raw.json").map(|body| snapshot_identity(&body))});
        atomic(
            &self.archive.path(self.target, "manifest.json")?,
            &serde_json::to_vec_pretty(&manifest)?,
        )
    }
}

fn frame_names(motion: &Value) -> Result<HashSet<String>> {
    crate::transport::validate_url(str_at(motion, "originalSrc")?, true)?;
    let frames = motion["frames"]
        .as_array()
        .filter(|v| !v.is_empty())
        .ok_or_else(|| invalid("Missing ugoira frame timing"))?;
    let mut names = HashSet::new();
    for frame in frames {
        let name = str_at(frame, "file")?;
        let delay = frame["delay"]
            .as_f64()
            .ok_or_else(|| invalid("Invalid ugoira delay"))?;
        if name.is_empty()
            || name.contains(['/', '\\', ':'])
            || name.chars().any(char::is_control)
            || !delay.is_finite()
            || !(0.0 < delay && delay <= 3600000.0)
            || !names.insert(name.to_owned())
        {
            return Err(invalid("Invalid ugoira frame metadata"));
        }
    }
    Ok(names)
}

fn validate_frames(path: &Path, expected: &HashSet<String>) -> Result<()> {
    let mut zip = zip::ZipArchive::new(File::open(path)?)?;
    if zip.len() != expected.len() {
        return Err(invalid("Ugoira ZIP members do not match frame metadata"));
    }
    let mut names = HashSet::new();
    let mut total = 0_u64;
    for i in 0..zip.len() {
        let mut member = zip.by_index(i)?;
        if !expected.contains(member.name())
            || !names.insert(member.name().to_owned())
            || member.encrypted()
        {
            return Err(invalid("Invalid ugoira ZIP member"));
        }
        // Read every member to EOF so CRC errors cannot be accepted as complete.
        let read = std::io::copy(
            &mut member.by_ref().take(256 * 1024 * 1024 + 1),
            &mut std::io::sink(),
        )
        .map_err(|error| {
            if error.kind() == std::io::ErrorKind::InvalidData {
                invalid("Ugoira ZIP CRC or compressed payload validation failed")
            } else {
                Error::Io(error)
            }
        })?;
        total += read;
        if read > 256 * 1024 * 1024 || total > 1024 * 1024 * 1024 {
            return Err(invalid("Ugoira expanded data exceeds validation limit"));
        }
    }
    Ok(())
}

pub type SharedArchive = Arc<Archive>;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::{Fixture, DEMO};
    struct BudgetTransport {
        fixture: Fixture,
        budget: usize,
    }
    impl Transport for BudgetTransport {
        fn api(&self, path: &str, query: &[(&str, String)]) -> Result<Value> {
            if self.fixture.requests() >= self.budget {
                return Err(Error::Budget);
            }
            self.fixture.api(path, query)
        }
        fn download(&self, url: &str, path: &Path) -> Result<()> {
            if self.fixture.requests() >= self.budget {
                return Err(Error::Budget);
            }
            self.fixture.download(url, path)
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
    fn body_is_usable_before_media_and_budget_does_not_cancel_other_work() {
        let temp = tempfile::tempdir().unwrap();
        let archive = Archive::open(temp.path(), false).unwrap();
        let target = Target::parse("novel:1004").unwrap();
        let client = BudgetTransport {
            fixture: Fixture::new(DEMO, Arc::default()).unwrap(),
            budget: 1,
        };
        assert_eq!(archive.prepare(&target, &client).unwrap(), "assets_pending");
        assert_eq!(client.requests(), 1);
        let manifest = archive.manifest(&target).unwrap();
        assert_eq!(manifest["status"], "partial");
        assert_eq!(manifest["stages"]["body_ready"], true);
        assert!(manifest["resources"].as_array().unwrap().len() >= 3);
        assert!(!archive.complete(&target).unwrap());
        assert_eq!(archive.collect_work(&target, &|| false).copied, 1);
        assert!(temp.path().join("works/novel/novel_1004.txt").is_file());
        assert!(matches!(
            archive.finish(&target, &client),
            Err(Error::Budget)
        ));
        assert!(!client.stopped());
        assert_eq!(archive.status(&target).unwrap().as_deref(), Some("paused"));
        assert_eq!(archive.verify().unwrap()["valid"], 1);
        let resumed = Fixture::new(DEMO, Arc::default()).unwrap();
        assert_eq!(archive.save(&target, &resumed).unwrap(), "complete");
        assert_eq!(resumed.requests(), 4, "detail is already checkpointed");
        assert_eq!(
            archive.manifest(&target).unwrap()["stages"]["verified"],
            true
        );
    }
    #[test]
    fn interrupted_material_checkpoint_repairs_only_missing_assets() {
        let temp = tempfile::tempdir().unwrap();
        let archive = Archive::open(temp.path(), false).unwrap();
        let target = Target::parse("novel:1004").unwrap();
        let client = BudgetTransport {
            fixture: Fixture::new(DEMO, Arc::default()).unwrap(),
            budget: 2,
        };
        assert!(matches!(archive.save(&target, &client), Err(Error::Budget)));
        assert!(temp.path().join("works/novel_1004/cover.png").is_file());
        let resumed = Fixture::new(DEMO, Arc::default()).unwrap();
        assert_eq!(archive.save(&target, &resumed).unwrap(), "complete");
        assert_eq!(
            resumed.requests(),
            3,
            "healthy detail and cover must not be fetched"
        );
        fs::write(temp.path().join("works/novel_1004/plain.txt"), b"broken").unwrap();
        let repair = Fixture::new(DEMO, Arc::default()).unwrap();
        assert_eq!(archive.save(&target, &repair).unwrap(), "complete");
        assert_eq!(
            repair.requests(),
            0,
            "plain text can be rebuilt from verified raw without HTTP"
        );
    }
    #[test]
    fn partial_cover_retry_uses_cached_relations_and_legacy_assets() {
        let temp = tempfile::tempdir().unwrap();
        let archive = Archive::open(temp.path(), false).unwrap();
        let target = Target::parse("novel:1004").unwrap();
        let mut data: Value = serde_json::from_str(DEMO).unwrap();
        data["assets"]
            .as_object_mut()
            .unwrap()
            .remove("https://i.pximg.net/synthetic/pixiv-archive-lab/1004_cover.png");
        let client = Fixture::new(&data.to_string(), Arc::default()).unwrap();
        assert_eq!(archive.save(&target, &client).unwrap(), "partial");
        let mut manifest = archive.manifest(&target).unwrap();
        // Emulate an alpha.1 partial snapshot with no additive stage/relations fields.
        for key in ["relations", "snapshot_identity", "stages", "resources"] {
            manifest.as_object_mut().unwrap().remove(key);
        }
        atomic(
            &temp.path().join("works/novel_1004/manifest.json"),
            &serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();
        let repaired = Fixture::new(DEMO, Arc::default()).unwrap();
        assert_eq!(archive.save(&target, &repaired).unwrap(), "complete");
        assert_eq!(
            repaired.requests(),
            1,
            "only the missing cover is needed, even for a legacy partial"
        );
    }
    #[test]
    fn changed_remote_snapshot_does_not_overwrite_preserved_payload() {
        for kind in ["novel", "art"] {
            let temp = tempfile::tempdir().unwrap();
            let archive = Archive::open(temp.path(), false).unwrap();
            let id = if kind == "novel" { "1004" } else { "1001" };
            let target = Target::new(kind, id).unwrap();
            let fixture = Fixture::new(DEMO, Arc::default()).unwrap();
            assert_eq!(archive.save(&target, &fixture).unwrap(), "complete");
            let root = temp.path().join(format!("works/{}", target.key()));
            let manifest = fs::read(root.join("manifest.json")).unwrap();
            let metadata = fs::read(root.join("metadata.json")).unwrap();
            fs::write(root.join("raw.json"), b"damaged raw").unwrap();
            let mut changed: Value = serde_json::from_str(DEMO).unwrap();
            let endpoint = if kind == "novel" { "novel" } else { "illust" };
            if kind == "novel" {
                changed["responses"][format!("/ajax/{endpoint}/{id}")]["coverUrl"] =
                    json!("https://i.pximg.net/synthetic/new-cover.png");
            } else {
                changed["responses"][format!("/ajax/{endpoint}/{id}")]["uploadDate"] =
                    json!("2026-02-02T00:00:00+09:00");
            }
            let changed = Fixture::new(&changed.to_string(), Arc::default()).unwrap();
            assert_eq!(archive.save(&target, &changed).unwrap(), "failed");
            assert!(archive
                .reason(&target)
                .unwrap()
                .unwrap()
                .contains("update_needed"));
            assert_eq!(fs::read(root.join("manifest.json")).unwrap(), manifest);
            assert_eq!(fs::read(root.join("metadata.json")).unwrap(), metadata);
            assert_eq!(fs::read(root.join("raw.json")).unwrap(), b"damaged raw");
        }
    }
    #[test]
    fn wrong_manifest_identity_is_not_complete_or_exported() {
        let temp = tempfile::tempdir().unwrap();
        let archive = Archive::open(temp.path(), false).unwrap();
        let target = Target::parse("art:1001").unwrap();
        let fixture = Fixture::new(DEMO, Arc::default()).unwrap();
        archive.save(&target, &fixture).unwrap();
        let mut manifest = archive.manifest(&target).unwrap();
        manifest["key"] = json!("art_9999");
        atomic(
            &temp.path().join("works/art_1001/manifest.json"),
            &serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();
        assert!(!archive.complete(&target).unwrap());
        assert_eq!(archive.verify().unwrap()["valid"], 0);
        let export = tempfile::tempdir().unwrap();
        assert!(archive.pack(&export.path().join("export.zip")).is_err());
    }
    #[test]
    fn legacy_damaged_raw_refuses_unsafe_snapshot_reconstruction() {
        let temp = tempfile::tempdir().unwrap();
        let archive = Archive::open(temp.path(), false).unwrap();
        let target = Target::parse("novel:1004").unwrap();
        let fixture = Fixture::new(DEMO, Arc::default()).unwrap();
        archive.save(&target, &fixture).unwrap();
        let mut manifest = archive.manifest(&target).unwrap();
        manifest
            .as_object_mut()
            .unwrap()
            .remove("snapshot_identity");
        let root = temp.path().join("works/novel_1004");
        atomic(
            &root.join("manifest.json"),
            &serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();
        fs::write(root.join("raw.json"), b"damaged").unwrap();
        let fixture = Fixture::new(DEMO, Arc::default()).unwrap();
        assert_eq!(archive.save(&target, &fixture).unwrap(), "failed");
        assert_eq!(fixture.requests(), 0);
        assert!(archive
            .reason(&target)
            .unwrap()
            .unwrap()
            .contains("update_needed"));
        assert_eq!(fs::read(root.join("raw.json")).unwrap(), b"damaged");
    }
    #[test]
    fn crc_invalid_zip_is_not_reused_on_repair() {
        use base64::Engine;
        let temp = tempfile::tempdir().unwrap();
        let archive = Archive::open(temp.path(), false).unwrap();
        let target = Target::parse("art:1003").unwrap();
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        for name in ["000000.png", "000001.png"] {
            zip.start_file(name, opts).unwrap();
            zip.write_all(b"synthetic-payload").unwrap();
        }
        let mut bytes = zip.finish().unwrap().into_inner();
        let pos = bytes
            .windows(17)
            .position(|window| window == b"synthetic-payload")
            .unwrap();
        bytes[pos] ^= 1;
        let mut data: Value = serde_json::from_str(DEMO).unwrap();
        data["assets"]["https://i.pximg.net/synthetic/pixiv-archive-lab/1003_ugoira.zip"] =
            json!(base64::engine::general_purpose::STANDARD.encode(bytes));
        let bad = Fixture::new(&data.to_string(), Arc::default()).unwrap();
        assert_eq!(archive.save(&target, &bad).unwrap(), "failed");
        assert!(!archive.manifest(&target).unwrap()["files"]
            .as_array()
            .unwrap()
            .iter()
            .any(|file| file["path"] == "frames.zip"));
        let good = Fixture::new(DEMO, Arc::default()).unwrap();
        assert_eq!(archive.save(&target, &good).unwrap(), "complete");
        assert_eq!(
            good.requests(),
            1,
            "only ZIP needs fetching; frame metadata and detail are checkpointed"
        );
    }
    #[test]
    fn complete_manifest_cannot_omit_required_novel_body() {
        let temp = tempfile::tempdir().unwrap();
        let archive = Archive::open(temp.path(), false).unwrap();
        let target = Target::parse("novel:1004").unwrap();
        let fixture = Fixture::new(DEMO, Arc::default()).unwrap();
        archive.save(&target, &fixture).unwrap();
        let mut manifest = archive.manifest(&target).unwrap();
        manifest["files"]
            .as_array_mut()
            .unwrap()
            .retain(|entry| entry["path"] != "plain.txt");
        atomic(
            &temp.path().join("works/novel_1004/manifest.json"),
            &serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();
        assert!(!archive.complete(&target).unwrap());
        assert!(archive
            .check(&target)
            .iter()
            .any(|error| error.contains("plain.txt")));
    }
    #[test]
    fn fatal_io_errors_are_propagated_to_the_job_controller() {
        struct DiskFailure;
        impl Transport for DiskFailure {
            fn api(&self, _: &str, _: &[(&str, String)]) -> Result<Value> {
                Err(Error::Io(std::io::Error::other("simulated disk full")))
            }
            fn download(&self, _: &str, _: &Path) -> Result<()> {
                unreachable!()
            }
            fn requests(&self) -> usize {
                0
            }
            fn stopped(&self) -> bool {
                false
            }
            fn cancel(&self) {}
        }
        let temp = tempfile::tempdir().unwrap();
        let archive = Archive::open(temp.path(), false).unwrap();
        let target = Target::parse("novel:1004").unwrap();
        assert!(matches!(
            archive.prepare(&target, &DiskFailure),
            Err(Error::Io(_))
        ));
        assert_eq!(archive.status(&target).unwrap().as_deref(), Some("failed"));
    }
    #[test]
    fn os_lock_releases_and_only_versioned_stale_sentinels_are_recovered() {
        let temp = tempfile::tempdir().unwrap();
        let archive = Archive::open(temp.path(), false).unwrap();
        assert!(Archive::open(temp.path(), false).is_err());
        assert!(temp.path().join(".archive.lock").exists());
        assert!(temp.path().join(".run.lock").exists());
        drop(archive);
        assert!(!temp.path().join(".run.lock").exists());
        // Persistent guard is never unlinked, avoiding different-inode lock races.
        assert!(temp.path().join(".archive.lock").exists());
        fs::write(
            temp.path().join(".run.lock"),
            "OpenPixivDownloader archive lock v2\n999999\n",
        )
        .unwrap();
        let archive = Archive::open(temp.path(), true).unwrap();
        assert!(Archive::open(temp.path(), false).is_err());
        drop(archive);
        fs::write(temp.path().join(".run.lock"), "999999\n").unwrap();
        assert!(Archive::open(temp.path(), true)
            .err()
            .unwrap()
            .to_string()
            .contains("Legacy archive lock"));
        assert_eq!(
            fs::read(temp.path().join(".run.lock")).unwrap(),
            b"999999\n"
        );
    }
    #[test]
    fn damaged_raw_same_snapshot_is_repaired_without_refetching_materials() {
        let temp = tempfile::tempdir().unwrap();
        let archive = Archive::open(temp.path(), false).unwrap();
        let target = Target::parse("novel:1004").unwrap();
        let fixture = Fixture::new(DEMO, Arc::default()).unwrap();
        archive.save(&target, &fixture).unwrap();
        fs::write(temp.path().join("works/novel_1004/raw.json"), b"damaged").unwrap();
        let repaired = Fixture::new(DEMO, Arc::default()).unwrap();
        assert_eq!(archive.save(&target, &repaired).unwrap(), "complete");
        assert_eq!(repaired.requests(), 1);
    }
    #[test]
    fn complete_manifest_cannot_drop_an_expected_media_entry() {
        let temp = tempfile::tempdir().unwrap();
        let archive = Archive::open(temp.path(), false).unwrap();
        let target = Target::parse("art:1001").unwrap();
        let fixture = Fixture::new(DEMO, Arc::default()).unwrap();
        archive.save(&target, &fixture).unwrap();
        let mut manifest = archive.manifest(&target).unwrap();
        manifest["files"]
            .as_array_mut()
            .unwrap()
            .retain(|entry| entry["path"] != "images/0000.png");
        atomic(
            &temp.path().join("works/art_1001/manifest.json"),
            &serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();
        assert!(!archive.complete(&target).unwrap());
        assert!(archive
            .check(&target)
            .iter()
            .any(|error| error.contains("expected material")));
    }
    #[test]
    fn optional_cover_disk_failure_still_stops_the_job() {
        struct CoverDiskFailure(Fixture);
        impl Transport for CoverDiskFailure {
            fn api(&self, path: &str, query: &[(&str, String)]) -> Result<Value> {
                self.0.api(path, query)
            }
            fn download(&self, _: &str, _: &Path) -> Result<()> {
                Err(Error::Io(std::io::Error::other("simulated disk full")))
            }
            fn requests(&self) -> usize {
                self.0.requests()
            }
            fn stopped(&self) -> bool {
                self.0.stopped()
            }
            fn cancel(&self) {
                self.0.cancel();
            }
        }
        let temp = tempfile::tempdir().unwrap();
        let archive = Archive::open(temp.path(), false).unwrap();
        let target = Target::parse("novel:1004").unwrap();
        let client = CoverDiskFailure(Fixture::new(DEMO, Arc::default()).unwrap());
        assert_eq!(archive.prepare(&target, &client).unwrap(), "assets_pending");
        assert!(matches!(
            archive.finish(&target, &client),
            Err(Error::Io(_))
        ));
        assert_eq!(
            archive.manifest(&target).unwrap()["stages"]["body_ready"],
            true
        );
    }
    #[test]
    fn invalid_api_metadata_is_not_cached_forever() {
        for target_string in ["art:1001", "art:1003"] {
            let temp = tempfile::tempdir().unwrap();
            let archive = Archive::open(temp.path(), false).unwrap();
            let target = Target::parse(target_string).unwrap();
            let mut data: Value = serde_json::from_str(DEMO).unwrap();
            if target.id == "1001" {
                data["responses"]["/ajax/illust/1001/pages"] = json!([]);
            } else {
                data["responses"]["/ajax/illust/1003/ugoira_meta"]["frames"] = json!([]);
            }
            let bad = Fixture::new(&data.to_string(), Arc::default()).unwrap();
            assert_eq!(archive.save(&target, &bad).unwrap(), "failed");
            let good = Fixture::new(DEMO, Arc::default()).unwrap();
            assert_eq!(archive.save(&target, &good).unwrap(), "complete");
            assert_eq!(
                good.requests(),
                2,
                "re-fetch corrected metadata and missing media only"
            );
        }
    }
    #[test]
    fn inaccessible_insertion_relation_can_be_retried_explicitly() {
        let temp = tempfile::tempdir().unwrap();
        let archive = Archive::open(temp.path(), false).unwrap();
        let target = Target::parse("novel:1004").unwrap();
        let mut data: Value = serde_json::from_str(DEMO).unwrap();
        data["responses"]["/ajax/novel/1004/insert_illusts?id%5B%5D=1002-2"]["1002-2"]["visible"] =
            json!(false);
        let bad = Fixture::new(&data.to_string(), Arc::default()).unwrap();
        assert_eq!(archive.save(&target, &bad).unwrap(), "partial");
        let good = Fixture::new(DEMO, Arc::default()).unwrap();
        assert_eq!(archive.save(&target, &good).unwrap(), "complete");
        assert_eq!(good.requests(), 2);
    }
    struct MissingMedia {
        fixture: Fixture,
        needle: &'static str,
        status: u16,
        failures: std::sync::atomic::AtomicUsize,
    }
    impl Transport for MissingMedia {
        fn api(&self, path: &str, query: &[(&str, String)]) -> Result<Value> {
            self.fixture.api(path, query)
        }
        fn download(&self, url: &str, path: &Path) -> Result<()> {
            if url.contains(self.needle) {
                self.failures
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Err(Error::Missing {
                    status: self.status,
                })
            } else {
                self.fixture.download(url, path)
            }
        }
        fn requests(&self) -> usize {
            self.fixture.requests() + self.failures.load(std::sync::atomic::Ordering::SeqCst)
        }
        fn stopped(&self) -> bool {
            self.fixture.stopped()
        }
        fn cancel(&self) {
            self.fixture.cancel();
        }
    }
    #[test]
    fn typed_stale_cover_rechecks_detail_once_and_repairs_only_the_cover() {
        for status in [404, 410] {
            let temp = tempfile::tempdir().unwrap();
            let archive = Archive::open(temp.path(), false).unwrap();
            let target = Target::parse("novel:1004").unwrap();
            let bad = MissingMedia {
                fixture: Fixture::new(DEMO, Arc::default()).unwrap(),
                needle: "1004_cover",
                status,
                failures: 0.into(),
            };
            assert_eq!(archive.save(&target, &bad).unwrap(), "partial");
            let manifest = archive.manifest(&target).unwrap();
            assert_eq!(
                manifest["resources"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|resource| resource["role"] == "cover")
                    .unwrap()["last_http_status"],
                status
            );
            let repaired = Fixture::new(DEMO, Arc::default()).unwrap();
            assert_eq!(archive.save(&target, &repaired).unwrap(), "complete");
            assert_eq!(
                repaired.requests(),
                2,
                "one detail refresh and one missing cover"
            );
            assert!(!archive.manifest(&target).unwrap()["resources"]
                .as_array()
                .unwrap()
                .iter()
                .any(|resource| resource.get("last_http_status").is_some()));
        }
    }
    #[test]
    fn stale_cover_changed_reference_requires_update_without_mixing_snapshots() {
        let temp = tempfile::tempdir().unwrap();
        let archive = Archive::open(temp.path(), false).unwrap();
        let target = Target::parse("novel:1004").unwrap();
        let bad = MissingMedia {
            fixture: Fixture::new(DEMO, Arc::default()).unwrap(),
            needle: "1004_cover",
            status: 404,
            failures: 0.into(),
        };
        assert_eq!(archive.save(&target, &bad).unwrap(), "partial");
        let root = temp.path().join("works/novel_1004");
        let saved: Vec<_> = [
            "raw.json",
            "source.txt",
            "plain.txt",
            "metadata.json",
            "manifest.json",
        ]
        .into_iter()
        .map(|name| (name, fs::read(root.join(name)).unwrap()))
        .collect();
        let mut data: Value = serde_json::from_str(DEMO).unwrap();
        data["responses"]["/ajax/novel/1004"]["coverUrl"] =
            json!("https://i.pximg.net/synthetic/new-cover.png");
        let updated = Fixture::new(&data.to_string(), Arc::default()).unwrap();
        assert_eq!(archive.save(&target, &updated).unwrap(), "failed");
        assert_eq!(updated.requests(), 1);
        assert!(archive
            .reason(&target)
            .unwrap()
            .unwrap()
            .contains("update_needed"));
        for (name, bytes) in saved {
            assert_eq!(fs::read(root.join(name)).unwrap(), bytes);
        }
    }
    #[test]
    fn unchanged_stale_cover_remains_explicitly_missing() {
        let temp = tempfile::tempdir().unwrap();
        let archive = Archive::open(temp.path(), false).unwrap();
        let target = Target::parse("novel:1004").unwrap();
        for _ in 0..2 {
            let bad = MissingMedia {
                fixture: Fixture::new(DEMO, Arc::default()).unwrap(),
                needle: "1004_cover",
                status: 404,
                failures: 0.into(),
            };
            assert_eq!(archive.save(&target, &bad).unwrap(), "partial");
        }
        let manifest = archive.manifest(&target).unwrap();
        assert_eq!(manifest["status"], "partial");
        assert_eq!(
            manifest["resources"]
                .as_array()
                .unwrap()
                .iter()
                .find(|resource| resource["role"] == "cover")
                .unwrap()["last_http_status"],
            404
        );
        assert!(!archive.complete(&target).unwrap());
    }
    #[test]
    fn stale_artwork_or_inserted_image_refreshes_reference_metadata() {
        for (target_text, needle, expected) in [
            ("art:1001", "1001_p0", 3),
            ("art:1003", "1003_ugoira", 3),
            ("novel:1004", "1002_p1", 3),
        ] {
            let temp = tempfile::tempdir().unwrap();
            let archive = Archive::open(temp.path(), false).unwrap();
            let target = Target::parse(target_text).unwrap();
            let bad = MissingMedia {
                fixture: Fixture::new(DEMO, Arc::default()).unwrap(),
                needle,
                status: 404,
                failures: 0.into(),
            };
            assert_eq!(archive.save(&target, &bad).unwrap(), "partial");
            let good = Fixture::new(DEMO, Arc::default()).unwrap();
            assert_eq!(archive.save(&target, &good).unwrap(), "complete");
            assert_eq!(good.requests(), expected, "{target_text}");
        }
    }
    #[test]
    fn changed_bytes_at_same_url_never_replace_the_saved_expected_original() {
        use base64::Engine;
        for target_text in ["art:1001", "novel:1004"] {
            for missing in [false, true] {
                let temp = tempfile::tempdir().unwrap();
                let archive = Archive::open(temp.path(), false).unwrap();
                let target = Target::parse(target_text).unwrap();
                let fixture = Fixture::new(DEMO, Arc::default()).unwrap();
                assert_eq!(archive.save(&target, &fixture).unwrap(), "complete");
                let relative = if target.kind == "art" {
                    "images/0000.png"
                } else {
                    "cover.png"
                };
                let path = temp
                    .path()
                    .join(format!("works/{}/{relative}", target.key()));
                let original = fs::read(&path).unwrap();
                let original_hash = format!("{:x}", Sha256::digest(&original));
                let entry = archive.manifest(&target).unwrap()["files"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|entry| entry["path"] == relative)
                    .unwrap()
                    .clone();
                let url = entry["source_url"].as_str().unwrap();
                if missing {
                    fs::remove_file(&path).unwrap();
                } else {
                    fs::write(&path, b"damaged local copy to preserve").unwrap();
                }
                let mut changed = original.clone();
                changed[0] ^= 1; // Same length, different hash, same source URL.
                let mut data: Value = serde_json::from_str(DEMO).unwrap();
                data["assets"][url] =
                    json!(base64::engine::general_purpose::STANDARD.encode(changed));
                for _ in 0..2 {
                    let fixture = Fixture::new(&data.to_string(), Arc::default()).unwrap();
                    assert_eq!(archive.save(&target, &fixture).unwrap(), "failed");
                    assert_eq!(
                        fixture.requests(),
                        1,
                        "only the untrusted replacement is fetched"
                    );
                    if missing {
                        assert!(!path.exists());
                    } else {
                        assert_eq!(fs::read(&path).unwrap(), b"damaged local copy to preserve");
                    }
                    assert!(archive
                        .reason(&target)
                        .unwrap()
                        .unwrap()
                        .contains("update_needed"));
                    let manifest = archive.manifest(&target).unwrap();
                    let resource = manifest["resources"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .find(|resource| resource["path"] == relative)
                        .unwrap();
                    assert_eq!(resource["expected_sha256"], original_hash);
                    assert_eq!(resource["expected_bytes"], original.len() as u64);
                    assert!(!archive.complete(&target).unwrap());
                }
                let fixture = Fixture::new(DEMO, Arc::default()).unwrap();
                assert_eq!(archive.save(&target, &fixture).unwrap(), "complete");
                assert_eq!(fixture.requests(), 1);
                assert_eq!(fs::read(&path).unwrap(), original);
            }
        }
    }
    #[test]
    fn expected_media_identity_survives_missing_pages_metadata_and_multiple_prepares() {
        use base64::Engine;
        let temp = tempfile::tempdir().unwrap();
        let archive = Archive::open(temp.path(), false).unwrap();
        let target = Target::parse("art:1001").unwrap();
        let fixture = Fixture::new(DEMO, Arc::default()).unwrap();
        archive.save(&target, &fixture).unwrap();
        let root = temp.path().join("works/art_1001");
        let original = fs::read(root.join("images/0000.png")).unwrap();
        fs::remove_file(root.join("images/0000.png")).unwrap();
        fs::remove_file(root.join("pages.json")).unwrap();
        for _ in 0..2 {
            assert_eq!(
                archive.prepare(&target, &fixture).unwrap(),
                "assets_pending"
            );
            let manifest = archive.manifest(&target).unwrap();
            assert_eq!(
                manifest["resources"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|resource| resource["path"] == "images/0000.png")
                    .unwrap()["expected_sha256"],
                format!("{:x}", Sha256::digest(&original))
            );
        }
        let mut data: Value = serde_json::from_str(DEMO).unwrap();
        let mut changed = original;
        changed[0] ^= 1;
        data["assets"]["https://i.pximg.net/synthetic/pixiv-archive-lab/1001_p0.png"] =
            json!(base64::engine::general_purpose::STANDARD.encode(changed));
        let bad = Fixture::new(&data.to_string(), Arc::default()).unwrap();
        assert_eq!(archive.finish(&target, &bad).unwrap(), "failed");
        assert!(!root.join("images/0000.png").exists());
    }
    #[test]
    fn semantic_zip_mismatch_does_not_discard_a_previously_verified_original() {
        let temp = tempfile::tempdir().unwrap();
        let archive = Archive::open(temp.path(), false).unwrap();
        let target = Target::parse("art:1003").unwrap();
        let fixture = Fixture::new(DEMO, Arc::default()).unwrap();
        archive.save(&target, &fixture).unwrap();
        let root = temp.path().join("works/art_1003");
        let original = fs::read(root.join("frames.zip")).unwrap();
        let hash = format!("{:x}", Sha256::digest(&original));
        fs::remove_file(root.join("frames.json")).unwrap();
        let mut data: Value = serde_json::from_str(DEMO).unwrap();
        data["responses"]["/ajax/illust/1003/ugoira_meta"]["frames"][0]["file"] =
            json!("changed.png");
        let bad = Fixture::new(&data.to_string(), Arc::default()).unwrap();
        assert_eq!(archive.save(&target, &bad).unwrap(), "failed");
        let manifest = archive.manifest(&target).unwrap();
        let resource = manifest["resources"]
            .as_array()
            .unwrap()
            .iter()
            .find(|resource| resource["path"] == "frames.zip")
            .unwrap();
        assert_eq!(resource["expected_sha256"], hash);
        assert_eq!(resource["expected_payload_verified"], true);
        assert_eq!(fs::read(root.join("frames.zip")).unwrap(), original);
        fs::remove_file(root.join("frames.json")).unwrap();
        let good = Fixture::new(DEMO, Arc::default()).unwrap();
        assert_eq!(archive.save(&target, &good).unwrap(), "complete");
        assert_eq!(
            good.requests(),
            1,
            "refetch metadata but reuse the verified original ZIP"
        );
    }
    #[test]
    fn four_formats_resume_repair_and_export() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("日本語");
        let a = Archive::open(&root, false).unwrap();
        let f = Fixture::new(DEMO, Arc::default()).unwrap();
        for t in discover(&f, &Target::parse("user:9001").unwrap(), "all").unwrap() {
            assert_eq!(a.save(&t, &f).unwrap(), "complete");
        }
        assert_eq!(a.verify().unwrap()["valid"], 4);
        let novel = fs::read_to_string(root.join("works/novel_1004/index.html")).unwrap();
        assert!(novel.contains("<ruby>出発"));
        assert_eq!(novel.matches("<img ").count(), 2);
        let t = Target::parse("art:1001").unwrap();
        let before = f.requests();
        assert_eq!(a.save(&t, &f).unwrap(), "already_complete");
        assert_eq!(f.requests(), before);
        fs::write(root.join("works/art_1001/images/0000.png"), b"broken").unwrap();
        assert_eq!(a.verify().unwrap()["problems"].as_array().unwrap().len(), 1);
        assert_eq!(a.save(&t, &f).unwrap(), "complete");
        assert_eq!(
            a.pack(&temp.path().join("export.zip")).unwrap()["complete_works"],
            4
        );
        assert!(a.pack(&root.join("export.zip")).is_err());
        assert!(Archive::open(&root, false).is_err());
        drop(a);
        assert!(Archive::open(&root, true).is_ok());
    }
    #[test]
    fn omissions_and_page_mismatch_are_not_complete() {
        let mut data: Value = serde_json::from_str(DEMO).unwrap();
        data["assets"]
            .as_object_mut()
            .unwrap()
            .remove("https://i.pximg.net/synthetic/pixiv-archive-lab/1004_cover.png");
        data["responses"]["/ajax/illust/1002"]["pageCount"] = json!(3);
        let f = Fixture::new(&data.to_string(), Arc::default()).unwrap();
        let temp = tempfile::tempdir().unwrap();
        let a = Archive::open(temp.path(), false).unwrap();
        assert_eq!(
            a.save(&Target::parse("novel:1004").unwrap(), &f).unwrap(),
            "partial"
        );
        assert_eq!(
            a.save(&Target::parse("art:1002").unwrap(), &f).unwrap(),
            "failed"
        );
    }
    #[test]
    fn unsafe_paths_and_access_fields() {
        for s in ["../a", "/a", "a\\b", "C:/x", "a/./b", "a//b", "a\0b"] {
            assert!(safe_path(Path::new("/tmp"), s).is_err());
        }
        assert!(!eligible(&json!({"id":"1"})));
        assert!(!eligible(&json!({"xRestrict":2})));
        assert!(eligible(&json!({"xRestrict":0})));
    }
    #[test]
    fn crc_corruption_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("frames.zip");
        let mut z = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        z.start_file(
            "a.png",
            zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored),
        )
        .unwrap();
        z.write_all(b"fixture-frame-payload").unwrap();
        let mut bytes = z.finish().unwrap().into_inner();
        let pos = bytes
            .windows(21)
            .position(|w| w == b"fixture-frame-payload")
            .unwrap();
        bytes[pos] ^= 1;
        fs::write(&file, bytes).unwrap();
        assert!(validate_frames(&file, &HashSet::from(["a.png".into()])).is_err());
    }
}
