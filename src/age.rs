//! How long the process in a pane has been running.
//!
//! A Herdr snapshot carries no clocks, so the only honest answer comes from
//! the operating system. `pane.process_info` names the process living in a
//! pane and `ps` says how old it is (the kernel's process times on Windows).
//! A start time never moves, so one reading per pane is enough for as long as
//! the bar is open; everything after that is arithmetic.

use crate::pyjson::{self, Value};
use crate::pystr;

pub const MINUTE: i128 = 60;
#[cfg_attr(windows, allow(dead_code))]
pub const HOUR: i128 = 60 * MINUTE;
#[cfg_attr(windows, allow(dead_code))]
pub const DAY: i128 = 24 * HOUR;

fn positive_int(value: &Value) -> bool {
    value.as_int().map(|v| v > 0).unwrap_or(false) || matches!(value, Value::BigInt(t) if !t.starts_with('-'))
}

/// The process whose age stands for the pane's.
pub fn pid_for(info: &Value) -> Option<Value> {
    let leader = info.get("foreground_process_group_id");
    let mut pids: Vec<Value> = Vec::new();
    if let Value::List(processes) = info.get("foreground_processes") {
        for process in processes {
            let pid = process.get("pid");
            if pid.is_int() && positive_int(pid) {
                pids.push(pid.clone());
            }
        }
    }
    if leader.is_int() && pids.iter().any(|p| pyjson::py_eq(p, leader)) {
        // The group leader is the command you actually started.
        return Some(leader.clone());
    }
    if let Some(first) = pids.into_iter().next() {
        return Some(first);
    }
    let shell = info.get("shell_pid");
    if shell.is_int() && positive_int(shell) {
        return Some(shell.clone());
    }
    None
}

/// Read ps(1) elapsed time -- `[[dd-]hh:]mm:ss` -- as seconds.
#[cfg_attr(windows, allow(dead_code))]
pub fn parse_etime(text: &str) -> Option<f64> {
    let mut text = pystr::strip(text);
    if text.is_empty() {
        return None;
    }
    let mut days: i128 = 0;
    if let Some((head, rest)) = text.split_once('-') {
        days = pystr::parse_int(head)?;
        text = rest;
    }
    let parts: Vec<&str> = text.split(':').collect();
    if parts.len() > 3 {
        return None;
    }
    let mut numbers = Vec::new();
    for part in parts {
        numbers.push(pystr::parse_int(part)?);
    }
    let mut seconds: i128 = 0;
    for number in numbers {
        if number < 0 {
            return None;
        }
        seconds = seconds * 60 + number;
    }
    let total = days * DAY + seconds;
    if total >= 0 {
        Some(total as f64)
    } else {
        None
    }
}

/// Seconds since `pid` started, or None if it cannot be read.
pub fn elapsed_seconds(pid: &Value) -> Option<f64> {
    #[cfg(windows)]
    {
        crate::host::process_age(pid.as_int()?)
    }
    #[cfg(not(windows))]
    {
        let argv = ["ps".to_string(), "-o".into(), "etime=".into(), "-p".into(), pyjson::py_str(pid)];
        let output = crate::proc::run_capture(&argv, 1.0)?;
        let text: String = output.stdout.iter().map(|&b| if b < 0x80 { b as char } else { '\u{FFFD}' }).collect();
        parse_etime(&text)
    }
}

