//! Recently visited rows, persisted under the plugin state directory.
//!
//! Ordering an empty bar by "where you have been" is what makes the first
//! Enter useful: the top row is the tab you came from, so open-and-Enter
//! behaves like alt-tab.

use crate::pyjson::{self, Map, Value};
use std::io::Write;
use std::path::{Path, PathBuf};

pub const MAX_ENTRIES: usize = 120;

#[derive(Clone, Debug)]
pub struct Entry {
    pub key: String,
    pub title: String,
    /// Kept as read: an int, a float, or even a bool, as Python allowed.
    pub ts: Value,
}

pub struct Recents {
    pub path: Option<PathBuf>,
    pub entries: Vec<Entry>,
}

impl Recents {
    pub fn new(path: Option<PathBuf>) -> Recents {
        let mut recents = Recents { path, entries: Vec::new() };
        recents.load();
        recents
    }

    fn load(&mut self) {
        let Some(path) = &self.path else { return };
        let Ok(data) = std::fs::read(path) else { return };
        let Ok(text) = String::from_utf8(data) else { return };
        let Ok(data) = pyjson::loads(&text) else { return };
        let Value::List(entries) = data.get("entries") else { return };
        let mut cleaned = Vec::new();
        for entry in entries {
            let Value::Dict(entry) = entry else { continue };
            let Some(Value::Str(key)) = entry.get("key") else { continue };
            if key.is_empty() {
                continue;
            }
            let title = match entry.get("title") {
                Some(Value::Str(title)) => title.clone(),
                _ => String::new(),
            };
            let ts = match entry.get("ts") {
                Some(value @ (Value::Int(_) | Value::BigInt(_) | Value::Float(_) | Value::Bool(_))) => value.clone(),
                _ => Value::Int(0),
            };
            cleaned.push(Entry { key: key.clone(), title, ts });
        }
        cleaned.truncate(MAX_ENTRIES);
        self.entries = cleaned;
    }

    /// Position in the recent list, 0 being most recent.
    pub fn rank(&self, key: &str, title: &str) -> Option<usize> {
        if let Some(index) = self.entries.iter().position(|e| e.key == key) {
            return Some(index);
        }
        if !title.is_empty() {
            return self.entries.iter().position(|e| e.title == title);
        }
        None
    }

    pub fn touch(&mut self, key: &str, title: &str) {
        self.entries.retain(|e| e.key != key);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i128)
            .unwrap_or(0);
        self.entries.insert(0, Entry { key: key.to_string(), title: title.to_string(), ts: Value::Int(now) });
        self.entries.truncate(MAX_ENTRIES);
    }

    pub fn to_json(&self) -> Value {
        let entries = self
            .entries
            .iter()
            .map(|e| {
                let mut map = Map::new();
                map.insert("key", Value::str(e.key.clone()));
                map.insert("title", Value::str(e.title.clone()));
                map.insert("ts", e.ts.clone());
                Value::Dict(map)
            })
            .collect();
        let mut map = Map::new();
        map.insert("version", Value::Int(1));
        map.insert("entries", Value::List(entries));
        Value::Dict(map)
    }

    pub fn save(&self) {
        let Some(path) = &self.path else { return };
        // Losing the recent list is not worth failing a jump over.
        let _ = write_atomic(path, ".recent-", ".json", pyjson::dumps(&self.to_json(), false).as_bytes());
    }
}

/// tempfile.NamedTemporaryFile next to `path`, then os.replace over it.
pub fn write_atomic(path: &Path, prefix: &str, suffix: &str, data: &[u8]) -> std::io::Result<()> {
    let directory = path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
    std::fs::create_dir_all(directory)?;
    let (temp_path, mut file) = create_temp(directory, prefix, suffix)?;
    let result = (|| {
        file.write_all(data)?;
        file.flush()?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&temp_path, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp_path);
    }
    result
}

