//! Talk to the running Herdr server.
//!
//! Two transports, same surface:
//!
//! * the socket at `HERDR_SOCKET_PATH` -- one request per connection,
//!   newline-delimited JSON. A Unix socket on POSIX; on Windows the named
//!   pipe Herdr serves under the same name. Used for the hot paths
//!   (`session.snapshot` on every refresh, `pane.read` for the preview).
//! * the `herdr` CLI at `HERDR_BIN_PATH` -- portable, works when the socket
//!   is missing. Used as the automatic fallback for everything else.

use crate::host;
use crate::opener::herdr_bin;
use crate::pyjson::{self, Map, Value};
use crate::pystr;
use std::fmt;
use std::io;
#[cfg(unix)]
use std::io::{Read, Write};
use std::time::{Duration, Instant};

/// What kind of failure, as the Python release told them apart by class.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// HerdrError: a request failed, or no server could be reached.
    Herdr,
    /// RenamingPluginDetected, a HerdrError: another plugin names tabs.
    Renaming,
    /// OSError: the local filesystem or process table said no.
    Os,
}

/// A Herdr request failed, or no server could be reached; or, with
/// `Kind::Os`, a local operation failed along the way.
#[derive(Clone, Debug, PartialEq)]
pub struct HerdrError {
    pub message: String,
    pub kind: Kind,
}

impl fmt::Display for HerdrError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl HerdrError {
    pub fn new(message: impl Into<String>) -> HerdrError {
        HerdrError { message: message.into(), kind: Kind::Herdr }
    }

    pub fn os(error: &io::Error) -> HerdrError {
        HerdrError { message: host::os_error_str(error), kind: Kind::Os }
    }

    pub fn renaming(message: impl Into<String>) -> HerdrError {
        HerdrError { message: message.into(), kind: Kind::Renaming }
    }

    /// `except HerdrError`: everything but a local OSError.
    pub fn is_herdr(&self) -> bool {
        self.kind != Kind::Os
    }
}

impl From<io::Error> for HerdrError {
    fn from(error: io::Error) -> HerdrError {
        HerdrError::os(&error)
    }
}

pub type Result<T> = std::result::Result<T, HerdrError>;

/// What went wrong below the protocol: Python's OSError and ValueError.
enum Transport {
    Os(io::Error),
    Value,
    Herdr(HerdrError),
}

impl Transport {
    fn message(&self) -> String {
        match self {
            Transport::Os(error) => host::os_error_str(error),
            Transport::Value => "invalid JSON in herdr's response".into(),
            Transport::Herdr(error) => error.message.clone(),
        }
    }
}

fn env_path(name: &str) -> Option<String> {
    std::env::var_os(name).map(|v| v.to_string_lossy().into_owned()).filter(|v| !v.is_empty())
}

pub fn dict(entries: &[(&str, Value)]) -> Value {
    let mut map = Map::new();
    for (key, value) in entries {
        map.insert(*key, value.clone());
    }
    Value::Dict(map)
}

#[derive(Clone)]
pub struct HerdrClient {
    pub socket_path: Option<String>,
    pub bin_path: String,
    pub timeout: f64,
    pub socket_only: bool,
    socket_ok: bool,
}

impl HerdrClient {
    pub fn new(socket_path: Option<String>, timeout: f64, socket_only: bool) -> HerdrClient {
        let socket_path = socket_path.or_else(|| env_path("HERDR_SOCKET_PATH"));
        let mut client = HerdrClient { socket_path, bin_path: herdr_bin(), timeout, socket_only, socket_ok: false };
        client.socket_ok = client.socket_usable();
        client
    }

    pub fn from_env() -> HerdrClient {
        HerdrClient::new(None, 4.0, false)
    }

    pub fn socket_ok(&self) -> bool {
        self.socket_ok
    }

    // -- transports ---------------------------------------------------------

    fn socket_usable(&self) -> bool {
        match &self.socket_path {
            // On Windows the path is the marker file next to the named pipe.
            Some(path) => std::fs::metadata(path).is_ok(),
            None => false,
        }
    }

    fn socket_call(&self, method: &str, params: &Value) -> std::result::Result<Value, Transport> {
        let line = self.socket_line(method, params)?;
        HerdrClient::parse_line(&line)
    }