/// `elapsed_seconds` for many processes, with one `ps` where one will do.
///
/// Plain pids are asked about together (`ps -o pid=,etime= -p a,b,c`) and
/// matched back by pid; a pid ps leaves out is one it could not date, which
/// is what an empty answer for it alone would have meant. Anything ps could
/// reject as a whole list (a bool, a huge number) still goes on its own, and
/// any answer that does not look like the one asked for falls back to one
/// `ps` per pid, so the readings are exactly the ones `elapsed_seconds` gives.
pub fn elapsed_seconds_many(pids: &[Option<Value>]) -> Vec<Option<f64>> {
    let mut results: Vec<Option<f64>> = vec![None; pids.len()];
    #[cfg(not(windows))]
    let batch: Vec<usize> = (0..pids.len())
        .filter(|&i| matches!(pids[i], Some(Value::Int(pid)) if (1..=0x7fff_ffff).contains(&pid)))
        .collect();
    #[cfg(windows)]
    let batch: Vec<usize> = Vec::new();
    let mut alone: Vec<usize> = (0..pids.len()).filter(|i| pids[*i].is_some() && !batch.contains(i)).collect();
    if batch.len() > 1 {
        match batch_etimes(&batch.iter().map(|&i| pids[i].as_ref().and_then(|p| p.as_int()).unwrap_or(0)).collect::<Vec<_>>()) {
            Some(found) => {
                for (&index, seconds) in batch.iter().zip(found) {
                    results[index] = seconds;
                }
            }
            None => alone.extend(batch),
        }
    } else {
        alone.extend(batch);
    }
    alone.sort_unstable();
    for index in alone {
        if let Some(pid) = &pids[index] {
            results[index] = elapsed_seconds(pid);
        }
    }
    results
}

/// One `ps` for several pids; None when the answer cannot be trusted to be
/// the same as asking one at a time (the caller then does exactly that).
#[cfg(not(windows))]
fn batch_etimes(pids: &[i128]) -> Option<Vec<Option<f64>>> {
    let mut list: Vec<String> = Vec::new();
    for pid in pids {
        let text = pid.to_string();
        if !list.contains(&text) {
            list.push(text);
        }
    }
    let argv = ["ps".to_string(), "-o".into(), "pid=,etime=".into(), "-p".into(), list.join(",")];
    let output = match crate::proc::run(&argv, 1.0) {
        Ok(output) => output,
        // Not installed, or no answer in time: no pid gets a reading, which
        // is what asking for each one alone would have given.
        Err(_) => return Some(vec![None; pids.len()]),
    };
    let text: String = output.stdout.iter().map(|&b| if b < 0x80 { b as char } else { '\u{FFFD}' }).collect();
    let mut etimes: Vec<(i128, &str)> = Vec::new();
    for line in text.lines() {
        let mut fields = line.split_ascii_whitespace();
        let (Some(pid), Some(etime), None) = (fields.next(), fields.next(), fields.next()) else {
            if line.trim().is_empty() {
                continue;
            }
            return None;
        };
        let pid: i128 = pid.parse().ok()?;
        if !pids.contains(&pid) || etimes.iter().any(|(seen, _)| *seen == pid) {
            return None;
        }
        etimes.push((pid, etime));
    }
    if etimes.is_empty() && !output.success {
        // Nothing found, or a ps that does not take a list: ask one by one.
        return None;
    }
    Some(
        pids.iter()
            .map(|pid| etimes.iter().find(|(seen, _)| seen == pid).and_then(|(_, etime)| parse_etime(etime)))
            .collect(),
    )
}

#[cfg(windows)]
fn batch_etimes(_pids: &[i128]) -> Option<Vec<Option<f64>>> {
    None
}