fn create_temp(directory: &Path, prefix: &str, suffix: &str) -> std::io::Result<(PathBuf, std::fs::File)> {
    const LETTERS: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789_";
    let mut seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
        ^ (std::process::id() as u64).rotate_left(32);
    for _ in 0..100 {
        let mut name = String::from(prefix);
        for _ in 0..8 {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            name.push(LETTERS[(seed % LETTERS.len() as u64) as usize] as char);
        }
        name.push_str(suffix);
        let candidate = directory.join(name);
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(&candidate) {
            Ok(file) => return Ok((candidate, file)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(std::io::Error::new(std::io::ErrorKind::AlreadyExists, "no usable temporary file name"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;

    fn state_path(dir: &TempDir) -> PathBuf {
        dir.path().join("state").join("recent.json")
    }

    #[test]
    fn touch_orders_most_recent_first() {
        let mut recents = Recents::new(None);
        recents.touch("a", "");
        recents.touch("b", "");
        assert_eq!(recents.rank("b", ""), Some(0));
        assert_eq!(recents.rank("a", ""), Some(1));
    }

    #[test]
    fn touching_again_moves_to_the_front_without_duplicating() {
        let mut recents = Recents::new(None);
        recents.touch("a", "");
        recents.touch("b", "");
        recents.touch("a", "");
        assert_eq!(recents.rank("a", ""), Some(0));
        assert_eq!(recents.entries.len(), 2);
    }

    #[test]
    fn unknown_keys_have_no_rank() {
        assert_eq!(Recents::new(None).rank("nope", ""), None);
    }

    #[test]
    fn titles_match_when_ids_change_between_sessions() {
        let mut recents = Recents::new(None);
        recents.touch("w1:p4", "cards meaningless");
        assert_eq!(recents.rank("w9:p9", "cards meaningless"), Some(0));
    }

    #[test]
    fn list_is_capped() {
        let mut recents = Recents::new(None);
        for index in 0..MAX_ENTRIES + 25 {
            recents.touch(&format!("key-{}", index), "");
        }
        assert_eq!(recents.entries.len(), MAX_ENTRIES);
    }

    #[test]
    fn round_trip_through_disk() {
        let dir = TempDir::new();
        let mut recents = Recents::new(Some(state_path(&dir)));
        recents.touch("a", "Agent A");
        recents.save();
        assert_eq!(Recents::new(Some(state_path(&dir))).rank("a", ""), Some(0));
    }

    #[test]
    fn save_is_atomic_and_creates_the_directory() {
        let dir = TempDir::new();
        let path = state_path(&dir);
        let mut recents = Recents::new(Some(path.clone()));
        recents.touch("a", "");
        recents.save();
        let text = std::fs::read_to_string(&path).expect("state file written");
        assert_eq!(pyjson::loads(&text).unwrap().get("version"), &Value::Int(1));
        // No temporary file is left next to it.
        let names: Vec<_> = std::fs::read_dir(path.parent().unwrap()).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(names, vec![std::ffi::OsString::from("recent.json")]);
    }

    #[test]
    fn corrupt_state_is_ignored() {
        let dir = TempDir::new();
        let path = state_path(&dir);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "{not json").unwrap();
        assert!(Recents::new(Some(path)).entries.is_empty());
    }

    #[test]
    fn junk_entries_are_dropped() {
        let dir = TempDir::new();
        let path = state_path(&dir);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, r#"{"entries": [{"key": "ok"}, {"nope": 1}, "string", 5]}"#).unwrap();
        let recents = Recents::new(Some(path));
        let keys: Vec<&str> = recents.entries.iter().map(|e| e.key.as_str()).collect();
        assert_eq!(keys, ["ok"]);
    }

    #[test]
    fn saving_without_a_path_is_a_no_op() {
        let mut recents = Recents::new(None);
        recents.touch("a", "");
        recents.save();
    }

    #[test]
    fn unwritable_location_does_not_raise() {
        // A regular file where the state directory should be: unwritable on
        // every platform, even for root.
        let dir = TempDir::new();
        let blocker = dir.path().join("not-a-dir");
        std::fs::write(&blocker, "").unwrap();
        let path = blocker.join("state").join("recent.json");
        let mut recents = Recents::new(Some(path.clone()));
        recents.touch("a", "");
        recents.save();
        assert!(!path.exists());
    }
}
