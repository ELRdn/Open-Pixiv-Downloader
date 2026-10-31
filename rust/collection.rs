//! Independent, user-facing copies. Editing these files must not alter the archive.
use crate::{
    archive::{atomic, safe_path},
    error::{invalid, Error, Result},
    target::Target,
};
use serde::Serialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File},
    io::{Read, Write},
    path::Path,
};

#[derive(Default, Serialize)]
pub struct Report {
    pub works: usize,
    pub copied: usize,
    pub existing: usize,
    pub cancelled: bool,
    /// Actual filesystem failures stop new HTTP; edited-copy conflicts do not.
    pub fatal_io: bool,
    pub files: Vec<Value>,
    pub problems: Vec<Value>,
}
impl Report {
    pub fn merge(&mut self, other: Self) {
        self.works += other.works;
        self.copied += other.copied;
        self.existing += other.existing;
        self.cancelled |= other.cancelled;
        self.fatal_io |= other.fatal_io;
        self.files.extend(other.files);
        self.problems.extend(other.problems);
    }
    pub fn problem(&mut self, key: &str, path: &str, error: impl std::fmt::Display) {
        self.problems
            .push(json!({"key":key,"path":path,"reason":error.to_string()}));
    }
    pub fn value(&self, root: &Path) -> Result<Value> {
        let mut value = serde_json::to_value(self)?;
        value["status"] = json!(if self.cancelled {
            "interrupted"
        } else if self.problems.is_empty() {
            "complete"
        } else {
            "partial"
        });
        value["image_directory"] = json!(root.join("works/image"));
        value["novel_directory"] = json!(root.join("works/novel"));
        Ok(value)
    }
    pub fn write(&self, root: &Path) -> Result<Value> {
        let value = self.value(root)?;
        atomic(
            &safe_path(root, "collection-report.json")?,
            &serde_json::to_vec_pretty(&value)?,
        )?;
        Ok(value)
    }
}
pub fn prepare(root: &Path) -> Result<()> {
    for name in ["works/image", "works/novel"] {
        fs::create_dir_all(safe_path(root, name)?)?;
    }
    Ok(())
}
fn destination(target: &Target, source: &str) -> Option<String> {
    if target.kind == "novel" {
        return (source == "plain.txt").then(|| format!("works/novel/{}.txt", target.key()));
    }
    if target.kind != "art" {
        return None;
    }
    match source {
        "frames.zip" => return Some(format!("works/image/{}_ugoira.zip", target.key())),
        "frames.json" => return Some(format!("works/image/{}_frames.json", target.key())),
        _ => {}
    }
    let name = source.strip_prefix("images/")?;
    let (page, ext) = name.rsplit_once('.')?;
    if page.len() < 4
        || page.len() > 10
        || !page.bytes().all(|c| c.is_ascii_digit())
        || !["jpg", "jpeg", "png", "gif", "webp", "avif"].contains(&ext)
    {
        return None;
    }
    Some(format!("works/image/{}_p{page}.{ext}", target.key()))
}
fn check_cancel(cancel: &dyn Fn() -> bool) -> Result<()> {
    if cancel() {
        Err(Error::Stop(
            "集約を中断しました。保存済み作品は保持されています。".into(),
        ))
    } else {
        Ok(())
    }
}
fn hash_file(path: &Path, cancel: &dyn Fn() -> bool) -> Result<(String, u64)> {
    let mut source = File::open(path)?;
    let mut hash = Sha256::new();
    let mut bytes = 0;
    let mut buf = [0; 65536];
    loop {
        check_cancel(cancel)?;
        let n = source.read(&mut buf)?;
        if n == 0 {
            break;
        }
        bytes += n as u64;
        hash.update(&buf[..n]);
    }
    Ok((format!("{:x}", hash.finalize()), bytes))
}
fn copy_checked(
    root: &Path,
    source: &str,
    output: &str,
    entry: &Value,
    cancel: &dyn Fn() -> bool,
) -> Result<bool> {
    check_cancel(cancel)?;
    let source = safe_path(root, source)?;
    let output = safe_path(root, output)?;
    let expected_hash = entry["sha256"]
        .as_str()
        .filter(|h| h.len() == 64 && h.bytes().all(|c| c.is_ascii_hexdigit()))
        .ok_or_else(|| invalid("集約元のハッシュ情報が不正です。"))?;
    let expected_bytes = entry["bytes"]
        .as_u64()
        .ok_or_else(|| invalid("集約元のサイズ情報が不正です。"))?;
    // A preexisting file is never replaced, even if an earlier collection created it.
    if output.exists() {
        let (hash, bytes) = hash_file(&output, cancel)?;
        if hash == expected_hash && bytes == expected_bytes {
            return Ok(false);
        }
        return Err(invalid("集約先に内容の異なるファイルがあります。名前を変更するか別の場所に移してから再集約してください。"));
    }
    let mut source = File::open(source)?;
    let mut temp = tempfile::NamedTempFile::new_in(output.parent().unwrap())?;
    let mut hash = Sha256::new();
    let mut bytes = 0;
    let mut buf = [0; 65536];
    loop {
        check_cancel(cancel)?;
        let n = source.read(&mut buf)?;
        if n == 0 {
            break;
        }
        bytes += n as u64;
        if bytes > expected_bytes {
            return Err(invalid("集約元のファイルサイズが変わっています。"));
        }
        hash.update(&buf[..n]);
        temp.write_all(&buf[..n])?;
    }
    if bytes != expected_bytes || format!("{:x}", hash.finalize()) != expected_hash {
        return Err(invalid(
            "集約元のファイルが変更されています。整合性検査を実行してください。",
        ));
    }
    temp.as_file().sync_all()?;
    check_cancel(cancel)?;
    // Atomic no-clobber also protects a file created while copying.
    temp.persist_noclobber(&output)
        .map_err(|e| Error::Io(e.error))?;
    Ok(true)
}
pub fn work(
    root: &Path,
    target: &Target,
    manifest: Option<Value>,
    cancel: &dyn Fn() -> bool,
) -> Report {
    let mut report = Report {
        works: 1,
        ..Default::default()
    };
    let key = target.key();
    let result = (|| -> Result<()> {
        check_cancel(cancel)?;
        prepare(root)?;
        let manifest = manifest.ok_or_else(|| invalid("集約元のmanifestがありません。"))?;
        if manifest["key"] != key
            || !matches!(manifest["status"].as_str(), Some("complete" | "partial"))
        {
            return Err(invalid("集約元の作品情報を確認できません。"));
        }
        let entries = manifest["files"]
            .as_array()
            .ok_or_else(|| invalid("集約元のファイル一覧が不正です。"))?;
        let mut eligible = 0;
        for entry in entries {
            let Some(relative) = entry["path"].as_str() else {
                continue;
            };
            let Some(output) = destination(target, relative) else {
                continue;
            };
            eligible += 1;
            let source = format!("works/{key}/{relative}");
            match copy_checked(root, &source, &output, entry, cancel) {
                Ok(copied) => {
                    if copied {
                        report.copied += 1;
                    } else {
                        report.existing += 1;
                    }
                    report.files.push(json!({"key":key,"source":source,"output":output,"status":if copied{"copied"}else{"existing"}}));
                }
                Err(e @ Error::Stop(_)) | Err(e @ Error::Io(_)) => return Err(e),
                Err(e) => report.problem(&key, &output, e),
            }
        }
        if eligible == 0 {
            return Err(invalid("集約できる画像または小説本文がありません。"));
        }
        Ok(())
    })();
    match result {
        Err(Error::Stop(_)) => report.cancelled = true,
        Err(e @ Error::Io(_)) => {
            report.fatal_io = true;
            report.problem(&key, "", e);
        }
        Err(e) => report.problem(&key, "", e),
        Ok(()) => {}
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        archive::Archive,
        transport::{Fixture, DEMO},
    };
    use std::sync::Arc;
    #[test]
    fn useful_files_are_independent_deduplicated_copies_with_stable_names() {
        let temp = tempfile::tempdir().unwrap();
        let archive = Archive::open(temp.path(), false).unwrap();
        let fixture = Fixture::new(DEMO, Arc::default()).unwrap();
        let mut report = Report::default();
        for name in ["art:1001", "art:1002", "art:1003", "novel:1004"] {
            let target = Target::parse(name).unwrap();
            assert_eq!(archive.save(&target, &fixture).unwrap(), "complete");
            report.merge(work(
                &archive.root,
                &target,
                archive.manifest(&target),
                &|| false,
            ));
        }
        assert_eq!(report.copied, 6);
        assert!(report.problems.is_empty());
        let image = temp.path().join("works/image/art_1002_p0000.png");
        let source = temp.path().join("works/art_1002/images/0000.png");
        assert_eq!(fs::read(&image).unwrap(), fs::read(&source).unwrap());
        assert_eq!(
            fs::read(temp.path().join("works/novel/novel_1004.txt")).unwrap(),
            fs::read(temp.path().join("works/novel_1004/plain.txt")).unwrap()
        );
        assert_eq!(
            fs::read_dir(temp.path().join("works/novel"))
                .unwrap()
                .count(),
            1
        );
        let target = Target::parse("art:1002").unwrap();
        let repeated = work(&archive.root, &target, archive.manifest(&target), &|| false);
        assert_eq!(repeated.existing, 2);
        assert_eq!(repeated.copied, 0);
        fs::write(&image, b"edited copy").unwrap();
        assert_ne!(fs::read(&image).unwrap(), fs::read(&source).unwrap());
        let conflict = work(&archive.root, &target, archive.manifest(&target), &|| false);
        assert_eq!(conflict.problems.len(), 1);
        assert_eq!(fs::read(&image).unwrap(), b"edited copy");
        assert!(archive.check(&target).is_empty());
        fs::remove_file(&image).unwrap();
        let restored = work(&archive.root, &target, archive.manifest(&target), &|| false);
        assert_eq!(restored.copied, 1);
        assert!(restored.problems.is_empty());
    }
    #[test]
    fn corrupt_source_and_cancelled_copy_do_not_publish_outputs() {
        let temp = tempfile::tempdir().unwrap();
        let archive = Archive::open(temp.path(), false).unwrap();
        let fixture = Fixture::new(DEMO, Arc::default()).unwrap();
        let target = Target::parse("art:1001").unwrap();
        archive.save(&target, &fixture).unwrap();
        fs::write(
            temp.path().join("works/art_1001/images/0000.png"),
            b"corrupt",
        )
        .unwrap();
        let report = work(&archive.root, &target, archive.manifest(&target), &|| false);
        assert_eq!(report.problems.len(), 1);
        assert!(!temp.path().join("works/image/art_1001_p0000.png").exists());
        assert_eq!(
            fs::read_dir(temp.path().join("works/image"))
                .unwrap()
                .count(),
            0
        );
        let report = work(&archive.root, &target, archive.manifest(&target), &|| true);
        assert!(report.cancelled);
        let checks = std::cell::Cell::new(0);
        let report = work(&archive.root, &target, archive.manifest(&target), &|| {
            checks.set(checks.get() + 1);
            checks.get() >= 4
        });
        assert!(report.cancelled);
        assert_eq!(
            fs::read_dir(temp.path().join("works/image"))
                .unwrap()
                .count(),
            0
        );
        assert!(destination(&target, "images/../raw.json").is_none());
        assert!(destination(&target, "metadata.json").is_none());
    }
    #[cfg(unix)]
    #[test]
    fn collection_never_writes_through_a_symlink() {
        let temp = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let archive = Archive::open(temp.path(), false).unwrap();
        let fixture = Fixture::new(DEMO, Arc::default()).unwrap();
        let target = Target::parse("art:1001").unwrap();
        archive.save(&target, &fixture).unwrap();
        std::os::unix::fs::symlink(outside.path(), temp.path().join("works/image")).unwrap();
        let report = work(&archive.root, &target, archive.manifest(&target), &|| false);
        assert_eq!(report.copied, 0);
        assert_eq!(report.problems.len(), 1);
        assert_eq!(fs::read_dir(outside.path()).unwrap().count(), 0);
    }
}