    /// One request over the socket: the raw response line, unparsed.
    fn socket_line(&self, method: &str, params: &Value) -> std::result::Result<Vec<u8>, Transport> {
        let request = dict(&[("id", Value::str("bar")), ("method", Value::str(method)), ("params", params.clone())]);
        let mut payload = pyjson::dumps(&request, false).into_bytes();
        payload.push(b'\n');
        let path = self.socket_path.as_deref().unwrap_or("");
        let line = if host::WINDOWS {
            host::pipe_request(&host::pipe_name(path), &payload, self.timeout).map_err(Transport::Os)?
        } else {
            unix_request(path, &payload, self.timeout).map_err(Transport::Os)?
        };
        Ok(line)
    }

    fn parse_line(line: &[u8]) -> std::result::Result<Value, Transport> {
        if line.is_empty() {
            return Err(Transport::Herdr(HerdrError::new("herdr closed the connection without a response")));
        }
        let text = String::from_utf8_lossy(line);
        pyjson::loads(&text).map_err(|_| Transport::Value)
    }

    fn cli(&self, args: &[String]) -> Result<Value> {
        let mut argv = vec![self.bin_path.clone()];
        argv.extend(args.iter().cloned());
        let joined = args.join(" ");
        let output = match crate::proc::run(&argv, self.timeout + 4.0) {
            Ok(output) => output,
            Err(crate::proc::RunError::Spawn(error)) if error.kind() == io::ErrorKind::NotFound => {
                return Err(HerdrError::new("herdr binary not found (set HERDR_BIN_PATH)"));
            }
            Err(crate::proc::RunError::Spawn(error)) => return Err(HerdrError::os(&error)),
            Err(crate::proc::RunError::Timeout) => return Err(HerdrError::new(format!("herdr {} timed out", joined))),
        };
        let out = String::from_utf8_lossy(&output.stdout);
        let out = pystr::strip(&out);
        if out.is_empty() {
            let err = String::from_utf8_lossy(&output.stderr);
            let err = pystr::strip(&err);
            if err.is_empty() {
                return Err(HerdrError::new(format!("herdr {} produced no output", joined)));
            }
            return Err(HerdrError::new(err));
        }
        let lines = pystr::splitlines(out);
        let last = lines.last().copied().unwrap_or("");
        pyjson::loads(last).map_err(|_| HerdrError::new(format!("herdr {} returned unparseable output", joined)))
    }

    pub fn unwrap(response: Value) -> Result<Map> {
        let Value::Dict(mut response) = response else {
            return Err(HerdrError::new("herdr returned an unexpected response shape"));
        };
        if let Some(error) = response.get("error") {
            if error.truthy() {
                let message = match error {
                    Value::Dict(map) => map.get("message").cloned().unwrap_or(Value::Null),
                    other => Value::str(pyjson::py_str(other)),
                };
                if message.truthy() {
                    return Err(HerdrError::new(pyjson::py_str(&message)));
                }
                return Err(HerdrError::new("herdr request failed"));
            }
        }
        match response.remove("result") {
            Some(Value::Dict(result)) => Ok(result),
            _ => Err(HerdrError::new("herdr returned an unexpected response shape")),
        }
    }

    /// Run a request over the socket, falling back to the CLI.
    pub fn call(&mut self, method: &str, params: Value, cli_args: &[&str]) -> Result<Map> {
        if self.socket_only {
            if self.socket_path.is_none() {
                return Err(HerdrError::new("background naming requires HERDR_SOCKET_PATH"));
            }
            return match self.socket_call(method, &params) {
                Ok(response) => HerdrClient::unwrap(response),
                Err(error) => Err(HerdrError::new(error.message())),
            };
        }
        if self.socket_ok {
            match self.socket_call(method, &params) {
                Ok(response) => return HerdrClient::unwrap(response),
                Err(Transport::Herdr(error)) => return Err(error),
                // Transport-level failure only: the server may have restarted
                // or handed off. Stop trusting the socket and use the CLI.
                Err(_) => self.socket_ok = false,
            }
        }
        let args: Vec<String> = cli_args.iter().map(|s| s.to_string()).collect();
        HerdrClient::unwrap(self.cli(&args)?)
    }

    // -- operations ---------------------------------------------------------

    pub fn snapshot(&mut self) -> Result<Value> {
        let result = self.call("session.snapshot", dict(&[]), &["api", "snapshot"])?;
        HerdrClient::snapshot_of(result)
    }

