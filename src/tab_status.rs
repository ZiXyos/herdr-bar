//! Lead agent tab names with the vendor logo and a live status glyph.
//!
//! Herdr tab labels are plain text, so the status lives in the label itself:
//! `<logo>  <glyph> <name>`. Glyphs are static: a tab is renamed only when its
//! agent's status or its name changes, never on a timer. Only tabs recorded
//! here are ever stripped, and externally changed labels are yielded for the
//! rest of the tab's lifetime, including across watcher restarts. A record is
//! written before its tab's first decorated label and dropped only after the
//! bare name is back or the tab closes.
//!
//! Herdr numbers unnamed tabs by position, and a rename cannot hand that
//! back. Decorated tabs that started unnamed therefore keep following their
//! position for as long as the watcher runs.

use crate::client::{HerdrError, Result};
use crate::config::Config;
use crate::icons;
use crate::pyjson::{self, py_eq, Map, Value, NULL};
use crate::renaming_guard::AutomaticTabClient;
use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Duration, Instant};

/// A snapshot several holders see and change, as Python shared its dicts.
pub type Shared = Rc<RefCell<Value>>;

pub fn shared(value: Value) -> Shared {
    Rc::new(RefCell::new(value))
}

pub const GLYPHS: &[(&str, char)] =
    &[("blocked", '◉'), ("working", '◐'), ("done", '●'), ("idle", '✓'), ("unknown", '○')];
const FONT_RECHECK: Duration = Duration::from_secs(60);
/// Spinner frames and the blink's blank cell from earlier local builds.
const RETIRED_GLYPHS: &str = "⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏ ";

fn rank(status: &str) -> Option<u8> {
    Some(match status {
        "blocked" => 0,
        "done" => 1,
        "working" => 2,
        "idle" => 3,
        "unknown" => 4,
        _ => return None,
    })
}

fn glyph(status: &str) -> char {
    GLYPHS.iter().find(|(name, _)| *name == status).map(|(_, g)| *g).unwrap_or('○')
}

fn is_glyph(c: char) -> bool {
    GLYPHS.iter().any(|(_, g)| *g == c) || RETIRED_GLYPHS.contains(c)
}

/// `^(?:[LOGOS]  )?[GLYPHS] ` removed once.
pub fn strip(label: &str) -> String {
    let chars: Vec<char> = label.chars().take(5).collect();
    let skip = if chars.len() >= 5
        && icons::is_logo(chars[0])
        && chars[1] == ' '
        && chars[2] == ' '
        && is_glyph(chars[3])
        && chars[4] == ' '
    {
        5
    } else if chars.len() >= 2 && is_glyph(chars[0]) && chars[1] == ' ' {
        2
    } else {
        0
    };
    label.chars().skip(skip).collect()
}

pub fn compose(base: &str, status: &str, logo: Option<char>) -> String {
    let glyph = glyph(status);
    // The icon font draws into the next cell, as in the bar's rows.
    match logo {
        Some(logo) => format!("{}  {} {}", logo, glyph, base),
        None => format!("{} {}", glyph, base),
    }
}

/// tab id -> (most urgent agent status, that agent's kind).
pub fn tab_states(snapshot: &Value) -> HashMap<String, (String, String)> {
    let mut states: HashMap<String, (String, String)> = HashMap::new();
    for agent in snapshot.get("agents").list_or_empty() {
        let Some(tab_id) = agent.get("tab_id").as_str() else { continue };
        let status = agent.get("agent_status").as_str().filter(|s| rank(s).is_some()).unwrap_or("unknown");
        let replace = match states.get(tab_id) {
            None => true,
            Some(current) => rank(status) < rank(&current.0),
        };
        if replace {
            let kind = agent.get("agent");
            let kind = if kind.truthy() { pyjson::py_str(kind) } else { String::new() };
            states.insert(tab_id.to_string(), (status.to_string(), kind));
        }
    }
    states
}

fn dict(entries: Vec<(&str, Value)>) -> Value {
    let mut map = Map::new();
    for (key, value) in entries {
        map.insert(key, value);
    }
    Value::Dict(map)
}

fn get<'a>(map: &'a Map, key: &str) -> &'a Value {
    map.get(key).unwrap_or(&NULL)
}

/// Present tabs to auto titles by their bare names; write them decorated.
///
/// Stands in for the client in `AutoTitles::apply`: `snapshot` hands out
/// names without the prefix and `rename_tab` adds it, so a new title and
/// its status reach Herdr in one rename.
pub struct TabStatus {
    pub client: AutomaticTabClient,
    pub path: Option<PathBuf>,
    pub enabled: bool,
    logos: bool,
    font_checked: Option<Instant>,
    pub records: Map,
    states: HashMap<String, (String, String)>,
    latest: Shared,
}

impl TabStatus {
    pub fn new(client: AutomaticTabClient, state_dir: Option<PathBuf>) -> TabStatus {
        let path = state_dir.map(|d| d.join("tab-status.json"));
        let records = TabStatus::load(path.as_ref());
        TabStatus {
            client,
            enabled: path.is_some(),
            path,
            logos: false,
            font_checked: None,
            records,
            states: HashMap::new(),
            latest: shared(Value::Dict(Map::new())),
        }
    }

    pub fn configure(&mut self, config: &Config) {
        self.enabled = config.tab_status && self.path.is_some();
        // The watcher outlives font installs; the popup's once-per-run cache does not.
        let due = self.font_checked.map(|at| at.elapsed() >= FONT_RECHECK).unwrap_or(true);
        if due {
            icons::clear_font_cache();
            self.font_checked = Some(Instant::now());
        }
        self.logos = icons::enabled(&config.agent_icons);
    }

    // -- records ---------------------------------------------------------------

    pub fn load(path: Option<&PathBuf>) -> Map {
        let Some(path) = path else { return Map::new() };
        let Ok(data) = std::fs::read(path) else { return Map::new() };
        let Ok(text) = String::from_utf8(data) else { return Map::new() };
        let Ok(Value::Dict(saved)) = pyjson::loads(&text) else { return Map::new() };
        let mut records = Map::new();
        for (tab_id, record) in saved.iter() {
            let Value::Dict(record) = record else { continue };
            if !matches!(record.get("base"), Some(Value::Str(_))) {
                continue;
            }
            let mut record = record.clone();
            let default = record.get("default").map(|v| v.is_true()).unwrap_or(false);
            record.insert("default", Value::Bool(default));
            records.insert(tab_id.clone(), Value::Dict(record));
        }
        records
    }

