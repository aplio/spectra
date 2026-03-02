use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::xdg;

#[derive(Debug, Clone)]
pub struct DataStore {
    base_dir: PathBuf,
}

#[derive(Debug, Clone, Serialize)]
pub struct SessionInfo {
    pub session_id: String,
    pub session_name: String,
    pub pid: u32,
    pub started_unix: u64,
    pub pane_count: usize,
    pub window_count: usize,
    pub focused_pane_id: Option<usize>,
}

impl DataStore {
    /// Compatibility constructor for spectra.
    pub fn from_xdg() -> io::Result<Self> {
        let base_dir = xdg::app_data_dir();
        fs::create_dir_all(&base_dir)?;
        Ok(Self { base_dir })
    }

    pub fn from_base_dir_for_tests(base_dir: PathBuf) -> Self {
        Self { base_dir }
    }

    pub fn base_dir(&self) -> &Path {
        &self.base_dir
    }

    pub fn normalize_session_id(name: &str) -> String {
        let mut out = String::new();
        for ch in name.chars() {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                out.push(ch.to_ascii_lowercase());
            } else {
                out.push('_');
            }
        }
        let out = out.trim_matches('_');
        if out.is_empty() {
            "session".to_string()
        } else {
            out.to_string()
        }
    }

    pub fn session_dir(&self, session_id: &str) -> PathBuf {
        self.base_dir.join("sessions").join(session_id)
    }

    fn ensure_session_dir(&self, session_id: &str) -> io::Result<PathBuf> {
        let dir = self.session_dir(session_id);
        fs::create_dir_all(&dir)?;
        Ok(dir)
    }

    pub fn write_session_info(&self, info: &SessionInfo) -> io::Result<PathBuf> {
        let dir = self.ensure_session_dir(&info.session_id)?;
        let path = dir.join("session-info.json");
        self.write_json_pretty(&path, info)?;
        Ok(path)
    }

    pub fn append_log_line(&self, session_id: &str, line: &str) -> io::Result<PathBuf> {
        let dir = self.ensure_session_dir(session_id)?;
        let path = dir.join("session.log");
        let mut file = OpenOptions::new().create(true).append(true).open(&path)?;
        file.write_all(line.as_bytes())?;
        file.write_all(b"\n")?;
        Ok(path)
    }

    pub fn write_scrollback(
        &self,
        session_id: &str,
        pane_id: usize,
        scrollback: &str,
    ) -> io::Result<PathBuf> {
        let dir = self.ensure_session_dir(session_id)?.join("scrollback");
        fs::create_dir_all(&dir)?;
        let path = dir.join(format!("pane-{pane_id}-{}.txt", unix_time_now()));
        fs::write(&path, scrollback)?;
        Ok(path)
    }

    pub fn write_layout<T: Serialize>(&self, session_id: &str, layout: &T) -> io::Result<PathBuf> {
        let dir = self.ensure_session_dir(session_id)?.join("layouts");
        fs::create_dir_all(&dir)?;
        let timestamp = unix_time_now();
        let path = dir.join(format!("layout-{timestamp}.json"));
        self.write_json_pretty(&path, layout)?;

        let latest = dir.join("latest-layout.json");
        self.write_json_pretty(&latest, layout)?;

        Ok(path)
    }

    pub fn runtime_state_path(&self) -> PathBuf {
        self.base_dir.join("runtime-state.json")
    }

    pub fn write_runtime_state<T: Serialize>(&self, state: &T) -> io::Result<PathBuf> {
        let path = self.runtime_state_path();
        self.write_json_pretty(&path, state)?;
        Ok(path)
    }

    pub fn read_runtime_state<T: DeserializeOwned>(&self) -> io::Result<Option<T>> {
        let path = self.runtime_state_path();
        let content = match fs::read_to_string(&path) {
            Ok(content) => content,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(err),
        };
        let parsed = serde_json::from_str::<T>(&content).map_err(|err| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("parse runtime state {}: {err}", path.display()),
            )
        })?;
        Ok(Some(parsed))
    }

    fn write_json_pretty<T: Serialize>(&self, path: &Path, value: &T) -> io::Result<()> {
        let json = serde_json::to_vec_pretty(value)
            .map_err(|err| io::Error::other(format!("serialize json: {err}")))?;
        fs::write(path, json)
    }
}

pub fn unix_time_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::{DataStore, SessionInfo, unix_time_now};

    #[test]
    fn normalize_session_id_sanitizes() {
        assert_eq!(
            DataStore::normalize_session_id("Dev Session"),
            "dev_session"
        );
        assert_eq!(DataStore::normalize_session_id("$"), "session");
    }

    #[test]
    fn writes_session_artifacts() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = DataStore {
            base_dir: dir.path().to_path_buf(),
        };

        let info = SessionInfo {
            session_id: "dev".to_string(),
            session_name: "Dev".to_string(),
            pid: 7,
            started_unix: unix_time_now(),
            pane_count: 1,
            window_count: 1,
            focused_pane_id: Some(1),
        };

        let info_path = store.write_session_info(&info).expect("write info");
        assert!(info_path.exists());

        let log_path = store
            .append_log_line("dev", "hello")
            .expect("append log line");
        assert!(log_path.exists());
        assert!(
            fs::read_to_string(&log_path)
                .expect("read log")
                .contains("hello")
        );

        let scrollback_path = store
            .write_scrollback("dev", 1, "line")
            .expect("write scrollback");
        assert!(scrollback_path.exists());

        let layout_path = store
            .write_layout("dev", &serde_json::json!({"a": 1}))
            .expect("write layout");
        assert!(layout_path.exists());
        assert!(
            store
                .base_dir()
                .join("sessions")
                .join("dev")
                .join("layouts")
                .join("latest-layout.json")
                .exists()
        );

        let runtime_path = store
            .write_runtime_state(&serde_json::json!({"version": 1, "sessions": []}))
            .expect("write runtime state");
        assert!(runtime_path.exists());
        let runtime: serde_json::Value = store
            .read_runtime_state()
            .expect("read runtime state")
            .expect("runtime state should exist");
        assert_eq!(runtime["version"], serde_json::json!(1));
    }
}