    fn snapshot_of(mut result: Map) -> Result<Value> {
        match result.remove("snapshot") {
            Some(snapshot @ Value::Dict(_)) => Ok(snapshot),
            _ => Err(HerdrError::new("session snapshot missing from response")),
        }
    }

    /// `snapshot`, or `Ok(None)` when Herdr's answer is byte for byte the
    /// one that produced the last snapshot this returned.
    ///
    /// `previous` holds that answer. Same bytes parse to the same snapshot,
    /// so a caller that already built everything from it can skip the work;
    /// a periodic refresh of an idle session is then one round trip. Only
    /// the socket answers are remembered; the CLI path always reads anew.
    pub fn snapshot_since(&mut self, previous: &mut Vec<u8>) -> Result<Option<Value>> {
        if !self.socket_only && self.socket_ok {
            let parsed = self.socket_line("session.snapshot", &dict(&[])).and_then(|line| {
                if !line.is_empty() && line == *previous {
                    return Ok(None);
                }
                HerdrClient::parse_line(&line).map(|response| Some((line, response)))
            });
            match parsed {
                Ok(None) => return Ok(None),
                Ok(Some((line, response))) => {
                    previous.clear();
                    let snapshot = HerdrClient::unwrap(response).and_then(HerdrClient::snapshot_of)?;
                    *previous = line;
                    return Ok(Some(snapshot));
                }
                Err(Transport::Herdr(error)) => {
                    previous.clear();
                    return Err(error);
                }
                // As in `call`: stop trusting the socket and use the CLI.
                Err(_) => self.socket_ok = false,
            }
        }
        previous.clear();
        self.snapshot().map(Some)
    }

    pub fn read_pane(&mut self, pane_id: &str, lines: i64, source: &str) -> Result<String> {
        let count = lines.to_string();
        let result = self.call(
            "pane.read",
            dict(&[("pane_id", Value::str(pane_id)), ("source", Value::str(source)), ("lines", Value::Int(lines as i128))]),
            &["pane", "read", pane_id, "--source", source, "--lines", &count],
        )?;
        if let Some(Value::Dict(read)) = result.get("read") {
            if let Some(Value::Str(text)) = read.get("text") {
                return Ok(text.clone());
            }
        }
        Ok(String::new())
    }

    pub fn process_info(&mut self, pane_id: &str) -> Result<Value> {
        let mut result = self.call(
            "pane.process_info",
            dict(&[("pane_id", Value::str(pane_id))]),
            &["pane", "process-info", "--pane", pane_id],
        )?;
        match result.remove("process_info") {
            Some(info @ Value::Dict(_)) => Ok(info),
            _ => Ok(Value::Dict(Map::new())),
        }
    }

    /// `process_info` for each pane, in order.
    ///
    /// Over the socket every request is its own connection anyway, so a
    /// screenful of them goes out on a few connections at once instead of
    /// one after another. Each answer is the one `process_info` gives; a
    /// transport failure on any of them stops trusting the socket afterwards,
    /// as it would have in sequence.
    pub fn process_infos(&mut self, pane_ids: &[String]) -> Vec<Result<Value>> {
        const MAX_CONNECTIONS: usize = 8;
        if pane_ids.len() < 2 || self.socket_only || !self.socket_ok {
            return pane_ids.iter().map(|pane_id| self.process_info(pane_id)).collect();
        }
        let size = pane_ids.len().div_ceil(MAX_CONNECTIONS);
        let (results, socket_ok) = std::thread::scope(|scope| {
            let handles: Vec<_> = pane_ids
                .chunks(size)
                .map(|chunk| {
                    let mut client = self.clone();
                    scope.spawn(move || {
                        let results: Vec<Result<Value>> = chunk.iter().map(|pane_id| client.process_info(pane_id)).collect();
                        (results, client.socket_ok)
                    })
                })
                .collect();
            let mut results = Vec::with_capacity(pane_ids.len());
            let mut socket_ok = true;
            for handle in handles {
                let (chunk, ok) = handle.join().unwrap_or_else(|panic| std::panic::resume_unwind(panic));
                results.extend(chunk);
                socket_ok &= ok;
            }
            (results, socket_ok)
        });
        self.socket_ok &= socket_ok;
        results
    }

    pub fn close_tab(&mut self, tab_id: &str) -> Result<()> {
        self.call("tab.close", dict(&[("tab_id", Value::str(tab_id))]), &["tab", "close", tab_id]).map(|_| ())
    }