    fn save(&mut self, records: Map) -> Result<()> {
        if records == self.records {
            return Ok(());
        }
        let Some(path) = &self.path else {
            return Err(HerdrError::os(&std::io::Error::other("no plugin state directory")));
        };
        let staging = path.with_extension("tmp");
        std::fs::write(&staging, pyjson::dumps(&Value::Dict(records.clone()), true))?;
        std::fs::rename(&staging, path)?;
        self.records = records;
        Ok(())
    }

    fn with_record(&self, tab_id: &str, record: Value) -> Map {
        let mut records = self.records.clone();
        records.insert(tab_id, record);
        records
    }

    // -- client stand-in -------------------------------------------------------

    pub fn snapshot(&mut self) -> Result<Shared> {
        let Value::Dict(mut snapshot) = self.client.snapshot()? else {
            return Err(HerdrError::new("session snapshot missing from response"));
        };
        // Copies: bare names must never leak back into the client's own records.
        let tabs = match snapshot.get("tabs") {
            None => Vec::new(),
            Some(Value::List(tabs)) => tabs.clone(),
            Some(_) => return Err(HerdrError::new("session snapshot tabs are not a list")),
        };
        let mut out_tabs: Vec<Value> = Vec::with_capacity(tabs.len());
        let mut positions: Vec<(Value, i64)> = Vec::new();
        for tab in tabs {
            let Value::Dict(mut tab) = tab else {
                return Err(HerdrError::new("session snapshot tab is not an object"));
            };
            let workspace = tab.get("workspace_id").cloned().unwrap_or_else(|| Value::str(""));
            let position = match positions.iter_mut().find(|(k, _)| py_eq(k, &workspace)) {
                Some(slot) => {
                    slot.1 += 1;
                    slot.1
                }
                None => {
                    positions.push((workspace, 1));
                    1
                }
            };
            let Some(Value::Str(label)) = tab.get("label").cloned() else {
                out_tabs.push(Value::Dict(tab));
                continue;
            };
            tab.insert("shown", Value::str(label.clone()));
            tab.insert("position", Value::str(position.to_string()));
            let tab_id = tab.get("tab_id").and_then(|v| v.as_str()).map(|s| s.to_string());
            let record = tab_id.as_deref().and_then(|id| self.records.get(id)).cloned();
            if let (Some(tab_id), Some(Value::Dict(mut record))) = (tab_id, record) {
                let label_value = Value::str(label.clone());
                if get(&record, "yielded").truthy() {
                    tab.insert("title_conflict", Value::Bool(true));
                    out_tabs.push(Value::Dict(tab));
                    continue;
                }
                let written = get(&record, "written").clone();
                let base = get(&record, "base").clone();
                if written.is_null() {
                    // Older releases recorded only the base. Adopt a prefix
                    // only when removing it gives that exact recorded base.
                    if py_eq(&label_value, &base) || py_eq(&Value::str(strip(&label)), &base) {
                        record.insert("written", label_value.clone());
                        let records = self.with_record(&tab_id, Value::Dict(record.clone()));
                        self.save(records)?;
                    } else {
                        self.yield_tab(&tab_id, label_value)?;
                        tab.insert("title_conflict", Value::Bool(true));
                        out_tabs.push(Value::Dict(tab));
                        continue;
                    }
                } else if !py_eq(&label_value, &written) && !py_eq(&label_value, get(&record, "previous")) {
                    self.yield_tab(&tab_id, label_value)?;
                    tab.insert("title_conflict", Value::Bool(true));
                    out_tabs.push(Value::Dict(tab));
                    continue;
                }
                let (base, default) = if py_eq(&label_value, get(&record, "previous")) {
                    // A refused or interrupted write still shows the old label.
                    let Some(base) = record.get("previous_base").cloned() else {
                        return Err(HerdrError::new("tab status record is missing previous_base"));
                    };
                    (base, record.get("previous_default").cloned().unwrap_or(Value::Bool(false)))
                } else {
                    let Some(base) = record.get("base").cloned() else {
                        return Err(HerdrError::new("tab status record is missing base"));
                    };
                    (base, record.get("default").cloned().unwrap_or(Value::Bool(false)))
                };
                let shown = if default.truthy() { Value::str(position.to_string()) } else { base };
                tab.insert("default", default);
                tab.insert("label", shown);
            }
            out_tabs.push(Value::Dict(tab));
        }
        snapshot.insert("tabs", Value::List(out_tabs));
        let snapshot = Value::Dict(snapshot);
        self.states = tab_states(&snapshot);
        self.latest = shared(snapshot);
        Ok(self.latest.clone())
    }

    pub fn rename_tab(&mut self, tab_id: &str, label: &str) -> Result<()> {
        let latest = self.latest.clone();
        let index = {
            let latest = latest.borrow();
            latest.get("tabs").list_or_empty().iter().position(|tab| py_eq(tab.get("tab_id"), &Value::str(tab_id)))
        };
        let desired = Value::str(self.desired(tab_id, label));
        match index {
            Some(index) => self.sync_at(&latest, index, &Value::str(label), &desired),
            None => {
                let mut tab = Map::new();
                tab.insert("tab_id", Value::str(tab_id));
                self.sync(&mut tab, &Value::str(label), &desired)
            }
        }
    }

    // -- decoration ------------------------------------------------------------

    fn desired(&self, tab_id: &str, base: &str) -> String {
        let state = self.states.get(tab_id);
        match state {
            Some((status, kind)) if self.enabled && !base.is_empty() => {
                let logo = if self.logos { icons::logo_for(Some(kind)) } else { None };
                compose(base, status, logo)
            }
            _ => base.to_string(),
        }
    }

    /// Remember another writer took over, including across watcher restarts.
    fn yield_tab(&mut self, tab_id: &str, label: Value) -> Result<()> {
        let record = dict(vec![("base", label), ("default", Value::Bool(false)), ("yielded", Value::Bool(true))]);
        let records = self.with_record(tab_id, record);
        self.save(records)
    }

    /// Sync the tab at `index` of a shared snapshot, writing back what it shows.
    fn sync_at(&mut self, snapshot: &Shared, index: usize, base: &Value, label: &Value) -> Result<()> {
        let mut tab = match snapshot.borrow().get("tabs").list_or_empty().get(index) {
            Some(Value::Dict(tab)) => tab.clone(),
            _ => return Ok(()),
        };
        let result = self.sync(&mut tab, base, label);
        if let Some(shown) = tab.get("shown").cloned() {
            if let Some(Value::List(tabs)) = snapshot.borrow_mut().as_dict_mut().and_then(|s| s.get_mut("tabs")) {
                if let Some(Value::Dict(slot)) = tabs.get_mut(index) {
                    slot.insert("shown", shown);
                }
            }
        }
        result
    }

