//! A fake Herdr on a Unix socket serving the busy session: one request per
//! connection, as `tests/parity/herdr.rs` does, but with a preview per pane,
//! a pid per pane, live status changes and a note of where the bar jumped.

use crate::pyjson::{self, Map, Value};
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

pub struct State {
    pub data: Value,
    pub previews: BTreeMap<String, String>,
    pub fallback: String,
    pub pids: BTreeMap<String, i128>,
    /// The title of the row the bar jumped to, once it has.
    pub jumped: Option<String>,
}

pub struct Herdr {
    path: PathBuf,
    pub state: Arc<Mutex<State>>,
    closing: Arc<AtomicBool>,
    subscribers: Arc<Mutex<Vec<UnixStream>>>,
}

fn obj(key: &str, value: Value) -> Value {
    let mut map = Map::new();
    map.insert(key, value);
    Value::Dict(map)
}

impl Herdr {
    pub fn start(path: &Path, state: State) -> Herdr {
        let listener =
            UnixListener::bind(path).unwrap_or_else(|e| panic!("bind {}: {}", path.display(), e));
        let herdr = Herdr {
            path: path.to_path_buf(),
            state: Arc::new(Mutex::new(state)),
            closing: Arc::new(AtomicBool::new(false)),
            subscribers: Arc::new(Mutex::new(Vec::new())),
        };
        let (state, closing, subscribers) = (
            herdr.state.clone(),
            herdr.closing.clone(),
            herdr.subscribers.clone(),
        );
        std::thread::spawn(move || {
            for conn in listener.incoming() {
                if closing.load(Ordering::SeqCst) {
                    return;
                }
                let Ok(conn) = conn else { continue };
                let (state, subscribers) = (state.clone(), subscribers.clone());
                std::thread::spawn(move || serve(&state, &subscribers, conn));
            }
        });
        herdr
    }

    /// Move one agent to another state, the way a live session would, and
    /// give its pane the output that goes with it.
    pub fn set_status(&self, pane_id: &str, status: &str, preview: Option<String>) {
        let tab_id = pane_id.replace(":p", ":t");
        let mut state = self.state.lock().unwrap();
        if let Some(map) = state.data.as_dict_mut() {
            for group in ["agents", "panes", "tabs"] {
                if let Some(Value::List(records)) = map.get_mut(group) {
                    for record in records.iter_mut() {
                        let matches = record.get("pane_id").as_str() == Some(pane_id)
                            || (group == "tabs"
                                && record.get("tab_id").as_str() == Some(tab_id.as_str()));
                        if let (true, Some(record)) = (matches, record.as_dict_mut()) {
                            record.insert("agent_status", Value::str(status));
                        }
                    }
                }
            }
        }
        if let Some(text) = preview {
            state.previews.insert(pane_id.to_string(), text);
        }
    }

    pub fn close(&self) {
        if self.closing.swap(true, Ordering::SeqCst) {
            return;
        }
        let _ = UnixStream::connect(&self.path);
        for conn in self.subscribers.lock().unwrap().drain(..) {
            let _ = conn.shutdown(std::net::Shutdown::Both);
        }
    }
}

impl Drop for Herdr {
    fn drop(&mut self) {
        self.close();
    }
}

fn serve(state: &Mutex<State>, subscribers: &Mutex<Vec<UnixStream>>, mut conn: UnixStream) {
    let mut buffered = Vec::new();
    let mut chunk = vec![0u8; 65536];
    while !buffered.contains(&b'\n') {
        match conn.read(&mut chunk) {
            Ok(0) | Err(_) => return,
            Ok(n) => buffered.extend_from_slice(&chunk[..n]),
        }
    }
    let line = &buffered[..buffered.iter().position(|b| *b == b'\n').unwrap()];
    let Ok(request) = pyjson::loads_bytes(line) else {
        return;
    };
    let id = match request.get("id") {
        Value::Null => Value::str("bar"),
        id => id.clone(),
    };
    let method = request.get("method").as_str().unwrap_or("").to_string();
    let params = request.get("params").clone();
    let result = if method == "events.subscribe" {
        Value::Dict(Map::new())
    } else {
        answer(&mut state.lock().unwrap(), &method, &params)
    };
    let message: Map = [("id".to_string(), id), ("result".to_string(), result)]
        .into_iter()
        .collect();
    let mut out = pyjson::dumps(&Value::Dict(message), false).into_bytes();
    out.push(b'\n');
    if conn.write_all(&out).is_ok() && method == "events.subscribe" {
        // Kept open and silent: the bar's own refresh picks changes up.
        subscribers.lock().unwrap().push(conn);
    }
}

fn tab_label(data: &Value, tab_id: &str) -> Option<String> {
    data.get("tabs")
        .list_or_empty()
        .iter()
        .find(|tab| tab.get("tab_id").as_str() == Some(tab_id))
        .and_then(|tab| tab.get("label").as_str().map(str::to_string))
}

fn answer(state: &mut State, method: &str, params: &Value) -> Value {
    let pane_id = params.get("pane_id").as_str().unwrap_or("").to_string();
    match method {
        "session.snapshot" => obj("snapshot", state.data.clone()),
        "pane.read" => {
            let text = state
                .previews
                .get(&pane_id)
                .cloned()
                .unwrap_or_else(|| state.fallback.clone());
            obj("read", obj("text", Value::str(text)))
        }
        "pane.process_info" => {
            let info = match state.pids.get(&pane_id) {
                Some(pid) => obj("shell_pid", Value::Int(*pid)),
                None => Value::Dict(Map::new()),
            };
            obj("process_info", info)
        }
        "tab.rename" => {
            if let Some(Value::List(tabs)) =
                state.data.as_dict_mut().and_then(|d| d.get_mut("tabs"))
            {
                for tab in tabs.iter_mut() {
                    if pyjson::py_eq(tab.get("tab_id"), params.get("tab_id")) {
                        if let Some(tab) = tab.as_dict_mut() {
                            tab.insert("label", params.get("label").clone());
                        }
                    }
                }
            }
            Value::Dict(Map::new())
        }
        "tab.close" => {
            let tab_id = params.get("tab_id").clone();
            if let Some(map) = state.data.as_dict_mut() {
                for key in ["tabs", "agents", "panes"] {
                    let kept: Vec<Value> = map
                        .get(key)
                        .map(|v| v.list_or_empty())
                        .unwrap_or(&[])
                        .iter()
                        .filter(|entry| !pyjson::py_eq(entry.get("tab_id"), &tab_id))
                        .cloned()
                        .collect();
                    map.insert(key, Value::List(kept));
                }
            }
            Value::Dict(Map::new())
        }
        method if method.contains("focus") => {
            // A jump: name the row by its tab, as the caption under it does.
            let tab_id = match params.get("tab_id").as_str() {
                Some(tab_id) => Some(tab_id.to_string()),
                None => state
                    .data
                    .get("panes")
                    .list_or_empty()
                    .iter()
                    .find(|pane| pane.get("pane_id").as_str() == Some(pane_id.as_str()))
                    .and_then(|pane| pane.get("tab_id").as_str().map(str::to_string)),
            };
            if state.jumped.is_none() {
                state.jumped = tab_id.and_then(|tab_id| tab_label(&state.data, &tab_id));
            }
            Value::Dict(Map::new())
        }
        _ => Value::Dict(Map::new()),
    }
}
