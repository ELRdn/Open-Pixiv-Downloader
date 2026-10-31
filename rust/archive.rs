use crate::{
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
fn str_at<'a>(value: &'a Value, key: &str) -> Result<&'a str> {
    value[key]
        .as_str()
        .ok_or_else(|| invalid(&format!("Missing or invalid field: {key}")))
}

pub fn discover(client: &dyn Transport, target: &Target, types: &str) -> Result<Vec<Target>> {
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
    db: Mutex<Connection>,
    lock: PathBuf,
}
type StateRow = (Target, String, String, Option<String>);
impl Archive {
    pub fn open(root: &Path, existing: bool) -> Result<Self> {
        if existing && !root.join("state.sqlite3").is_file() {
            return Err(invalid("Archive database does not exist"));
        }
        fs::create_dir_all(root)?;
        let root = root.canonicalize()?;
        let lock = safe_path(&root, ".run.lock")?;
        let mut file = OpenOptions::new().write(true).create_new(true).open(&lock).map_err(|e|
            if e.kind() == std::io::ErrorKind::AlreadyExists { invalid("Archive is locked. Confirm no process is running before removing .run.lock") } else { Error::Io(e) })?;
        let result = (|| {
            writeln!(file, "{}", std::process::id())?;
            let db = Connection::open(safe_path(&root, "state.sqlite3")?)?;
            db.execute_batch("CREATE TABLE IF NOT EXISTS works(kind TEXT NOT NULL,id TEXT NOT NULL,status TEXT NOT NULL,updated_at TEXT NOT NULL,reason TEXT,PRIMARY KEY(kind,id));")?;
            Ok(Self {
                root,
                db: Mutex::new(db),
                lock: lock.clone(),
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
        let Some(files) = manifest["files"].as_array().filter(|v| !v.is_empty()) else {
            return vec!["missing or invalid manifest files".into()];
        };
        files
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
            .collect()
    }
    pub fn complete(&self, target: &Target) -> Result<bool> {
        Ok(self.status(target)?.as_deref() == Some("complete")
            && self
                .manifest(target)
                .is_some_and(|m| m["status"] == "complete")
            && self.check(target).is_empty())
    }
    pub fn save(&self, target: &Target, client: &dyn Transport) -> Result<String> {
        if self.complete(target)? {
            return Ok("already_complete".into());
        }
        self.state(target, "in_progress", None)?;
        match self.save_body(target, client) {
            Ok(status) => {
                self.state(target, &status, None)?;
                Ok(status)
            }
            Err(e @ Error::Stop(_)) => {
                client.cancel();
                self.state(target, "paused", Some(&e.to_string()))?;
                Err(e)
            }
            Err(e @ Error::Unavailable(_)) => {
                self.state(target, "unavailable", Some(&e.to_string()))?;
                Ok("unavailable".into())
            }
            Err(e) => {
                self.state(target, "failed", Some(&e.to_string()))?;
                Ok("failed".into())
            }
        }
    }
    fn save_body(&self, target: &Target, client: &dyn Transport) -> Result<String> {
        let endpoint = if target.kind == "art" {
            "illust"
        } else {
            "novel"
        };
        let body = client.api(&format!("/ajax/{endpoint}/{}", target.id), &[])?;
        if !eligible(&body) {
            return Ok("skipped".into());
        }
        if body["id"] != target.id {
            return Err(invalid("Returned work ID does not match request"));
        }
        let art_type = body["illustType"].as_u64();
        let kind = if target.kind == "novel" {
            "novel"
        } else {
            match art_type {
                Some(0) => "illustration",
                Some(1) => "manga",
                Some(2) => "ugoira",
                _ => return Err(invalid("Unknown artwork type")),
            }
        };
        let metadata = json!({"schema_version":1,"id":target.id,"kind":kind,"title":body["title"],"author_id":body["userId"],"author_name":body["userName"],"source_url":target.url(),"created_at":body["createDate"],"updated_at":body["uploadDate"],"tags":body["tags"],"series":body["seriesNavData"],"archived_at":now(),"source_visibility":"public_general","rights":{"copyright_holder":"original author","license":"unknown","ai_training_permission":"unknown","redistribution_permission":"unknown"}});
        let mut writer = WorkWriter {
            archive: self,
            target,
            client,
            old: self.manifest(target),
            files: vec![],
            omissions: vec![],
        };
        writer.json("raw.json", &body)?;
        writer.json("metadata.json", &metadata)?;
        if target.kind == "art" && art_type != Some(2) {
            let pages = client.api(&format!("/ajax/illust/{}/pages", target.id), &[])?;
            let list = pages
                .as_array()
                .ok_or_else(|| invalid("Image page schema changed"))?;
            if list.is_empty() || body["pageCount"].as_u64() != Some(list.len() as u64) {
                return Err(invalid("Image-page count does not match metadata"));
            }
            writer.json("pages.json", &pages)?;
            for (i, page) in list.iter().enumerate() {
                let url = str_at(&page["urls"], "original")?;
                writer.asset(url, &format!("images/{i:04}{}", extension(url)?), false)?;
            }
        } else if target.kind == "art" {
            let motion = client.api(&format!("/ajax/illust/{}/ugoira_meta", target.id), &[])?;
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
            writer.asset(str_at(&motion, "originalSrc")?, "frames.zip", false)?;
            validate_frames(&self.path(target, "frames.zip")?, &names)?;
            writer.json("frames.json", &motion)?;
        } else {
            let content = str_at(&body, "content")?;
            if content.is_empty() && body["textCount"].as_u64().unwrap_or(0) > 0 {
                return Err(invalid("Novel body is empty despite declared text count"));
            }
            writer.write("source.txt", content.as_bytes())?;
            writer.write("plain.txt", novel::plain_text(content).as_bytes())?;
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
                    let response = match client.api(
                        &format!("/ajax/novel/{}/insert_illusts", target.id),
                        &[("id[]", image_id.into())],
                    ) {
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
        let manifest = json!({"schema_version":1,"tool_version":env!("CARGO_PKG_VERSION"),"key":target.key(),"source_url":target.url(),"archived_at":now(),"status":status,"completeness_scope":"supported work payload; excludes comments, likes and account data","files":writer.files,"omissions":writer.omissions});
        atomic(
            &self.path(target, "manifest.json")?,
            &serde_json::to_vec_pretty(&manifest)?,
        )?;
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
    pub fn verify(&self) -> Result<Value> {
        let mut checked = 0;
        let mut valid = 0;
        let mut problems = Vec::new();
        for (t, status, _, _) in self.rows()? {
            if !matches!(status.as_str(), "complete" | "partial") {
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
                included.push(t.key());
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

struct WorkWriter<'a> {
    archive: &'a Archive,
    target: &'a Target,
    client: &'a dyn Transport,
    old: Option<Value>,
    files: Vec<Value>,
    omissions: Vec<Value>,
}
impl WorkWriter<'_> {
    fn record(&mut self, relative: &str, url: Option<&str>) -> Result<()> {
        let (hash, bytes) = fingerprint(&self.archive.path(self.target, relative)?)?;
        let mut entry = json!({"path":relative,"sha256":hash,"bytes":bytes});
        if let Some(url) = url {
            entry["source_url"] = json!(url);
        }
        self.files.push(entry);
        Ok(())
    }
    fn write(&mut self, relative: &str, data: &[u8]) -> Result<()> {
        atomic(&self.archive.path(self.target, relative)?, data)?;
        self.record(relative, None)
    }
    fn json(&mut self, relative: &str, value: &Value) -> Result<()> {
        self.write(relative, &serde_json::to_vec_pretty(value)?)
    }
    fn omit(&mut self, resource: &str, reason: &str) {
        self.omissions
            .push(json!({"resource":resource,"reason":reason}));
    }
    fn asset(&mut self, url: &str, relative: &str, optional: bool) -> Result<bool> {
        let path = self.archive.path(self.target, relative)?;
        let old = self
            .old
            .as_ref()
            .and_then(|m| m["files"].as_array())
            .and_then(|f| {
                f.iter()
                    .find(|e| e["path"] == relative && e["source_url"] == url)
            });
        if old.is_some_and(|entry| {
            fingerprint(&path).is_ok_and(|(h, b)| entry["sha256"] == h && entry["bytes"] == b)
        }) {
            self.record(relative, Some(url))?;
            return Ok(true);
        }
        match self.client.download(url, &path) {
            Ok(()) => {
                self.record(relative, Some(url))?;
                Ok(true)
            }
            Err(e @ Error::Stop(_)) => Err(e),
            Err(e) if optional => {
                self.omit(relative, &e.to_string());
                Ok(false)
            }
            Err(e) => Err(e),
        }
    }
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
        )?;
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