    /// Rename only a label we still own, recording pending writes first.
    fn sync(&mut self, tab: &mut Map, base: &Value, label: &Value) -> Result<()> {
        let tab_id = pyjson::py_str(get(tab, "tab_id"));
        let yielded = match self.records.get(&tab_id) {
            Some(record) => record.get("yielded").truthy(),
            None => false,
        };
        if get(tab, "title_conflict").truthy() || yielded {
            return Err(HerdrError::new("tab name changed by another writer"));
        }
        let changing = !py_eq(label, get(tab, "shown"));
        if changing {
            let current = self.client.client.snapshot()?;
            let current = current
                .get("tabs")
                .list_or_empty()
                .iter()
                .find(|item| py_eq(item.get("tab_id"), &Value::str(tab_id.clone())))
                .cloned();
            let Some(current) = current else {
                return Err(HerdrError::new("tab closed before rename"));
            };
            if !py_eq(current.get("label"), get(tab, "shown")) {
                let label = current.get("label");
                let label = if label.truthy() { label.clone() } else { Value::str("") };
                self.yield_tab(&tab_id, label)?;
                return Err(HerdrError::new("tab name changed before rename"));
            }
        }
        let mut records = self.records.clone();
        let known = self.records.contains_key(&tab_id);
        // `base == position and (tab.get("default", False) or tab_id not in records)`
        let default = if !py_eq(base, get(tab, "position")) {
            Value::Bool(false)
        } else {
            let current = tab.get("default").cloned().unwrap_or(Value::Bool(false));
            if current.truthy() {
                current
            } else {
                Value::Bool(!known)
            }
        };
        if !py_eq(label, base) || (known && default.truthy()) {
            let mut record = Map::new();
            record.insert("base", base.clone());
            record.insert("default", default.clone());
            record.insert("written", label.clone());
            if changing {
                record.insert("previous", get(tab, "shown").clone());
                record.insert("previous_base", tab.get("label").cloned().unwrap_or_else(|| base.clone()));
                record.insert("previous_default", tab.get("default").cloned().unwrap_or_else(|| default.clone()));
            }
            records.insert(tab_id.clone(), Value::Dict(record));
            // If ownership cannot be saved, do not risk an untracked rename.
            self.save(records.clone())?;
        } else {
            records.remove(&tab_id);
        }
        if changing {
            let text = match label {
                Value::Str(text) => text.clone(),
                other => pyjson::py_str(other),
            };
            self.client.rename_tab(&tab_id, &text)?;
            tab.insert("shown", label.clone());
        }
        if records.contains_key(&tab_id) {
            // Successful writes must not accept a later reversion to the old
            // name as a failed write; that is another writer taking over.
            let record = dict(vec![("base", base.clone()), ("default", default), ("written", label.clone())]);
            records.insert(tab_id.clone(), record);
            self.save(records)?;
        } else {
            self.forget(&[tab_id]);
        }
        Ok(())
    }

    /// Bring every label up to date; a label already right costs no rename.
    pub fn apply(&mut self, snapshot: &Shared) {
        let tabs: Vec<Value> = snapshot.borrow().get("tabs").list_or_empty().to_vec();
        let mut live: Vec<String> = Vec::new();
        for (index, tab) in tabs.iter().enumerate() {
            let (Some(tab_id), Some(base)) = (tab.get("tab_id").as_str(), tab.get("label").as_str()) else { continue };
            live.push(tab_id.to_string());
            if tab.get("title_conflict").truthy() {
                continue;
            }
            let label = self.desired(tab_id, base);
            let shown = tab.as_dict().and_then(|t| t.get("shown")).cloned().unwrap_or_else(|| Value::str(base));
            if py_eq(&Value::str(label.clone()), &shown) && !self.records.contains_key(tab_id) {
                continue;
            }
            let _ = self.sync_at(snapshot, index, &Value::str(base), &Value::str(label));
        }
        let gone: Vec<String> = self.records.keys().filter(|k| !live.contains(k)).cloned().collect();
        self.forget(&gone);
    }

    fn forget(&mut self, tab_ids: &[String]) {
        if tab_ids.iter().any(|id| self.records.contains_key(id)) {
            let mut records = self.records.clone();
            records.retain(|key, _| !tab_ids.iter().any(|id| id == key));
            // A stale record only costs one extra strip later.
            let _ = self.save(records);
        }
    }

    /// Restore every recorded tab to its bare name before the watcher exits.
    pub fn clear(&mut self) {
        if self.records.is_empty() {
            return;
        }
        let Ok(snapshot) = self.snapshot() else { return };
        let tabs: Vec<Value> = snapshot.borrow().get("tabs").list_or_empty().to_vec();
        let mut present: Vec<Value> = Vec::new();
        for (index, tab) in tabs.iter().enumerate() {
            present.push(tab.get("tab_id").clone());
            let Some(tab_id) = tab.get("tab_id").as_str() else { continue };
            if !self.records.contains_key(tab_id) || tab.get("title_conflict").truthy() {
                continue;
            }
            let Some(label) = tab.as_dict().and_then(|t| t.get("label")).cloned() else { continue };
            if self.sync_at(&snapshot, index, &label, &label).is_err() {
                continue;
            }
            self.forget(&[tab_id.to_string()]);
        }
        let gone: Vec<String> = self
            .records
            .keys()
            .filter(|k| !present.iter().any(|p| py_eq(p, &Value::str(k.as_str()))))
            .cloned()
            .collect();
        self.forget(&gone);
    }

