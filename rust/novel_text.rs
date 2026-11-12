use super::*;

fn read_text(path: &Path) -> Result<String> {
    if fs::metadata(path)?.len() > 64 * 1024 * 1024 {
        return Err(invalid("Novel TXT exceeds 64 MiB"));
    }
    let text = fs::read_to_string(path)?;
    if text.trim_start_matches('\u{feff}').trim().is_empty() || text.contains('\0') {
        return Err(invalid("Novel TXT must contain nonempty UTF-8 text"));
    }
    Ok(text)
}

impl Archive {
    pub(super) fn text_path(&self, target: &Target) -> Result<PathBuf> {
        if target.kind != "novel" {
            return Err(invalid(
                "本文のみモードは小説専用です。小説URLを指定してください。",
            ));
        }
        safe_path(&self.root, &format!("novel/{}.txt", target.key()))
    }

    fn text_record(&self, target: &Target) -> Result<Option<(String, u64)>> {
        Ok(self
            .db
            .lock()
            .unwrap()
            .query_row(
                "SELECT sha256,bytes FROM novel_text WHERE id=?1",
                params![target.id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?)
    }

    pub(super) fn text_targets(&self) -> Result<Vec<Target>> {
        let db = self.db.lock().unwrap();
        let mut query = db.prepare("SELECT id FROM novel_text ORDER BY id")?;
        let ids = query
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        ids.iter().map(|id| Target::new("novel", id)).collect()
    }

    fn remember_text(&self, target: &Target, hash: &str, bytes: u64) -> Result<()> {
        self.db.lock().unwrap().execute(
            "INSERT INTO novel_text VALUES (?1,?2,?3) ON CONFLICT(id) DO UPDATE SET sha256=excluded.sha256,bytes=excluded.bytes",
            params![target.id, hash, bytes],
        )?;
        Ok(())
    }

    pub(super) fn text_matches(&self, target: &Target) -> Result<bool> {
        let path = self.text_path(target)?;
        let Some(expected) = self.text_record(target)? else {
            return Ok(false);
        };
        match fingerprint(&path) {
            Ok(actual) => Ok(actual == expected),
            Err(Error::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error),
        }
    }

    pub fn text_complete(&self, target: &Target) -> Result<bool> {
        let path = self.text_path(target)?;
        if let Some((_, bytes)) = self.text_record(target)? {
            if bytes < self.min_text_bytes {
                return Ok(false);
            }
            return self.text_matches(target);
        }
        if !path.try_exists()? {
            return Ok(false);
        }
        // Existing TXT has no source proof; validate once, then track its exact bytes.
        let text = match read_text(&path) {
            Ok(text) => text,
            Err(Error::Invalid(_)) => return Ok(false),
            Err(Error::Io(error))
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::InvalidData | std::io::ErrorKind::NotFound
                ) =>
            {
                return Ok(false)
            }
            Err(error) => return Err(error),
        };
        let hash = format!("{:x}", Sha256::digest(text.as_bytes()));
        if (text.len() as u64) < self.min_text_bytes {
            return Ok(false);
        }
        self.remember_text(target, &hash, text.len() as u64)?;
        Ok(true)
    }

    fn skip_short_text(&self, target: &Target, bytes: u64) -> Result<String> {
        self.state(
            target,
            "skipped",
            Some(&format!(
                "Novel TXT excluded by minimum size policy: {bytes} < {} bytes",
                self.min_text_bytes
            )),
        )?;
        Ok("skipped".into())
    }

    pub(super) fn prepare_text(&self, target: &Target, client: &dyn Transport) -> Result<String> {
        let path = self.text_path(target)?;
        if let Some((_, bytes)) = self.text_record(target)? {
            if bytes < self.min_text_bytes {
                return self.skip_short_text(target, bytes);
            }
        }
        if self.text_complete(target)? {
            return Ok("already_complete".into());
        }
        if path.try_exists()? {
            let bytes = fs::metadata(&path)?.len();
            if bytes < self.min_text_bytes {
                return self.skip_short_text(target, bytes);
            }
            return Err(invalid("既存の本文TXTが空・不正・変更済みです。上書きせず保持しました。別の場所へ移してから再開してください。"));
        }
        let mut local = None;
        if let Some(manifest) = self.manifest(target).filter(|m| m["key"] == target.key()) {
            for name in ["source.txt", "plain.txt"] {
                if let Some(entry) = manifest["files"]
                    .as_array()
                    .and_then(|files| files.iter().find(|entry| entry["path"] == name))
                {
                    let source = self.path(target, name)?;
                    if fingerprint(&source).is_ok_and(|(hash, bytes)| {
                        entry["sha256"] == hash && entry["bytes"] == bytes
                    }) {
                        local = Some(read_text(&source)?);
                        break;
                    }
                }
            }
        }
        if local.is_none() {
            let source = safe_path(&self.root, &format!("works/novel/{}.txt", target.key()))?;
            if source.try_exists()? {
                local = Some(read_text(&source)?);
            }
        }
        let content = if let Some(local) = local {
            local
        } else {
            let body = client.api(&format!("/ajax/novel/{}", target.id), &[])?;
            if !eligible(&body) {
                self.state(target, "skipped", None)?;
                return Ok("skipped".into());
            }
            if body["id"] != target.id {
                return Err(invalid("Returned work ID does not match request"));
            }
            str_at(&body, "content")?.to_owned()
        };
        let text = novel::body_text(&content);
        if text.trim().is_empty() || text.contains('\0') {
            return Err(invalid("Novel has no usable text body"));
        }
        let bytes = text.len() as u64;
        if bytes < self.min_text_bytes {
            return self.skip_short_text(target, bytes);
        }
        let hash = format!("{:x}", Sha256::digest(text.as_bytes()));
        if self
            .text_record(target)?
            .is_some_and(|expected| expected != (hash.clone(), bytes))
        {
            return Err(invalid(
                "update_needed: remote novel differs from the saved TXT checksum",
            ));
        }
        fs::create_dir_all(path.parent().unwrap())?;
        let mut file = tempfile::NamedTempFile::new_in(path.parent().unwrap())?;
        file.write_all(text.as_bytes())?;
        file.as_file().sync_all()?;
        file.persist_noclobber(&path)
            .map_err(|e| Error::Io(e.error))?;
        if fingerprint(&path)? != (hash.clone(), bytes) {
            return Err(invalid("Post-write TXT checksum verification failed"));
        }
        self.remember_text(target, &hash, bytes)?;
        Ok("complete".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::{Fixture, DEMO};

    #[test]
    fn novel_body_uses_one_api_and_no_media_or_duplicate_files() {
        let temp = tempfile::tempdir().unwrap();
        let mut archive = Archive::open(temp.path(), false).unwrap();
        archive.text_only = true;
        let target = Target::new("novel", "1004").unwrap();
        let client = Fixture::new(DEMO, Arc::default()).unwrap();
        assert_eq!(archive.prepare(&target, &client).unwrap(), "complete");
        assert_eq!(client.requests(), 1);
        let text = fs::read_to_string(archive.text_path(&target).unwrap()).unwrap();
        assert!(text.contains("出発"));
        assert!(!text.contains("[pixivimage:"));
        assert!(!text.contains("[uploadedimage:"));
        assert!(!temp.path().join("works").exists());
        assert_eq!(
            archive.prepare(&target, &client).unwrap(),
            "already_complete"
        );
        assert_eq!(client.requests(), 1);
        assert_eq!(archive.verify().unwrap()["valid"], 1);
        let zip = temp.path().with_extension("zip");
        archive.pack(&zip).unwrap();
        let mut packed = zip::ZipArchive::new(File::open(&zip).unwrap()).unwrap();
        assert!(packed.by_name("novel/novel_1004.txt").is_ok());
        fs::remove_file(zip).unwrap();
    }

    #[test]
    fn existing_flat_txt_is_registered_without_http_and_edits_are_preserved() {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir(temp.path().join("novel")).unwrap();
        let path = temp.path().join("novel/novel_1004.txt");
        fs::write(&path, "既に保存してある本文\n").unwrap();
        let mut archive = Archive::open(temp.path(), false).unwrap();
        archive.text_only = true;
        let target = Target::new("novel", "1004").unwrap();
        let client = Fixture::new(DEMO, Arc::default()).unwrap();
        assert!(archive.complete(&target).unwrap());
        assert_eq!(
            archive.prepare(&target, &client).unwrap(),
            "already_complete"
        );
        assert_eq!(client.requests(), 0);
        drop(archive);
        let mut archive = Archive::open(temp.path(), true).unwrap();
        archive.text_only = true;
        assert!(archive.complete(&target).unwrap());
        fs::write(&path, "ユーザーによる編集").unwrap();
        assert!(!archive.complete(&target).unwrap());
        assert_eq!(
            archive.verify().unwrap()["problems"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert_eq!(archive.prepare(&target, &client).unwrap(), "failed");
        assert_eq!(fs::read_to_string(path).unwrap(), "ユーザーによる編集");
        assert_eq!(client.requests(), 0);
    }

    #[test]
    fn old_archive_and_collection_text_are_reused_without_http() {
        let temp = tempfile::tempdir().unwrap();
        let mut archive = Archive::open(temp.path(), false).unwrap();
        let target = Target::new("novel", "1004").unwrap();
        let client = Fixture::new(DEMO, Arc::default()).unwrap();
        archive.save(&target, &client).unwrap();
        let before = client.requests();
        archive.text_only = true;
        assert_eq!(archive.prepare(&target, &client).unwrap(), "complete");
        assert_eq!(client.requests(), before);
        archive.text_only = false;
        assert!(archive.complete(&target).unwrap());

        let temp = tempfile::tempdir().unwrap();
        fs::create_dir_all(temp.path().join("works/novel")).unwrap();
        fs::write(
            temp.path().join("works/novel/novel_1004.txt"),
            "旧形式の本文",
        )
        .unwrap();
        let mut archive = Archive::open(temp.path(), false).unwrap();
        archive.text_only = true;
        archive.prepare(&target, &client).unwrap();
        assert_eq!(client.requests(), before);
        assert_eq!(
            fs::read_to_string(archive.text_path(&target).unwrap()).unwrap(),
            "旧形式の本文"
        );
    }

    #[test]
    fn invalid_existing_txt_is_not_overwritten_or_redownloaded() {
        for bytes in [b"".as_slice(), b"\xff", b"bad\0text", b" \r\n"] {
            let temp = tempfile::tempdir().unwrap();
            fs::create_dir(temp.path().join("novel")).unwrap();
            let path = temp.path().join("novel/novel_1004.txt");
            fs::write(&path, bytes).unwrap();
            let mut archive = Archive::open(temp.path(), false).unwrap();
            archive.text_only = true;
            let client = Fixture::new(DEMO, Arc::default()).unwrap();
            let target = Target::new("novel", "1004").unwrap();
            assert!(!archive.complete(&target).unwrap());
            assert_eq!(archive.prepare(&target, &client).unwrap(), "failed");
            assert_eq!(fs::read(path).unwrap(), bytes);
            assert_eq!(client.requests(), 0);
        }
    }

    #[test]
    fn deleted_txt_is_repaired_only_when_body_checksum_matches() {
        let temp = tempfile::tempdir().unwrap();
        let mut archive = Archive::open(temp.path(), false).unwrap();
        archive.text_only = true;
        let target = Target::new("novel", "1004").unwrap();
        let client = Fixture::new(DEMO, Arc::default()).unwrap();
        archive.prepare(&target, &client).unwrap();
        let path = archive.text_path(&target).unwrap();
        fs::remove_file(&path).unwrap();
        assert_eq!(archive.prepare(&target, &client).unwrap(), "complete");
        assert_eq!(client.requests(), 2);
        fs::remove_file(&path).unwrap();
        let mut changed: Value = serde_json::from_str(DEMO).unwrap();
        changed["responses"]["/ajax/novel/1004"]["content"] = json!("更新された本文");
        let changed = Fixture::new(&changed.to_string(), Arc::default()).unwrap();
        assert_eq!(archive.prepare(&target, &changed).unwrap(), "failed");
        assert!(!path.exists());
    }
    #[test]
    fn text_only_still_checks_visibility_identity_and_nonempty_content() {
        for (field, value, expected) in [
            ("xRestrict", json!(1), "skipped"),
            ("id", json!("9999"), "failed"),
            ("content", json!(""), "failed"),
        ] {
            let temp = tempfile::tempdir().unwrap();
            let mut archive = Archive::open(temp.path(), false).unwrap();
            archive.text_only = true;
            let mut fixture: Value = serde_json::from_str(DEMO).unwrap();
            fixture["responses"]["/ajax/novel/1004"][field] = value;
            let client = Fixture::new(&fixture.to_string(), Arc::default()).unwrap();
            let target = Target::new("novel", "1004").unwrap();
            assert_eq!(archive.prepare(&target, &client).unwrap(), expected);
            assert_eq!(client.requests(), 1);
            assert!(!archive.text_path(&target).unwrap().exists());
        }
    }
}