    pub fn rename_tab(&mut self, tab_id: &str, label: &str) -> Result<()> {
        self.call(
            "tab.rename",
            dict(&[("tab_id", Value::str(tab_id)), ("label", Value::str(label))]),
            &["tab", "rename", tab_id, label],
        )
        .map(|_| ())
    }

    pub fn focus_workspace(&mut self, workspace_id: &str) -> Result<()> {
        self.call(
            "workspace.focus",
            dict(&[("workspace_id", Value::str(workspace_id))]),
            &["workspace", "focus", workspace_id],
        )
        .map(|_| ())
    }

    pub fn focus_tab(&mut self, tab_id: &str) -> Result<()> {
        self.call("tab.focus", dict(&[("tab_id", Value::str(tab_id))]), &["tab", "focus", tab_id]).map(|_| ())
    }

    pub fn focus_agent(&mut self, target: &str) -> Result<()> {
        self.call("agent.focus", dict(&[("target", Value::str(target))]), &["agent", "focus", target]).map(|_| ())
    }

    /// Every action of every installed plugin, as `plugin.action.list` gives them.
    pub fn plugin_actions(&mut self) -> Result<Vec<Value>> {
        let mut result = self.call("plugin.action.list", dict(&[]), &["plugin", "action", "list"])?;
        match result.remove("actions") {
            Some(Value::List(actions)) => Ok(actions),
            _ => Err(HerdrError::new("plugin.action.list did not return an action list")),
        }
    }

    /// Start a plugin action. Herdr fills its context from the focused tab
    /// and pane, which a popup leaves where they were.
    pub fn invoke_action(&mut self, plugin_id: &str, action_id: &str) -> Result<()> {
        self.call(
            "plugin.action.invoke",
            dict(&[("plugin_id", Value::str(plugin_id)), ("action_id", Value::str(action_id))]),
            &["plugin", "action", "invoke", action_id, "--plugin", plugin_id],
        )
        .map(|_| ())
    }

    /// Close the open popup. The CLI has no such command; without the socket
    /// the popup closes when the bar exits, a moment later.
    pub fn close_popup(&mut self) -> Result<()> {
        if !self.socket_ok {
            return Ok(());
        }
        match self.socket_call("popup.close", &dict(&[])) {
            Ok(response) => HerdrClient::unwrap(response).map(|_| ()),
            Err(Transport::Herdr(error)) => Err(error),
            Err(_) => {
                self.socket_ok = false;
                Ok(())
            }
        }
    }

    /// The CLI's `pane focus` only moves by direction, so this call has no
    /// CLI form. Over the fallback transport the tab focus that jump() does
    /// first already lands on the right tab, so a missing socket degrades to
    /// that instead of erroring.
    pub fn focus_pane(&mut self, pane_id: &str) -> Result<()> {
        if !self.socket_ok {
            return Ok(());
        }
        match self.socket_call("pane.focus", &dict(&[("pane_id", Value::str(pane_id))])) {
            Ok(response) => HerdrClient::unwrap(response).map(|_| ()),
            Err(Transport::Herdr(error)) => Err(error),
            Err(_) => {
                self.socket_ok = false;
                Ok(())
            }
        }
    }
}

#[cfg(unix)]
fn unix_request(path: &str, payload: &[u8], timeout: f64) -> io::Result<Vec<u8>> {
    use std::os::unix::net::UnixStream;
    let mut conn = UnixStream::connect(path)?;
    let limit = Some(Duration::from_secs_f64(timeout.max(0.001)));
    conn.set_read_timeout(limit)?;
    conn.set_write_timeout(limit)?;
    conn.write_all(payload)?;
    // Read straight into the answer, in reads that grow with it: a big
    // snapshot arrives in a handful of reads instead of one per 64 KiB, each
    // of which would also have woken the sender.
    let mut buffered: Vec<u8> = Vec::new();
    let mut scanned = 0;
    loop {
        if buffered[scanned..].contains(&b'\n') {
            break;
        }
        scanned = buffered.len();
        let room = scanned.clamp(65536, 4 << 20);
        buffered.resize(scanned + room, 0);
        let count = match conn.read(&mut buffered[scanned..]) {
            Ok(count) => count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {
                buffered.truncate(scanned);
                continue;
            }
            Err(error) => return Err(error),
        };
        buffered.truncate(scanned + count);
        if count == 0 {
            break;
        }
    }
    let end = buffered.iter().position(|&b| b == b'\n').unwrap_or(buffered.len());
    buffered.truncate(end);
    Ok(buffered)
}