    /// Events that change what a tab should show, for an instant refresh.
    pub fn subscriptions(&self) -> Vec<Value> {
        if !self.enabled {
            return Vec::new();
        }
        let mut panes: Vec<String> = self
            .latest
            .borrow()
            .get("agents")
            .list_or_empty()
            .iter()
            .filter_map(|agent| agent.get("pane_id").as_str().map(|s| s.to_string()))
            .collect();
        panes.sort();
        panes.dedup();
        let mut out = vec![dict(vec![("type", Value::str("pane.agent_detected"))])];
        for pane in panes {
            out.push(dict(vec![("type", Value::str("pane.agent_status_changed")), ("pane_id", Value::str(pane))]));
        }
        out
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    #[test]
    fn strips_one_prefix() {
        let logo = icons::logo(0);
        assert_eq!(strip("◐ name"), "name");
        assert_eq!(strip(&format!("{}  ● name", logo)), "name");
        assert_eq!(strip("◐ ◐ name"), "◐ name");
        assert_eq!(strip(&format!("{}   name", logo)), format!("{}   name", logo));
        assert_eq!(strip("  name"), "name");
        assert_eq!(strip("name"), "name");
    }

    // -- ported from tests/test_tab_status.py ----------------------------------

    fn claude() -> char {
        icons::logo_for(Some("claude")).unwrap()
    }

    #[test]
    fn every_label_strips_back_to_the_name() {
        for logo in [None, Some(claude())] {
            for status in ["blocked", "working", "done", "idle", "unknown", "odd"] {
                let label = compose("Fix login", status, logo);
                assert_eq!(strip(&label), "Fix login", "{}", label);
            }
        }
    }

    #[test]
    fn glyphs_are_static() {
        assert_eq!(compose("x", "blocked", Some(claude())), format!("{}  ◉ x", claude()));
        assert_eq!(compose("x", "working", None), "◐ x");
        assert_eq!(compose("x", "idle", None), "✓ x");
    }

    #[test]
    fn labels_from_the_animated_release_still_strip() {
        let logo = claude();
        for label in [
            "⠋ Fix login".to_string(),
            "⠏ Fix login".to_string(),
            "  Fix login".to_string(),
            format!("{}  ⠹ Fix login", logo),
            format!("{}    Fix login", logo),
        ] {
            assert_eq!(strip(&label), "Fix login", "{}", label);
        }
    }

    #[test]
    fn plain_names_are_left_alone() {
        for label in ["Fix login", "●", "◉notes", " lead", ""] {
            assert_eq!(strip(label), label);
        }
    }

    pub(crate) fn session(status: &str, label: &str) -> Value {
        let mut data = pyjson::loads(
            r#"{"focused_workspace_id": "w1", "focused_tab_id": "w1:t1",
                "tabs": [{"tab_id": "w1:t1", "workspace_id": "w1", "label": "1"},
                         {"tab_id": "w1:t2", "workspace_id": "w1", "label": ""}],
                "panes": [{"pane_id": "w1:p2", "tab_id": "w1:t2", "terminal_id": "term2"}],
                "agents": [{"pane_id": "w1:p2", "tab_id": "w1:t2", "agent": "claude",
                            "agent_status": "", "cwd": "/work/app"}]}"#,
        )
        .unwrap();
        set(&mut list(&mut data, "tabs")[1], "label", Value::str(label));
        set(&mut list(&mut data, "agents")[0], "agent_status", Value::str(status));
        data
    }

