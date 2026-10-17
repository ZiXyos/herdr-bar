//! One scenario of each kind, recorded as parity.py's run_bar, run_cli and
//! run_watcher record it.

use crate::machine::{Guard, Machine, Session, SPAWN_LOCK};
use crate::pyjson::{Map, Value};
use crate::screen::Screen;
use std::io::Read;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::process::{ExitStatus, Stdio};
use std::time::{Duration, Instant};

fn num(value: &Value) -> f64 {
    match value {
        Value::Int(i) => *i as f64,
        Value::Float(f) => *f,
        _ => panic!("not a number: {:?}", value),
    }
}

fn strings(value: &Value) -> Vec<String> {
    value.list_or_empty().iter().map(|v| v.as_str().unwrap().to_string()).collect()
}

fn dict(entries: Vec<(&str, Value)>) -> Value {
    Value::Dict(entries.into_iter().map(|(k, v)| (k.to_string(), v)).collect::<Map>())
}

/// `Popen.returncode`: the exit code, or minus the signal.
fn returncode(status: ExitStatus) -> Value {
    Value::Int(status.code().map(i128::from).unwrap_or_else(|| -(status.signal().unwrap_or(0) as i128)))
}

pub fn run(machine: &Value, scenario: &Value) -> Value {
    match scenario.get("kind").as_str().unwrap() {
        "bar" => run_bar(machine, scenario),
        "cli" => run_cli(machine, scenario),
        "watch" => run_watcher(machine, scenario),
        kind => panic!("unknown kind {}", kind),
    }
}

fn run_bar(spec: &Value, scenario: &Value) -> Value {
    let size = match scenario.get("size") {
        Value::Null => spec.get("default_size"),
        size => size,
    };
    let (width, height) = (num(&size.list_or_empty()[0]) as usize, num(&size.list_or_empty()[1]) as usize);
    let settle = num(spec.get("settle"));
    let exit_timeout = num(spec.get("exit_timeout"));
    let machine = Machine::new(spec, scenario);
    let mut session = Session::spawn(machine.command(&strings(scenario.get("args"))), width as u16, height as u16);
    let mut screen = Screen::new(width, height);
    let mut frames = Vec::new();
    let mut fed = 0;
    session.drain(num(spec.get("first_settle")));
    let open = dict(vec![("after", Value::str("open"))]);
    for step in std::iter::once(&open).chain(scenario.get("steps").list_or_empty()) {
        if let Value::List(keys) = step.get("keys") {
            let bytes: Vec<u8> = keys.iter().map(|b| num(b) as u8).collect();
            session.send(&bytes);
            session.drain(settle);
        } else if !step.get("wait").is_null() {
            std::thread::sleep(Duration::from_secs_f64(num(step.get("wait"))));
            session.drain(settle);
        }
        let painted = session.output.len() - fed;
        screen.feed(&String::from_utf8_lossy(&session.output[fed..]));
        fed = session.output.len();
        frames.push(dict(vec![
            ("after", step.get("after").clone()),
            ("bytes", Value::Int(painted as i128)),
            ("screen", Value::List(screen.dump().into_iter().map(Value::Str).collect())),
        ]));
        if session.reap() {
            break;
        }
    }
    let exit = if session.wait_reaped(exit_timeout) {
        Value::Int(libc::WEXITSTATUS(session.status.unwrap()) as i128)
    } else {
        session.send(b"\x03");
        if session.wait_reaped(exit_timeout) {
            Value::str(format!("did not exit; ctrl+c -> {}", session.status.unwrap()))
        } else {
            Value::str("hung")
        }
    };
    session.drain(0.1);
    screen.feed(&String::from_utf8_lossy(&session.output[fed..]));
    let server = machine.server.as_ref();
    dict(vec![
        ("frames", Value::List(frames)),
        ("exit", exit),
        ("printed", Value::Str(screen.main_text.clone())),
        ("modes", Value::List(screen.modes.iter().cloned().map(Value::Str).collect())),
        ("changes", Value::List(server.map(|s| s.changes()).unwrap_or_default())),
        ("reads", Value::List(server.map(|s| s.reads()).unwrap_or_default())),
        ("files", machine.state_files()),
    ])
}