#[cfg(not(unix))]
fn unix_request(_path: &str, _payload: &[u8], _timeout: f64) -> io::Result<Vec<u8>> {
    Err(io::Error::new(io::ErrorKind::Unsupported, "no Unix sockets here"))
}

/// A long-lived `events.subscribe` connection used only as a wake-up.
///
/// The events themselves are not interpreted: whoever waits re-reads the
/// snapshot, so a dropped or coalesced event costs latency, never accuracy.
/// On Windows there is no stream at all -- a named pipe cannot be polled
/// alongside a timeout here -- and `wait` simply sleeps, so the watcher polls
/// at its interval.
pub struct EventStream {
    pub socket_path: Option<String>,
    #[cfg_attr(not(unix), allow(dead_code))]
    pub timeout: f64,
    #[cfg(unix)]
    conn: Option<std::os::unix::net::UnixStream>,
    #[cfg(not(unix))]
    conn: Option<()>,
    subscriptions: Vec<Value>,
    retry_at: Option<Instant>,
    pending: bool,
}

#[cfg_attr(not(unix), allow(dead_code))]
const RETRY: Duration = Duration::from_secs(2);

impl EventStream {
    pub fn new(socket_path: Option<String>, timeout: f64) -> EventStream {
        EventStream { socket_path, timeout, conn: None, subscriptions: Vec::new(), retry_at: None, pending: false }
    }

    /// Keep exactly these subscriptions open; an error on refusal.
    pub fn follow(&mut self, subscriptions: &[Value]) -> Result<()> {
        if self.conn.is_some() && subscriptions == self.subscriptions.as_slice() {
            return Ok(());
        }
        self.close();
        // A refused or dropped stream is retried at polling pace, never in a hot loop.
        let waiting = self.retry_at.map(|at| Instant::now() < at).unwrap_or(false);
        if self.socket_path.as_deref().unwrap_or("").is_empty() || subscriptions.is_empty() || waiting {
            return Ok(());
        }
        #[cfg(unix)]
        {
            match self.subscribe(subscriptions) {
                Ok((conn, pending)) => {
                    self.conn = Some(conn);
                    self.subscriptions = subscriptions.to_vec();
                    self.pending = pending;
                    Ok(())
                }
                Err(error) => {
                    self.retry_at = Some(Instant::now() + RETRY);
                    Err(HerdrError::new(format!("event subscription failed: {}", error.message())))
                }
            }
        }
        #[cfg(not(unix))]
        {
            Ok(())
        }
    }

    #[cfg(unix)]
    fn subscribe(&self, subscriptions: &[Value]) -> std::result::Result<(std::os::unix::net::UnixStream, bool), Transport> {
        use std::os::unix::net::UnixStream;
        let path = self.socket_path.as_deref().unwrap_or("");
        let mut conn = UnixStream::connect(path).map_err(Transport::Os)?;
        let limit = Some(Duration::from_secs_f64(self.timeout.max(0.001)));
        conn.set_read_timeout(limit).map_err(Transport::Os)?;
        conn.set_write_timeout(limit).map_err(Transport::Os)?;
        let request = dict(&[
            ("id", Value::str("bar-events")),
            ("method", Value::str("events.subscribe")),
            ("params", dict(&[("subscriptions", Value::List(subscriptions.to_vec()))])),
        ]);
        let mut payload = pyjson::dumps(&request, false).into_bytes();
        payload.push(b'\n');
        conn.write_all(&payload).map_err(Transport::Os)?;
        let mut buffered: Vec<u8> = Vec::new();
        let mut chunk = vec![0u8; 65536];
        while !buffered.contains(&b'\n') {
            let count = match conn.read(&mut chunk) {
                Ok(count) => count,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(Transport::Os(error)),
            };
            if count == 0 {
                return Err(Transport::Herdr(HerdrError::new("herdr closed the event subscription")));
            }
            buffered.extend_from_slice(&chunk[..count]);
            if buffered.len() > 65536 {
                return Err(Transport::Herdr(HerdrError::new("event subscription response exceeded 64 KiB")));
            }
        }
        let split = buffered.iter().position(|&b| b == b'\n').unwrap();
        let response = String::from_utf8_lossy(&buffered[..split]).into_owned();
        let trailing = &buffered[split + 1..];
        let response = pyjson::loads(&response).map_err(|_| Transport::Value)?;
        HerdrClient::unwrap(response).map_err(Transport::Herdr)?;
        conn.set_nonblocking(true).map_err(Transport::Os)?;
        // bytes.strip(): ASCII whitespace only.
        let pending = trailing.iter().any(|b| !b" \t\n\r\x0b\x0c".contains(b));
        Ok((conn, pending))
    }