/// A running time short enough to sit in a row: 42s, 7m, 2h04m, 3d4h.
pub fn format_age(seconds: f64) -> String {
    let total = seconds.trunc() as i128;
    if total < 0 {
        return String::new();
    }
    if total < MINUTE {
        return format!("{}s", total);
    }
    let minutes = total / MINUTE;
    if minutes < 60 {
        return format!("{}m", minutes);
    }
    let (hours, minutes) = (minutes / 60, minutes % 60);
    if hours < 24 {
        return format!("{}h{:02}m", hours, minutes);
    }
    let (days, hours) = (hours / 24, hours % 24);
    format!("{}d{}h", days, hours)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn batch_reads_match_one_at_a_time() {
        let pids = vec![
            Some(Value::Int(1)),
            None,
            Some(Value::Int(std::process::id() as i128)),
            Some(Value::Int(0x7fff_fffe)),
            Some(Value::Bool(true)),
            Some(Value::Int(1)),
            Some(Value::BigInt("99999999999999999999999999999999999999999".into())),
        ];
        let batched = elapsed_seconds_many(&pids);
        let alone: Vec<Option<f64>> = pids.iter().map(|p| p.as_ref().and_then(elapsed_seconds)).collect();
        assert_eq!(batched.len(), alone.len());
        for (b, a) in batched.iter().zip(&alone) {
            match (b, a) {
                (Some(b), Some(a)) => assert!((b - a).abs() <= 1.0),
                (b, a) => assert_eq!(b, a),
            }
        }
        assert!(batched[0].is_some() && batched[2].is_some());
    }

    fn info(text: &str) -> Value {
        pyjson::loads(text).unwrap()
    }

    #[test]
    fn the_group_leader_wins_over_its_children() {
        let info = info(r#"{"foreground_process_group_id": 200, "foreground_processes": [{"pid": 311}, {"pid": 200}, {"pid": 480}], "shell_pid": 100}"#);
        assert_eq!(pid_for(&info), Some(Value::Int(200)));
    }

    #[test]
    fn the_first_process_stands_in_for_an_unlisted_leader() {
        let info = info(r#"{"foreground_process_group_id": 999, "foreground_processes": [{"pid": 311}], "shell_pid": 100}"#);
        assert_eq!(pid_for(&info), Some(Value::Int(311)));
    }

    #[test]
    fn an_idle_pane_falls_back_to_its_shell() {
        assert_eq!(pid_for(&info(r#"{"foreground_processes": [], "shell_pid": 100}"#)), Some(Value::Int(100)));
    }

    #[test]
    fn nothing_usable_reads_as_no_pid() {
        assert_eq!(pid_for(&info("{}")), None);
        assert_eq!(pid_for(&info(r#"{"shell_pid": 0, "foreground_processes": [{"pid": "x"}]}"#)), None);
    }

    #[test]
    fn minutes_and_seconds() {
        assert_eq!(parse_etime("07:23"), Some(443.0));
    }

    #[test]
    fn hours() {
        assert_eq!(parse_etime("02:04:00"), Some(7440.0));
    }

    #[test]
    fn days() {
        assert_eq!(parse_etime("3-04:05:06"), Some((3 * 86400 + 4 * 3600 + 5 * 60 + 6) as f64));
    }

    #[test]
    fn surrounding_whitespace_is_ignored() {
        assert_eq!(parse_etime("      01:00\n"), Some(60.0));
    }

    #[test]
    fn junk_reads_as_unknown() {
        for text in ["", "   ", "not-a-time", "1:2:3:4", "x-01:00"] {
            assert_eq!(parse_etime(text), None, "{:?}", text);
        }
    }

    #[cfg(unix)]
    #[test]
    fn this_process_has_an_age() {
        let seconds = elapsed_seconds(&Value::Int(std::process::id() as i128));
        assert!(seconds.is_some_and(|s| s >= 0.0), "{:?}", seconds);
    }

    #[cfg(unix)]
    #[test]
    fn a_dead_pid_reads_as_unknown() {
        assert_eq!(elapsed_seconds(&Value::Int(1 << 30)), None);
    }

    #[test]
    fn shapes() {
        assert_eq!(format_age(0.0), "0s");
        assert_eq!(format_age(59.9), "59s");
        assert_eq!(format_age(60.0), "1m");
        assert_eq!(format_age(3599.0), "59m");
        assert_eq!(format_age(3600.0), "1h00m");
        assert_eq!(format_age(7440.0), "2h04m");
        assert_eq!(format_age(86399.0), "23h59m");
        assert_eq!(format_age(86400.0), "1d0h");
        assert_eq!(format_age((4 * 86400 + 3 * 3600) as f64), "4d3h");
    }

    #[test]
    fn it_stays_short_enough_for_a_row() {
        for seconds in [0, 61, 3601, 86401, 400 * 86400] {
            assert!(format_age(seconds as f64).len() <= 6, "{}", seconds);
        }
    }

    #[test]
    fn a_negative_age_shows_nothing() {
        assert_eq!(format_age(-1.0), "");
    }
}