fn run_cli(spec: &Value, scenario: &Value) -> Value {
    let machine = Machine::new(spec, scenario);
    let mut command = machine.command(&strings(scenario.get("args")));
    command.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = {
        let _guard = SPAWN_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        Guard(command.spawn().expect("spawn herdr-bar"))
    };
    fn collect(mut pipe: impl Read + Send + 'static) -> std::thread::JoinHandle<Vec<u8>> {
        std::thread::spawn(move || {
            let mut out = Vec::new();
            let _ = pipe.read_to_end(&mut out);
            out
        })
    }
    let readers = [collect(child.0.stdout.take().unwrap()), collect(child.0.stderr.take().unwrap())];
    let deadline = Instant::now() + Duration::from_secs(30);
    let status = loop {
        if let Some(status) = child.0.try_wait().unwrap() {
            break status;
        }
        assert!(Instant::now() < deadline, "cli scenario timed out");
        std::thread::sleep(Duration::from_millis(5));
    };
    let mut outputs = readers.into_iter().map(|r| String::from_utf8_lossy(&r.join().unwrap()).into_owned());
    let (mut stdout, mut stderr) = (outputs.next().unwrap(), outputs.next().unwrap());
    for mask in strings(scenario.get("mask")) {
        stdout = stdout
            .split('\n')
            .map(|line| if line.starts_with(&mask) { "<masked>" } else { line })
            .collect::<Vec<_>>()
            .join("\n");
    }
    let tmp = machine.tmp.to_str().unwrap();
    stdout = stdout.replace(tmp, "<tmp>");
    stderr = stderr.replace(tmp, "<tmp>");
    dict(vec![
        ("exit", returncode(status)),
        ("stdout", Value::Str(stdout)),
        ("stderr", Value::Str(stderr)),
        ("changes", Value::List(machine.server.as_ref().map(|s| s.changes()).unwrap_or_default())),
        ("files", machine.state_files()),
    ])
}

fn run_watcher(spec: &Value, scenario: &Value) -> Value {
    let machine = Machine::new(spec, scenario);
    let server = machine.server.as_ref().expect("watcher scenarios need Herdr up");
    let directory = machine.watcher_dir();
    std::fs::create_dir_all(&directory).unwrap();
    let log = std::fs::OpenOptions::new().create(true).append(true).open(directory.join("title-watcher.log")).unwrap();
    let mut command = machine.command(&["--watch-titles".to_string(), "-".to_string()]);
    command.current_dir(&directory).stdin(Stdio::null()).stdout(log.try_clone().unwrap()).stderr(log);
    // As the bar's spawn sets it for the watcher it starts.
    command.env("HERDR_PLUGIN_ROOT", &machine.plugin_root);
    unsafe {
        command.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    let mut child = {
        let _guard = SPAWN_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        Guard(command.spawn().expect("spawn herdr-bar"))
    };
    let mut events: Vec<Value> = scenario.get("events").list_or_empty().to_vec();
    let quiet_for = match scenario.get("quiet") {
        Value::Null => 3.0,
        quiet => num(quiet),
    };
    let start = Instant::now();
    let deadline = start + Duration::from_secs(30);
    let (mut last_count, mut last_change) = (usize::MAX, Instant::now());
    while Instant::now() < deadline && child.0.try_wait().unwrap().is_none() {
        if !events.is_empty() && last_change.elapsed().as_secs_f64() > 1.0 {
            let event = events.remove(0);
            server.with_state(|state| apply(&mut state.data, &event));
            server.set_event_interval(Some(0.2));
            last_change = Instant::now();
        }
        let count = server.change_count();
        if count != last_count {
            (last_count, last_change) = (count, Instant::now());
        } else if events.is_empty() && last_change.elapsed().as_secs_f64() > quiet_for {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    server.set_event_interval(None);
    let exited_alone = child.0.try_wait().unwrap().is_some();
    std::fs::write(directory.join("title-watcher.stopped"), "").unwrap();
    let wait_until = Instant::now() + Duration::from_secs_f64(num(spec.get("exit_timeout")));
    let status = loop {
        if let Some(status) = child.0.try_wait().unwrap() {
            break status;
        }
        if Instant::now() >= wait_until {
            let _ = child.0.kill();
            break child.0.wait().unwrap();
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let labels = server.with_state(|state| {
        state.data.get("tabs").list_or_empty().iter().map(|tab| tab.get("label").clone()).collect::<Vec<_>>()
    });
    dict(vec![
        ("exited_alone", Value::Bool(exited_alone)),
        ("exit", returncode(status)),
        ("changes", Value::List(server.changes())),
        ("labels", Value::List(labels)),
        ("files", machine.state_files()),
    ])
}

/// A scenario event: parity.py's set_status / set_label.
fn apply(data: &mut Value, event: &Value) {
    let (list, index, key, value) = match event.get("op").as_str().unwrap() {
        "set_agent_status" => ("agents", event.get("agent"), "agent_status", event.get("status")),
        "set_tab_label" => ("tabs", event.get("tab"), "label", event.get("label")),
        op => panic!("unknown event {}", op),
    };
    let index = num(index) as usize;
    if let Some(Value::List(entries)) = data.as_dict_mut().and_then(|d| d.get_mut(list)) {
        if let Some(entry) = entries.get_mut(index).and_then(Value::as_dict_mut) {
            entry.insert(key, value.clone());
        }
    }
}