    /// Sleep until an event arrives or the timeout passes; true on an event.
    pub fn wait(&mut self, timeout: f64) -> bool {
        #[cfg(unix)]
        {
            use std::os::unix::io::AsRawFd;
            let Some(conn) = self.conn.as_mut() else {
                sleep(timeout);
                return false;
            };
            if self.pending {
                self.pending = false;
                return true;
            }
            let mut poll = libc::pollfd { fd: conn.as_raw_fd(), events: libc::POLLIN, revents: 0 };
            let deadline = Instant::now() + Duration::from_secs_f64(timeout.max(0.0));
            loop {
                let left = deadline.saturating_duration_since(Instant::now());
                let ms = left.as_millis().min(i32::MAX as u128) as i32 + if !left.subsec_nanos().is_multiple_of(1_000_000) { 1 } else { 0 };
                let ready = unsafe { libc::poll(&mut poll, 1, ms) };
                if ready < 0 {
                    if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                        continue;
                    }
                    break;
                }
                if ready == 0 {
                    return false;
                }
                // Bound each wake-up even if events arrive faster than we read.
                // Remaining bytes keep the socket readable for the next wait.
                let mut chunk = vec![0u8; 65536];
                match conn.read(&mut chunk) {
                    Ok(count) if count > 0 => return true,
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => return true,
                    _ => break,
                }
            }
            self.close();
            self.retry_at = Some(Instant::now() + RETRY);
            true
        }
        #[cfg(not(unix))]
        {
            sleep(timeout);
            false
        }
    }

    pub fn close(&mut self) {
        self.conn = None;
        self.subscriptions.clear();
        self.pending = false;
    }
}

