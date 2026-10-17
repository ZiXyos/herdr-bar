//! Helpers shared by the unit tests.
//!
//! The fixture data in `tests/data/fixtures.json` is the session the Python
//! release's own tests used (their `tests/fixtures.py`), exported once so the
//! Rust tests check the same rows.
#![allow(dead_code)]

use crate::pyjson::{self, Value};
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, MutexGuard};

static ENV: Mutex<()> = Mutex::new(());
static COUNTER: AtomicUsize = AtomicUsize::new(0);

/// Hold this for the whole of a test that reads or changes environment
/// variables, so tests that run in parallel do not see each other's values.
pub fn env_lock() -> MutexGuard<'static, ()> {
    ENV.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Sets environment variables and puts the old values back on drop.
/// Hold `env_lock()` for as long as this lives.
#[derive(Default)]
pub struct EnvVars {
    saved: Vec<(String, Option<OsString>)>,
}

impl EnvVars {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set(&mut self, key: &str, value: impl AsRef<OsStr>) -> &mut Self {
        self.save(key);
        std::env::set_var(key, value);
        self
    }

    pub fn remove(&mut self, key: &str) -> &mut Self {
        self.save(key);
        std::env::remove_var(key);
        self
    }

    fn save(&mut self, key: &str) {
        if !self.saved.iter().any(|(k, _)| k == key) {
            self.saved.push((key.to_string(), std::env::var_os(key)));
        }
    }
}

impl Drop for EnvVars {
    fn drop(&mut self) {
        for (key, value) in self.saved.drain(..).rev() {
            match value {
                Some(value) => std::env::set_var(&key, value),
                None => std::env::remove_var(&key),
            }
        }
    }
}

/// A fresh directory under the system temp directory, removed on drop.
pub struct TempDir(PathBuf);

impl TempDir {
    pub fn new() -> Self {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("hb-test-{}-{}", std::process::id(), n));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("create temp dir");
        TempDir(path)
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Default for TempDir {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A short, unused Unix socket path. macOS limits socket paths to about 104
/// bytes, and its temp directory alone uses half of that.
#[cfg(unix)]
pub fn socket_path() -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let path = PathBuf::from(format!("/tmp/hb-{}-{}.sock", std::process::id(), n));
    let _ = std::fs::remove_file(&path);
    path
}

fn fixtures() -> Value {
    pyjson::loads(include_str!("../tests/data/fixtures.json")).expect("fixtures.json parses")
}

/// `tests/fixtures.py`'s `snapshot()`: a fresh copy on every call.
pub fn snapshot() -> Value {
    fixtures().get("snapshot").clone()
}

/// `tests/fixtures.py`'s `PANE_AGES`: pane id -> seconds.
pub fn pane_ages() -> Value {
    fixtures().get("pane_ages").clone()
}

/// `tests/fixtures.py`'s `PREVIEW_TEXT`.
pub fn preview_text() -> String {
    fixtures().get("preview_text").as_str().unwrap_or("").to_string()
}