    pub(crate) fn list<'a>(data: &'a mut Value, key: &str) -> &'a mut Vec<Value> {
        match data.as_dict_mut().unwrap().get_mut(key) {
            Some(Value::List(items)) => items,
            _ => panic!("no list at {}", key),
        }
    }

    pub(crate) fn set(item: &mut Value, key: &str, value: Value) {
        item.as_dict_mut().unwrap().insert(key, value);
    }

    #[cfg(unix)]
    pub(crate) fn config(json: &str) -> Config {
        Config::from_values(pyjson::loads(json).unwrap().as_dict().unwrap())
    }

    #[test]
    fn bar_rows_show_the_bare_tab_name() {
        let mut data = session("working", "◐ Fix login");
        set(&mut list(&mut data, "tabs")[0], "label", Value::str("◐ my plain tab"));
        let items = crate::items::build_items(&data);
        let title = |key: &str| items.iter().find(|item| item.key == key).unwrap().title.clone();
        assert_eq!(title("w1:p2"), "Fix login");
        assert_eq!(title("w1:t1"), "◐ my plain tab");
    }

    #[test]
    fn without_a_stream_wait_just_sleeps() {
        let mut stream = crate::client::EventStream::new(None, 1.0);
        stream.follow(&[dict(vec![("type", Value::str("tab.focused"))])]).unwrap();
        let started = Instant::now();
        assert!(!stream.wait(0.05));
        assert!(started.elapsed() >= Duration::from_millis(50));
    }

    #[cfg(unix)]
    pub(crate) use fake::*;

    /// A Herdr socket that answers snapshots, renames and plugin lists, and
    /// applies renames to its data as the real server does.
    #[cfg(unix)]
    mod fake {
        use super::*;
        use crate::client::HerdrClient;
        use std::io::{BufRead, BufReader, Write};
        use std::os::unix::net::UnixListener;
        use std::path::PathBuf;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::{Arc, Mutex, MutexGuard};

        pub struct State {
            pub data: Value,
            pub renames: Vec<(String, String)>,
            pub fail_rename: bool,
            /// The whole `plugin.list` response.
            pub plugin_reply: Value,
            /// A file read at every rename, before it is applied.
            pub peek: Option<PathBuf>,
            pub peeked: Vec<String>,
        }

        pub struct FakeHerdr {
            pub path: PathBuf,
            state: Arc<Mutex<State>>,
            stop: Arc<AtomicBool>,
        }

        impl FakeHerdr {
            pub fn new(data: Value) -> FakeHerdr {
                let path = crate::testutil::socket_path();
                let listener = UnixListener::bind(&path).unwrap();
                let state = Arc::new(Mutex::new(State {
                    data,
                    renames: Vec::new(),
                    fail_rename: false,
                    plugin_reply: pyjson::loads(r#"{"result": {"plugins": []}}"#).unwrap(),
                    peek: None,
                    peeked: Vec::new(),
                }));
                let stop = Arc::new(AtomicBool::new(false));
                let (shared, stopped) = (state.clone(), stop.clone());
                std::thread::spawn(move || {
                    for conn in listener.incoming() {
                        if stopped.load(Ordering::SeqCst) {
                            break;
                        }
                        let Ok(mut conn) = conn else { continue };
                        let mut line = String::new();
                        if BufReader::new(&conn).read_line(&mut line).is_err() {
                            continue;
                        }
                        let Ok(request) = pyjson::loads(&line) else { continue };
                        let reply = FakeHerdr::answer(&mut shared.lock().unwrap(), &request);
                        let _ = conn.write_all(format!("{}\n", pyjson::dumps(&reply, false)).as_bytes());
                    }
                });
                FakeHerdr { path, state, stop }
            }

            fn answer(state: &mut State, request: &Value) -> Value {
                let params = request.get("params");
                match request.get("method").as_str().unwrap_or("") {
                    "session.snapshot" => dict(vec![("result", dict(vec![("snapshot", state.data.clone())]))]),
                    "plugin.list" => state.plugin_reply.clone(),
                    "tab.rename" => {
                        if state.fail_rename {
                            return pyjson::loads(r#"{"error": {"message": "rename refused"}}"#).unwrap();
                        }
                        if let Some(peek) = &state.peek {
                            let text = std::fs::read_to_string(peek).unwrap_or_default();
                            state.peeked.push(text);
                        }
                        let tab_id = params.get("tab_id").as_str().unwrap().to_string();
                        let label = params.get("label").as_str().unwrap().to_string();
                        for tab in list(&mut state.data, "tabs") {
                            if tab.get("tab_id").as_str() == Some(tab_id.as_str()) {
                                set(tab, "label", Value::str(label.clone()));
                            }
                        }
                        state.renames.push((tab_id, label));
                        dict(vec![("result", Value::Dict(Map::new()))])
                    }
                    _ => pyjson::loads(r#"{"error": {"message": "unknown method"}}"#).unwrap(),
                }
            }

            pub fn state(&self) -> MutexGuard<'_, State> {
                self.state.lock().unwrap()
            }

            pub fn renames(&self) -> Vec<(String, String)> {
                self.state().renames.clone()
            }

            pub fn label(&self, tab_id: &str) -> String {
                let state = self.state();
                let tab = state.data.get("tabs").list_or_empty().iter().find(|t| t.get("tab_id").as_str() == Some(tab_id)).cloned();
                tab.unwrap().get("label").as_str().unwrap().to_string()
            }

            pub fn client(&self) -> HerdrClient {
                HerdrClient::new(Some(self.path.to_string_lossy().into_owned()), 2.0, true)
            }

            pub fn writer(&self) -> AutomaticTabClient {
                AutomaticTabClient::new(self.client())
            }
        }

        impl Drop for FakeHerdr {
            fn drop(&mut self) {
                self.stop.store(true, Ordering::SeqCst);
                let _ = std::os::unix::net::UnixStream::connect(&self.path);
                let _ = std::fs::remove_file(&self.path);
            }
        }

        pub fn pair(t1: &str, t2: &str) -> Vec<(String, String)> {
            vec![(t1.to_string(), t2.to_string())]
        }

        /// The environment every client-side test runs in: no user config.
        pub fn quiet_env() -> crate::testutil::EnvVars {
            let mut env = crate::testutil::EnvVars::new();
            env.remove("HERDR_PLUGIN_CONFIG_DIR").remove("HERDR_SOCKET_PATH");
            env
        }
    }

    #[cfg(unix)]
    mod with_server {
        use super::*;
        use crate::testutil::{env_lock, TempDir};

        fn make(server: &FakeHerdr, dir: &TempDir, extra: &str) -> TabStatus {
            let mut tabs = TabStatus::new(server.writer(), Some(dir.path().to_path_buf()));
            let extra = if extra.is_empty() { String::new() } else { format!(", {}", extra) };
            tabs.configure(&config(&format!(r#"{{"agent_icons": "none", "tab_status": true{}}}"#, extra)));
            tabs
        }

        fn restart(server: &FakeHerdr, dir: &TempDir) -> TabStatus {
            let mut tabs = TabStatus::new(server.writer(), Some(dir.path().to_path_buf()));
            tabs.configure(&config(r#"{"tab_status": true, "agent_icons": "none"}"#));
            tabs
        }

        fn tick(tabs: &mut TabStatus) {
            let snapshot = tabs.snapshot().unwrap();
            tabs.apply(&snapshot);
        }

        fn edit(server: &FakeHerdr, change: impl FnOnce(&mut Value)) {
            change(&mut server.state().data);
        }

        fn saved(dir: &TempDir) -> Value {
            pyjson::loads(&std::fs::read_to_string(dir.path().join("tab-status.json")).unwrap()).unwrap()
        }

        #[test]
        fn agent_tab_gets_status_and_logo_others_are_untouched() {
            let _env = env_lock();
            let _vars = quiet_env();
            let (server, dir) = (FakeHerdr::new(session("working", "2")), TempDir::new());
            let mut tabs = make(&server, &dir, r#""agent_icons": "font""#);
            tick(&mut tabs);
            assert_eq!(server.renames(), pair("w1:t2", &format!("{}  ◐ 2", claude())));
            tick(&mut tabs);
            assert_eq!(server.renames().len(), 1, "an unchanged tab is not renamed again");
        }

        #[test]
        fn status_changes_rewrite_only_the_prefix() {
            let _env = env_lock();
            let _vars = quiet_env();
            let (server, dir) = (FakeHerdr::new(session("working", "Fix login")), TempDir::new());
            let mut tabs = make(&server, &dir, "");
            tick(&mut tabs);
            edit(&server, |d| set(&mut list(d, "agents")[0], "agent_status", Value::str("done")));
            tick(&mut tabs);
            assert_eq!(server.label("w1:t2"), "● Fix login");
        }

        #[test]
        fn record_is_written_before_the_decorated_label() {
            let _env = env_lock();
            let _vars = quiet_env();
            let (server, dir) = (FakeHerdr::new(session("working", "2")), TempDir::new());
            server.state().peek = Some(dir.path().join("tab-status.json"));
            let mut tabs = make(&server, &dir, "");
            tick(&mut tabs);
            let peeked = server.state().peeked.clone();
            assert!(!peeked.is_empty());
            let first = pyjson::loads(&peeked[0]).expect("record saved before the rename");
            assert!(first.as_dict().unwrap().contains_key("w1:t2"));
        }

        #[test]
        fn unwritable_state_never_leaves_an_unremovable_prefix() {
            let _env = env_lock();
            let _vars = quiet_env();
            let (server, dir) = (FakeHerdr::new(session("working", "2")), TempDir::new());
            // Writes into a directory that does not exist always fail.
            let mut tabs = TabStatus::new(server.writer(), Some(dir.path().join("missing")));
            tabs.configure(&config(r#"{"agent_icons": "none", "tab_status": true}"#));
            tick(&mut tabs);
            assert_eq!(server.renames(), vec![]);
            assert_eq!(server.label("w1:t2"), "2");
        }

        #[test]
        fn unnamed_tab_keeps_following_its_position() {
            let _env = env_lock();
            let _vars = quiet_env();
            let (server, dir) = (FakeHerdr::new(session("working", "2")), TempDir::new());
            let mut tabs = make(&server, &dir, "");
            tick(&mut tabs);
            edit(&server, |d| {
                list(d, "tabs").remove(0); // the first tab closes; herdr would say "1" now
            });
            tick(&mut tabs);
            assert_eq!(server.label("w1:t2"), "◐ 1");
            edit(&server, |d| list(d, "agents").clear());
            tick(&mut tabs);
            assert_eq!(server.label("w1:t2"), "1");
            edit(&server, |d| {
                let tab = pyjson::loads(r#"{"tab_id": "w1:t0", "workspace_id": "w1", "label": "1"}"#).unwrap();
                list(d, "tabs").insert(0, tab);
            });
            tick(&mut tabs);
            assert_eq!(server.label("w1:t2"), "2", "still numbered like an unnamed tab");
        }

        #[test]
        fn external_rename_yields_across_status_changes_and_restart() {
            let _env = env_lock();
            let _vars = quiet_env();
            let (server, dir) = (FakeHerdr::new(session("working", "2")), TempDir::new());
            let mut tabs = make(&server, &dir, "");
            tick(&mut tabs);
            edit(&server, |d| set(&mut list(d, "tabs")[1], "label", Value::str("Release notes")));
            for status in ["working", "done", "idle"] {
                edit(&server, |d| set(&mut list(d, "agents")[0], "agent_status", Value::str(status)));
                tick(&mut tabs);
            }
            let mut restarted = restart(&server, &dir);
            tick(&mut restarted);
            restarted.clear();
            assert_eq!(server.label("w1:t2"), "Release notes");
            assert_eq!(server.renames(), pair("w1:t2", "◐ 2"));
        }

        #[test]
        fn competing_plugin_restoring_original_name_does_not_loop() {
            let _env = env_lock();
            let _vars = quiet_env();
            let (server, dir) = (FakeHerdr::new(session("working", "Fix login")), TempDir::new());
            let mut tabs = make(&server, &dir, "");
            tick(&mut tabs);
            assert!(!saved(&dir).get("w1:t2").as_dict().unwrap().contains_key("previous"));
            let mut tabs = restart(&server, &dir);
            for _ in 0..5 {
                edit(&server, |d| set(&mut list(d, "tabs")[1], "label", Value::str("Fix login")));
                tick(&mut tabs);
            }
            assert_eq!(server.renames(), pair("w1:t2", "◐ Fix login"));
            assert_eq!(server.label("w1:t2"), "Fix login");
        }

        #[test]
        fn disable_and_stop_do_not_strip_another_writers_glyph() {
            let _env = env_lock();
            let _vars = quiet_env();
            for cleanup in ["disable", "stop"] {
                let (server, dir) = (FakeHerdr::new(session("working", "Fix login")), TempDir::new());
                let mut tabs = make(&server, &dir, "");
                tick(&mut tabs);
                edit(&server, |d| set(&mut list(d, "tabs")[1], "label", Value::str("✓ Other plugin")));
                if cleanup == "disable" {
                    tabs.configure(&config(r#"{"tab_status": false}"#));
                    tick(&mut tabs);
                } else {
                    tabs.clear();
                }
                assert_eq!(server.label("w1:t2"), "✓ Other plugin", "{}", cleanup);
                assert!(server.renames().len() <= 1, "{}", cleanup);
            }
        }

        #[test]
        fn external_rename_between_snapshot_and_write_is_respected() {
            let _env = env_lock();
            let _vars = quiet_env();
            let (server, dir) = (FakeHerdr::new(session("working", "Fix login")), TempDir::new());
            let mut tabs = make(&server, &dir, "");
            let snapshot = tabs.snapshot().unwrap();
            edit(&server, |d| set(&mut list(d, "tabs")[1], "label", Value::str("Other plugin")));
            tabs.apply(&snapshot);
            tick(&mut tabs);
            assert_eq!(server.label("w1:t2"), "Other plugin");
            assert_eq!(server.renames(), vec![]);
        }

        #[test]
        fn glyph_led_names_on_unrecorded_tabs_are_not_stripped() {
            let _env = env_lock();
            let _vars = quiet_env();
            let mut data = session("idle", "● pinned");
            set(&mut list(&mut data, "tabs")[0], "label", Value::str("✓ notes"));
            let (server, dir) = (FakeHerdr::new(data), TempDir::new());
            let mut tabs = make(&server, &dir, "");
            tick(&mut tabs);
            assert_eq!(server.label("w1:t1"), "✓ notes");
            assert_eq!(server.label("w1:t2"), "✓ ● pinned");
        }

        #[test]
        fn most_urgent_agent_leads_a_shared_tab() {
            let _env = env_lock();
            let _vars = quiet_env();
            let mut data = session("working", "Pair");
            let codex = pyjson::loads(r#"{"pane_id": "w1:p3", "tab_id": "w1:t2", "agent": "codex", "agent_status": "blocked"}"#).unwrap();
            list(&mut data, "agents").push(codex);
            let (server, dir) = (FakeHerdr::new(data), TempDir::new());
            let mut tabs = make(&server, &dir, r#""agent_icons": "font""#);
            tick(&mut tabs);
            assert_eq!(server.label("w1:t2"), format!("{}  ◉ Pair", icons::logo_for(Some("codex")).unwrap()));
        }

        #[test]
        fn labels_change_only_with_status() {
            let _env = env_lock();
            let _vars = quiet_env();
            let (server, dir) = (FakeHerdr::new(session("blocked", "Fix")), TempDir::new());
            let mut tabs = make(&server, &dir, "");
            tick(&mut tabs);
            assert_eq!(server.label("w1:t2"), "◉ Fix");
            for focus in ["w1:t2", "w1:t1", "w2:t1"] {
                edit(&server, |d| {
                    set(d, "focused_tab_id", Value::str(focus));
                    set(d, "focused_workspace_id", Value::str(focus.split(':').next().unwrap()));
                });
                tick(&mut tabs);
            }
            assert_eq!(server.renames().len(), 1, "focus and time never rename a tab");
            edit(&server, |d| set(&mut list(d, "agents")[0], "agent_status", Value::str("working")));
            tick(&mut tabs);
            tick(&mut tabs);
            assert_eq!(server.renames()[1..].to_vec(), pair("w1:t2", "◐ Fix"));
        }

        #[test]
        fn an_animated_label_settles_in_one_rename() {
            let _env = env_lock();
            let _vars = quiet_env();
            let (server, dir) = (FakeHerdr::new(session("working", "Fix")), TempDir::new());
            let mut tabs = make(&server, &dir, "");
            let records = pyjson::loads(r#"{"w1:t2": {"base": "Fix", "default": false}}"#).unwrap();
            tabs.save(records.as_dict().unwrap().clone()).unwrap();
            // left behind by the animated release
            edit(&server, |d| set(&mut list(d, "tabs")[1], "label", Value::str("⠸ Fix")));
            tick(&mut tabs);
            tick(&mut tabs);
            assert_eq!(server.renames(), pair("w1:t2", "◐ Fix"));
        }

        #[test]
        fn disabling_and_clearing_restore_bare_names() {
            let _env = env_lock();
            let _vars = quiet_env();
            let (server, dir) = (FakeHerdr::new(session("working", "Fix")), TempDir::new());
            let mut tabs = make(&server, &dir, "");
            tick(&mut tabs);
            tabs.configure(&config(r#"{"tab_status": false}"#));
            tick(&mut tabs);
            assert_eq!(server.label("w1:t2"), "Fix");
            tabs.configure(&config(r#"{"tab_status": true}"#));
            tick(&mut tabs);
            tabs.clear();
            assert_eq!(server.label("w1:t2"), "Fix");
            assert!(TabStatus::new(server.writer(), Some(dir.path().to_path_buf())).records.is_empty());
        }

        #[test]
        fn records_survive_a_restart() {
            let _env = env_lock();
            let _vars = quiet_env();
            let (server, dir) = (FakeHerdr::new(session("working", "Fix")), TempDir::new());
            let mut tabs = make(&server, &dir, "");
            tick(&mut tabs);
            edit(&server, |d| set(&mut list(d, "agents")[0], "agent_status", Value::str("idle")));
            let mut restarted = make(&server, &dir, "");
            tick(&mut restarted);
            assert_eq!(server.label("w1:t2"), "✓ Fix", "no second prefix after a restart");
        }

        #[test]
        fn failed_rename_is_retried_later() {
            let _env = env_lock();
            let _vars = quiet_env();
            let (server, dir) = (FakeHerdr::new(session("working", "Fix")), TempDir::new());
            let mut tabs = make(&server, &dir, "");
            server.state().fail_rename = true;
            tick(&mut tabs);
            server.state().fail_rename = false;
            tick(&mut tabs);
            assert_eq!(server.label("w1:t2"), "◐ Fix");
        }

        #[test]
        fn failed_rename_preserves_glyph_led_name_after_restart() {
            let _env = env_lock();
            let _vars = quiet_env();
            let (server, dir) = (FakeHerdr::new(session("working", "● pinned")), TempDir::new());
            let mut tabs = make(&server, &dir, "");
            server.state().fail_rename = true;
            tick(&mut tabs);
            server.state().fail_rename = false;
            let mut restarted = make(&server, &dir, "");
            tick(&mut restarted);
            assert_eq!(server.label("w1:t2"), "◐ ● pinned");
            restarted.clear();
            assert_eq!(server.label("w1:t2"), "● pinned");
        }

        const SESSION: &str = "12345678-1234-1234-1234-123456789abc";

        fn codex_titled(data: &mut Value) {
            let agent = &mut list(data, "agents")[0];
            set(agent, "agent", Value::str("codex"));
            set(agent, "terminal_title_stripped", Value::str("Repair OAuth | app"));
            let id = pyjson::loads(&format!(r#"{{"agent": "codex", "kind": "id", "value": "{}"}}"#, SESSION)).unwrap();
            set(agent, "agent_session", id);
        }

        fn titled(titles: &mut crate::auto_title::AutoTitles, tabs: &mut TabStatus) {
            let snapshot = tabs.snapshot().unwrap();
            let snapshot = titles.apply(tabs, snapshot).unwrap();
            tabs.apply(&snapshot);
        }

        #[test]
        fn auto_title_arrives_decorated_in_one_rename() {
            let _env = env_lock();
            let mut vars = quiet_env();
            let mut data = session("working", "2");
            set(&mut list(&mut data, "tabs")[1], "number", Value::Int(2));
            codex_titled(&mut data);
            let (server, dir) = (FakeHerdr::new(data), TempDir::new());
            let mut tabs = make(&server, &dir, "");
            vars.set("HERDR_PLUGIN_STATE_DIR", dir.path()).set("HERDR_AUTO_TITLE_TRANSCRIPT", "true");
            let mut titles = crate::auto_title::AutoTitles::new();
            titles.configure(&config(r#"{"auto_titles": true}"#));
            titled(&mut titles, &mut tabs);
            assert_eq!(server.renames(), pair("w1:t2", "◐ Repair OAuth"));
            // Ownership sees the bare title, so later task titles keep flowing.
            edit(&server, |d| {
                set(&mut list(d, "agents")[0], "terminal_title_stripped", Value::str("Repair OAuth callbacks | app"))
            });
            titled(&mut titles, &mut tabs);
            assert_eq!(server.label("w1:t2"), "◐ Repair OAuth callbacks");
        }

        #[test]
        fn conflict_blocks_task_titles_even_if_other_plugin_restores_a_number() {
            let _env = env_lock();
            let mut vars = quiet_env();
            vars.remove("HERDR_PLUGIN_STATE_DIR").remove("HERDR_AUTO_TITLE_TRANSCRIPT");
            let mut data = session("working", "2");
            codex_titled(&mut data);
            let (server, dir) = (FakeHerdr::new(data), TempDir::new());
            let mut tabs = make(&server, &dir, "");
            let mut titles = crate::auto_title::AutoTitles::new();
            titles.configure(&config(r#"{"auto_titles": true}"#));
            titled(&mut titles, &mut tabs);
            edit(&server, |d| set(&mut list(d, "tabs")[1], "label", Value::str("2")));
            for _ in 0..3 {
                titled(&mut titles, &mut tabs);
            }
            assert_eq!(server.renames(), pair("w1:t2", "◐ Repair OAuth"));
            assert_eq!(server.label("w1:t2"), "2");
        }

        #[test]
        fn subscriptions_follow_agent_panes_only() {
            let _env = env_lock();
            let _vars = quiet_env();
            let (server, dir) = (FakeHerdr::new(session("working", "2")), TempDir::new());
            let mut tabs = make(&server, &dir, "");
            tabs.snapshot().unwrap();
            let expected = pyjson::loads(
                r#"[{"type": "pane.agent_detected"}, {"type": "pane.agent_status_changed", "pane_id": "w1:p2"}]"#,
            )
            .unwrap();
            assert_eq!(Value::List(tabs.subscriptions()), expected);
            tabs.configure(&config(r#"{"tab_status": false}"#));
            assert_eq!(tabs.subscriptions(), vec![]);
        }
    }

    /// EventStreamTest: a stand-in Herdr that holds the subscription open.
    #[cfg(unix)]
    mod event_stream {
        use super::*;
        use crate::client::EventStream;
        use std::io::{BufRead, BufReader, Write};
        use std::os::unix::net::{UnixListener, UnixStream};
        use std::sync::mpsc;

        fn wanted() -> Vec<Value> {
            vec![dict(vec![("type", Value::str("pane.agent_status_changed")), ("pane_id", Value::str("w1:p2"))])]
        }

        /// Accept connections, hand each request and its stream to the test.
        fn serve(reply: &'static [u8]) -> (String, mpsc::Receiver<(Value, UnixStream)>) {
            let path = crate::testutil::socket_path();
            let listener = UnixListener::bind(&path).unwrap();
            let (send, receive) = mpsc::channel();
            std::thread::spawn(move || {
                for conn in listener.incoming() {
                    let Ok(mut conn) = conn else { break };
                    let mut line = String::new();
                    let _ = BufReader::new(&conn).read_line(&mut line);
                    let _ = conn.write_all(reply);
                    if send.send((pyjson::loads(&line).unwrap_or(Value::Null), conn)).is_err() {
                        break;
                    }
                }
            });
            (path.to_string_lossy().into_owned(), receive)
        }

        fn no_connection(receive: &mpsc::Receiver<(Value, UnixStream)>) -> bool {
            receive.recv_timeout(Duration::from_millis(100)).is_err()
        }

        #[test]
        fn events_wake_and_silence_times_out() {
            let (path, receive) = serve(b"{\"id\":\"bar-events\",\"result\":{\"type\":\"subscription_started\"}}\n");
            let mut stream = EventStream::new(Some(path), 2.0);
            stream.follow(&wanted()).unwrap();
            let (request, mut held) = receive.recv_timeout(Duration::from_secs(2)).unwrap();
            assert_eq!(request.get("params").get("subscriptions"), &Value::List(wanted()));
            let started = Instant::now();
            assert!(!stream.wait(0.05));
            assert!(started.elapsed() < Duration::from_secs(1));
            held.write_all(b"{\"event\":\"pane.agent_status_changed\",\"data\":{}}\n").unwrap();
            assert!(stream.wait(2.0));
            stream.follow(&wanted()).unwrap(); // unchanged: no reconnect
            assert!(no_connection(&receive));
            drop(held);
            assert!(stream.wait(2.0), "a dropped stream wakes the caller");
            // The stream is gone: waiting just sleeps.
            assert!(!stream.wait(0.01));
            stream.follow(&wanted()).unwrap();
            assert!(no_connection(&receive), "reconnects wait for the retry delay");
            stream.close();
        }

        #[test]
        fn refused_subscription_raises() {
            let (path, receive) = serve(b"{\"id\":\"bar-events\",\"error\":{\"code\":\"pane_not_found\",\"message\":\"gone\"}}\n");
            let mut stream = EventStream::new(Some(path), 2.0);
            let gone = [dict(vec![("type", Value::str("pane.agent_status_changed")), ("pane_id", Value::str("w1:p9"))])];
            assert!(stream.follow(&gone).is_err());
            let (_, conn) = receive.recv_timeout(Duration::from_secs(2)).unwrap();
            drop(conn);
            // No connection kept: waiting just sleeps, and retries back off.
            assert!(!stream.wait(0.01));
            stream.follow(&gone).unwrap();
            assert!(no_connection(&receive));
        }

        #[test]
        fn event_coalesced_with_acknowledgement_is_not_lost() {
            let (path, receive) = serve(
                b"{\"result\":{\"type\":\"subscription_started\"}}\n{\"event\":\"pane.agent_status_changed\"}\n",
            );
            let mut stream = EventStream::new(Some(path), 2.0);
            stream.follow(&[dict(vec![("type", Value::str("pane.agent_detected"))])]).unwrap();
            let (_, _held) = receive.recv_timeout(Duration::from_secs(2)).unwrap();
            let started = Instant::now();
            assert!(stream.wait(1.0));
            assert!(started.elapsed() < Duration::from_millis(500));
            assert!(!stream.wait(0.0));
            stream.close();
        }

        #[test]
        fn continuous_event_traffic_does_not_trap_the_waiter() {
            let (path, receive) = serve(b"{\"result\":{\"type\":\"subscription_started\"}}\n");
            let mut stream = EventStream::new(Some(path), 2.0);
            stream.follow(&wanted()).unwrap();
            let (_, held) = receive.recv_timeout(Duration::from_secs(2)).unwrap();
            // Keep writing for longer than the wait may take.
            let writer = std::thread::spawn(move || {
                let mut held = held;
                let chunk = vec![b'x'; 65536];
                let until = Instant::now() + Duration::from_millis(500);
                while Instant::now() < until {
                    if held.write_all(&chunk).is_err() {
                        break;
                    }
                }
            });
            let started = Instant::now();
            assert!(stream.wait(1.0));
            assert!(started.elapsed() < Duration::from_millis(500));
            stream.close();
            writer.join().unwrap();
        }

        #[test]
        fn invalid_acknowledgement_closes_stream_and_backs_off() {
            static LONG: [u8; 65537] = [b'x'; 65537];
            for response in [&b"[]\n"[..], &LONG[..]] {
                let (path, receive) = serve(response);
                let mut stream = EventStream::new(Some(path), 2.0);
                let detected = [dict(vec![("type", Value::str("pane.agent_detected"))])];
                assert!(stream.follow(&detected).is_err(), "{}", response.len());
                let (_, conn) = receive.recv_timeout(Duration::from_secs(2)).unwrap();
                // Our side closed: the peer sees end of stream.
                let _ = conn.set_read_timeout(Some(Duration::from_secs(2)));
                let mut rest = Vec::new();
                let _ = std::io::Read::read_to_end(&mut &conn, &mut rest);
                assert!(!stream.wait(0.01));
                stream.follow(&detected).unwrap();
                assert!(no_connection(&receive), "{}", response.len());
            }
        }
    }
}