pub fn sleep(seconds: f64) {
    if seconds > 0.0 {
        std::thread::sleep(Duration::from_secs_f64(seconds));
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::testutil::{self, TempDir};
    use std::io::{BufRead, BufReader};
    use std::os::unix::net::UnixListener;
    use std::sync::{Arc, Mutex};

    type Requests = Arc<Mutex<Vec<Value>>>;

    /// A stand-in for the Herdr socket: one request per connection. The
    /// responder's None means "never answer" (the connection stays open).
    fn serve(path: &std::path::Path, responder: fn(&Value) -> Option<Value>) -> Requests {
        let listener = UnixListener::bind(path).expect("bind fake herdr socket");
        let requests: Requests = Arc::new(Mutex::new(Vec::new()));
        let seen = requests.clone();
        std::thread::spawn(move || {
            let mut stalled = Vec::new();
            for conn in listener.incoming() {
                let Ok(mut conn) = conn else { return };
                let mut line = String::new();
                if BufReader::new(&conn).read_line(&mut line).is_err() {
                    continue;
                }
                let Ok(request) = pyjson::loads(&line) else { continue };
                seen.lock().unwrap().push(request.clone());
                match responder(&request) {
                    Some(reply) => {
                        let _ = conn.write_all(format!("{}\n", pyjson::dumps(&reply, false)).as_bytes());
                    }
                    None => stalled.push(conn),
                }
            }
        });
        requests
    }

    fn ok(request: &Value) -> Option<Value> {
        let id = request.get("id").clone();
        let result = match request.get("method").as_str() {
            Some("session.snapshot") => {
                dict(&[("type", Value::str("session_snapshot")), ("snapshot", testutil::snapshot())])
            }
            Some("pane.read") => dict(&[
                ("type", Value::str("pane_read")),
                ("read", dict(&[("text", Value::str("hello\nworld"))])),
            ]),
            _ => dict(&[("type", Value::str("ok"))]),
        };
        Some(dict(&[("id", id), ("result", result)]))
    }

    fn client(socket: &std::path::Path, bin: &str) -> HerdrClient {
        let mut client = HerdrClient::new(Some(socket.to_string_lossy().into_owned()), 4.0, false);
        client.bin_path = bin.to_string();
        client
    }

    fn params(text: &str) -> Value {
        pyjson::loads(text).unwrap()
    }

    #[test]
    fn snapshot_round_trip() {
        let _env = testutil::env_lock();
        let path = testutil::socket_path();
        let requests = serve(&path, ok);
        let mut client = client(&path, "/nonexistent");
        assert!(client.socket_ok());
        let snapshot = client.snapshot().unwrap();
        assert_eq!(snapshot.get("tabs").list_or_empty().len(), 8);
        assert_eq!(requests.lock().unwrap()[0].get("method"), &Value::str("session.snapshot"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn pane_read_sends_the_documented_params() {
        let _env = testutil::env_lock();
        let path = testutil::socket_path();
        let requests = serve(&path, ok);
        let mut client = client(&path, "/nonexistent");
        assert_eq!(client.read_pane("w1:p1", 12, "visible").unwrap(), "hello\nworld");
        assert_eq!(
            requests.lock().unwrap()[0].get("params"),
            &params(r#"{"pane_id": "w1:p1", "source": "visible", "lines": 12}"#)
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn focus_calls_use_the_documented_targets() {
        let _env = testutil::env_lock();
        let path = testutil::socket_path();
        let requests = serve(&path, ok);
        let mut client = client(&path, "/nonexistent");
        client.focus_tab("w1:t1").unwrap();
        client.focus_workspace("w1").unwrap();
        client.focus_agent("w1:p1").unwrap();
        client.focus_pane("w1:p2").unwrap();
        let seen: Vec<(Value, Value)> =
            requests.lock().unwrap().iter().map(|r| (r.get("method").clone(), r.get("params").clone())).collect();
        assert_eq!(
            seen,
            vec![
                (Value::str("tab.focus"), params(r#"{"tab_id": "w1:t1"}"#)),
                (Value::str("workspace.focus"), params(r#"{"workspace_id": "w1"}"#)),
                (Value::str("agent.focus"), params(r#"{"target": "w1:p1"}"#)),
                (Value::str("pane.focus"), params(r#"{"pane_id": "w1:p2"}"#)),
            ]
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn tab_rename_sends_the_documented_params() {
        let _env = testutil::env_lock();
        let path = testutil::socket_path();
        let requests = serve(&path, ok);
        let mut client = client(&path, "/nonexistent");
        client.rename_tab("w1:t1", "new panel name").unwrap();
        let requests = requests.lock().unwrap();
        assert_eq!(requests[0].get("method"), &Value::str("tab.rename"));
        assert_eq!(requests[0].get("params"), &params(r#"{"tab_id": "w1:t1", "label": "new panel name"}"#));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn action_calls_send_the_documented_params() {
        let _env = testutil::env_lock();
        let path = testutil::socket_path();
        let requests = serve(&path, |request| {
            let id = request.get("id").clone();
            let result = match request.get("method").as_str() {
                Some("plugin.action.list") => params(
                    r#"{"type": "plugin_action_list", "actions": [{"plugin_id": "p", "action_id": "a", "title": "A"}]}"#,
                ),
                _ => dict(&[("type", Value::str("ok"))]),
            };
            Some(dict(&[("id", id), ("result", result)]))
        });
        let mut client = client(&path, "/nonexistent");
        let actions = client.plugin_actions().unwrap();
        assert_eq!(actions.len(), 1);
        assert_eq!(actions[0].get("title"), &Value::str("A"));
        client.invoke_action("herdr-servers", "open").unwrap();
        client.close_popup().unwrap();
        let seen: Vec<(Value, Value)> =
            requests.lock().unwrap().iter().map(|r| (r.get("method").clone(), r.get("params").clone())).collect();
        assert_eq!(
            seen,
            vec![
                (Value::str("plugin.action.list"), params("{}")),
                (Value::str("plugin.action.invoke"), params(r#"{"plugin_id": "herdr-servers", "action_id": "open"}"#)),
                (Value::str("popup.close"), params("{}")),
            ]
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn server_errors_become_herdr_errors() {
        fn failing(request: &Value) -> Option<Value> {
            Some(dict(&[("id", request.get("id").clone()), ("error", params(r#"{"code": "not_found", "message": "no such tab"}"#))]))
        }
        let _env = testutil::env_lock();
        let path = testutil::socket_path();
        serve(&path, failing);
        let mut client = client(&path, "/nonexistent");
        let error = client.focus_tab("w1:t9").unwrap_err();
        assert!(error.is_herdr());
        assert!(error.message.contains("no such tab"), "{}", error);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn missing_socket_falls_back_to_the_cli() {
        let _env = testutil::env_lock();
        let dir = TempDir::new();
        let mut client = client(&dir.path().join("gone.sock"), "/nonexistent");
        assert!(!client.socket_ok());
        assert!(client.snapshot().unwrap_err().is_herdr());
    }

    #[test]
    fn dead_socket_falls_back_to_the_cli() {
        // The marker is there but nothing answers: a server that went away.
        let _env = testutil::env_lock();
        let dir = TempDir::new();
        let path = dir.path().join("herdr.sock");
        std::fs::write(&path, "").unwrap();
        let mut client = client(&path, "/nonexistent");
        assert!(client.socket_ok());
        let error = client.snapshot().unwrap_err();
        assert!(error.is_herdr());
        assert!(error.message.contains("not found"), "{}", error);
        assert!(!client.socket_ok());
    }

    #[test]
    fn silent_server_times_out_instead_of_hanging() {
        let _env = testutil::env_lock();
        let path = testutil::socket_path();
        serve(&path, |_| None);
        let mut client = client(&path, "/nonexistent");
        client.timeout = 0.3;
        let started = Instant::now();
        assert!(client.snapshot().unwrap_err().is_herdr());
        assert!(started.elapsed() < Duration::from_secs(3));
        assert!(!client.socket_ok());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn socket_only_client_reports_the_timeout() {
        let _env = testutil::env_lock();
        let path = testutil::socket_path();
        serve(&path, |_| None);
        let mut client = HerdrClient::new(Some(path.to_string_lossy().into_owned()), 0.3, true);
        let error = client.snapshot().unwrap_err();
        assert!(error.is_herdr());
        assert!(error.message.contains("timed out"), "{}", error);
        let _ = std::fs::remove_file(&path);
    }

    const SNAPSHOT_JSON: &str =
        r#"{"id":"cli","result":{"type":"session_snapshot","snapshot":{"tabs":[],"agents":[],"workspaces":[]}}}"#;

    /// A herdr stand-in that prints `output` whatever it is asked.
    fn fake_herdr(dir: &TempDir, output: &str) -> String {
        use std::os::unix::fs::PermissionsExt;
        let bin = dir.path().join("fake-herdr");
        std::fs::write(&bin, format!("#!/bin/sh\necho '{}'\n", output)).unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        bin.to_string_lossy().into_owned()
    }

    #[test]
    fn snapshot_over_the_cli() {
        let _env = testutil::env_lock();
        let dir = TempDir::new();
        let bin = fake_herdr(&dir, SNAPSHOT_JSON);
        let mut client = client(&dir.path().join("absent.sock"), &bin);
        assert_eq!(client.snapshot().unwrap().get("tabs"), &Value::List(Vec::new()));
    }

    #[test]
    fn missing_binary_reports_clearly() {
        let _env = testutil::env_lock();
        let dir = TempDir::new();
        let mut client = client(&dir.path().join("absent.sock"), "/nonexistent/herdr");
        let error = client.snapshot().unwrap_err();
        assert!(error.is_herdr());
        assert!(error.message.contains("not found"), "{}", error);
    }

    #[test]
    fn close_popup_degrades_to_a_no_op_over_the_cli() {
        // The CLI has no popup command; the popup closes when the bar exits.
        let _env = testutil::env_lock();
        let dir = TempDir::new();
        let mut client = client(&dir.path().join("absent.sock"), "/nonexistent/herdr");
        assert!(client.close_popup().is_ok());
    }

    #[test]
    fn focus_pane_degrades_to_a_no_op_over_the_cli() {
        // The CLI cannot focus a pane by id, so without the socket the call
        // must quietly leave the user on the already-focused tab.
        let _env = testutil::env_lock();
        let dir = TempDir::new();
        let mut client = client(&dir.path().join("absent.sock"), "/nonexistent/herdr");
        assert_eq!(client.focus_pane("w1:p1"), Ok(()));
    }

    #[test]
    fn unparseable_output_reports_clearly() {
        let _env = testutil::env_lock();
        let dir = TempDir::new();
        let bin = fake_herdr(&dir, "not json");
        let mut client = client(&dir.path().join("absent.sock"), &bin);
        assert!(client.snapshot().unwrap_err().is_herdr());
    }
}
