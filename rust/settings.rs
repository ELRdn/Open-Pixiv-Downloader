use crate::{
    archive::atomic,
    error::{invalid, Result},
    transport::{MAX_JOBS, MAX_REQUEST_BUDGET, MIN_REQUEST_INTERVAL},
};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::{Path, PathBuf},
};

#[derive(Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub schema_version: u32,
    pub out: String,
    pub jobs: usize,
    pub request_budget: usize,
    pub balanced: bool,
    pub interval: f64,
    pub api_interval: f64,
    pub text_only: bool,
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            schema_version: 1,
            out: "archive".into(),
            jobs: 2,
            request_budget: 20_000,
            balanced: true,
            interval: 1.0,
            api_interval: 2.0,
            text_only: false,
        }
    }
}
impl Settings {
    pub fn validate(&self) -> Result<()> {
        if self.schema_version != 1
            || self.out.trim().is_empty()
            || self.out.contains('\0')
            || !(1..=MAX_JOBS).contains(&self.jobs)
            || self.request_budget > MAX_REQUEST_BUDGET
            || ![self.interval, self.api_interval]
                .iter()
                .all(|n| n.is_finite() && (MIN_REQUEST_INTERVAL..=3600.0).contains(n))
        {
            return Err(invalid(
                "設定が不正です（並列1〜32、要求予算0〜1000000・0は無制限、間隔0.001〜3600秒）。",
            ));
        }
        Ok(())
    }
    pub fn load(path: &Path) -> Result<Self> {
        if fs::metadata(path)?.len() > 32 * 1024 {
            return Err(invalid("Settings exceed 32 KiB"));
        }
        let settings: Self = serde_json::from_slice(&fs::read(path)?)?;
        settings.validate()?;
        Ok(settings)
    }
    pub fn save(&self, path: &Path) -> Result<()> {
        self.validate()?;
        atomic(path, &serde_json::to_vec_pretty(self)?)
    }
}
pub fn path() -> Result<PathBuf> {
    #[cfg(windows)]
    let base = std::env::var_os("APPDATA").map(PathBuf::from);
    #[cfg(not(windows))]
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")));
    base.map(|base| base.join("opixiv/settings.json"))
        .ok_or_else(|| invalid("設定フォルダーが見つかりません。"))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn settings_round_trip_and_invalid_values() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("opixiv/settings.json");
        let mut settings = Settings {
            jobs: MAX_JOBS,
            interval: MIN_REQUEST_INTERVAL,
            api_interval: 0.01,
            text_only: true,
            request_budget: MAX_REQUEST_BUDGET,
            ..Default::default()
        };
        settings.save(&path).unwrap();
        let loaded = Settings::load(&path).unwrap();
        assert_eq!(loaded.jobs, MAX_JOBS);
        assert_eq!(loaded.interval, MIN_REQUEST_INTERVAL);
        assert_eq!(loaded.api_interval, 0.01);
        assert!(loaded.text_only);
        assert_eq!(loaded.request_budget, MAX_REQUEST_BUDGET);
        settings.request_budget += 1;
        assert!(settings.save(&path).is_err());
        assert_eq!(
            Settings::load(&path).unwrap().request_budget,
            MAX_REQUEST_BUDGET
        );
        settings.request_budget = 0;
        settings.save(&path).unwrap();
        assert_eq!(Settings::load(&path).unwrap().request_budget, 0);
        settings.jobs = MAX_JOBS + 1;
        assert!(settings.save(&path).is_err());
        settings.jobs = MAX_JOBS;
        settings.interval = MIN_REQUEST_INTERVAL / 10.0;
        assert!(settings.save(&path).is_err());
        fs::write(&path, b"{broken").unwrap();
        assert!(Settings::load(&path).is_err());
    }
}
