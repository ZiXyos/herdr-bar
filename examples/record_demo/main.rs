//! Film the real herdr-bar binary driving a fake, very busy Herdr session.
//!
//! ```text
//! cargo run --release --features demo --example record_demo             # demo/herdr-bar.mp4 + .gif
//! cargo run --release --features demo --example record_demo -- --no-gif
//! cargo run --release --features demo --example record_demo -- --stills # one PNG per scene, no video
//! ```
//!
//! Nothing is simulated on the bar's side: the release binary runs on a
//! pseudo-terminal against a fake Herdr on a Unix socket (with a fake `ps`
//! dating the panes, as `tests/parity.rs` does), the script types real keys
//! with real pauses, and a sampler photographs the screen model 20 times a
//! second. What the bar takes to draw, it takes on film too. The session is
//! `session.json`; the frames go to ffmpeg as raw rgb24.

#[cfg(not(unix))]
fn main() {
    eprintln!("record_demo needs a Unix pseudo-terminal");
    std::process::exit(1);
}

#[cfg(unix)]
#[allow(dead_code)]
#[path = "../../src/par.rs"]
mod par;
#[cfg(unix)]
#[allow(dead_code)]
#[path = "../../src/pyjson.rs"]
mod pyjson;
#[cfg(unix)]
#[allow(dead_code)]
#[path = "../../src/pystr.rs"]
mod pystr;
#[cfg(unix)]
#[allow(dead_code)]
#[path = "../../tests/parity/screen.rs"]
mod screen;
#[cfg(unix)]
#[allow(dead_code)]
#[path = "../../src/unicode_tables.rs"]
mod unicode_tables;

#[cfg(unix)]
mod herdr;
#[cfg(unix)]
mod paint;
#[cfg(unix)]
mod pty;

#[cfg(unix)]
fn main() {
    unix::main();
}

#[cfg(unix)]
mod unix {
    use crate::herdr::{Herdr, State};
    use crate::paint::{
        sgr_color, Cell, Frame, Painter, CHIP_TTL, COLS, DEFAULT_FG, PANEL_BG, ROWS,
    };
    use crate::pty::Session;
    use crate::pyjson::{self, Value};
    use crate::screen::Screen;
    use std::collections::BTreeMap;
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    const ROOT: &str = env!("CARGO_MANIFEST_DIR");
    const FPS: u32 = 20;
    const GIF_WIDTH: u32 = 1080;
    const GIF_FPS: u32 = 12;
    const CONFIG: &str = r#"{"selection_background": "237", "agent_icons": "font", "colors": {"muted": "244"}, "auto_titles": false, "tab_status": false}"#;

    fn keycap(name: &str) -> &str {
        match name {
            "up" => "↑",
            "down" => "↓",
            "left" => "←",
            "right" => "→",
            "tab" => "⇥",
            "enter" => "⏎",
            "backspace" => "⌫",
            "delete" => "⌦",
            "ctrl+u" => "^U",
            "ctrl+o" => "^O",
            "ctrl+r" => "^R",
            "ctrl+w" => "^W",
            "pgdn" => "⇟",
            "pgup" => "⇞",
            other => other,
        }
    }

    /// The bytes a terminal sends for a key, as src/keys.rs decodes them.
    fn key_bytes(name: &str) -> &'static [u8] {
        match name {
            "up" => b"\x1b[A",
            "down" => b"\x1b[B",
            "right" => b"\x1b[C",
            "left" => b"\x1b[D",
            "tab" => b"\t",
            "enter" => b"\r",
            "backspace" => b"\x7f",
            "delete" => b"\x1b[3~",
            "esc" => b"\x1b",
            "ctrl+u" => b"\x15",
            "ctrl+o" => b"\x0f",
            "ctrl+r" => b"\x12",
            "ctrl+w" => b"\x17",
            "pgup" => b"\x1b[5~",
            "pgdn" => b"\x1b[6~",
            other => panic!("no bytes for key {}", other),
        }
    }

    /// `ps`'s etime: `[[dd-]hh:]mm:ss`.
    fn etime(seconds: i128) -> String {
        let (days, rest) = (seconds / 86400, seconds % 86400);
        let (hours, minutes, secs) = (rest / 3600, rest % 3600 / 60, rest % 60);
        match (days, hours) {
            (0, 0) => format!("{:02}:{:02}", minutes, secs),
            (0, _) => format!("{:02}:{:02}:{:02}", hours, minutes, secs),
            _ => format!("{}-{:02}:{:02}:{:02}", days, hours, minutes, secs),
        }
    }

    /// A short scratch directory: macOS caps socket paths at 104 bytes.
    fn scratch() -> PathBuf {
        let path = PathBuf::from(format!("/tmp/hbdemo-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    fn write(path: &Path, text: &str, executable: bool) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
        if executable {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
    }

    /// A `ps` that answers both forms the bar uses (`-o etime= -p pid` and
    /// `-o pid=,etime= -p a,b,c`), and a silent `fc-match`.
    fn fake_tools(bin: &Path, etimes: &BTreeMap<i128, String>) {
        let table: Vec<String> = etimes
            .iter()
            .map(|(pid, e)| format!("{}) e='{}' ;;", pid, e))
            .collect();
        let ps = format!(
            "#!/bin/sh\nfmt=$2\nlist=$4\nfound=1\nIFS=,\nfor p in $list; do\n  case \"$p\" in {} *) continue ;; esac\n  found=0\n  if [ \"$fmt\" = \"etime=\" ]; then echo \"$e\"; else echo \"$p $e\"; fi\ndone\nexit $found\n",
            table.join(" ")
        );
        write(&bin.join("ps"), &ps, true);
        write(&bin.join("fc-match"), "#!/bin/sh\nexit 1\n", true);
    }

    fn binary() -> PathBuf {
        let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
        let status = Command::new(cargo)
            .args(["build", "--release", "--bin", "herdr-bar"])
            .current_dir(ROOT)
            .status()
            .expect("run cargo build");
        let path = Path::new(ROOT).join("target/release/herdr-bar");
        if !status.success() || !path.exists() {
            panic!(
                "could not build {}; run `cargo build --release` first",
                path.display()
            );
        }
        path
    }

    /// What the camera sees between frames.
    struct Live {
        caption: String,
        chip: Option<(String, Instant)>,
        /// Where each caption starts, by frame index.
        scenes: Vec<(usize, String)>,
    }

    struct Recorder {
        start: Instant,
        /// Script time: every pause is measured from `start`, so the pauses
        /// add up to the old script's however long each key takes to send.
        at: f64,
        live: Arc<Mutex<Live>>,
        frames: Arc<Mutex<Vec<Frame>>>,
        herdr: Herdr,
        session: Option<Session>,
        after: BTreeMap<(String, String), String>,
    }

    impl Recorder {
        fn say(&self, caption: &str) {
            let index = self.frames.lock().unwrap().len();
            let mut live = self.live.lock().unwrap();
            live.caption = caption.to_string();
            live.scenes.push((index, caption.to_string()));
        }

        fn hold(&mut self, seconds: f64) {
            self.at += seconds;
            let target = self.start + Duration::from_secs_f64(self.at);
            let now = Instant::now();
            if target > now {
                std::thread::sleep(target - now);
            }
        }

        fn send(&mut self, bytes: &[u8], chip: &str, dwell: f64) {
            if let Some(session) = &self.session {
                session.send(bytes);
            }
            self.live.lock().unwrap().chip = Some((chip.to_string(), Instant::now()));
            self.hold(dwell);
        }

        fn key(&mut self, name: &str, dwell: f64) {
            self.send(key_bytes(name), keycap(name), dwell);
        }

        fn repeat(&mut self, name: &str, times: usize, dwell: f64) {
            for _ in 0..times {
                self.key(name, dwell);
            }
        }

        fn type_text(&mut self, text: &str, dwell: f64) {
            for c in text.chars() {
                let mut buffer = [0u8; 4];
                self.send(c.encode_utf8(&mut buffer).as_bytes(), &c.to_string(), dwell);
            }
        }

        fn restatus(&self, pane_id: &str, status: &str) {
            let preview = self
                .after
                .get(&(pane_id.to_string(), status.to_string()))
                .cloned();
            self.herdr.set_status(pane_id, status, preview);
        }
    }

    /// The script: the old recorder's scenes, captions, keys and pauses.
    fn perform(r: &mut Recorder, spawn: impl FnOnce() -> Session) {
        r.say("one chord, and the whole session is a list");
        // New: the empty panel first, so the bar is seen opening.
        r.hold(0.4);
        r.session = Some(spawn());
        r.hold(1.3);

        r.say("automatic tab titles · the task once, plus its running time");
        r.hold(1.9);

        r.say("Claude, Codex, Gemini, Pi, Grok and Kimi · one session");
        r.hold(1.8);

        r.say("needs-you first, then done · where you just were floats up");
        r.hold(1.5);

        r.say("↑↓ moves · the right column tails the pane you land on");
        r.repeat("down", 4, 0.5);
        r.hold(0.7);

        r.say("twenty-five rows, and it scrolls");
        r.repeat("down", 7, 0.19);
        r.hold(1.1);

        r.say("type a few letters · fuzzy, and it underlines what matched");
        r.key("ctrl+u", 0.2);
        r.type_text("invo", 0.16);
        r.hold(2.2);

        r.say("search the task title and its project · both stay searchable");
        r.key("ctrl+u", 0.22);
        r.type_text("cache api", 0.15);
        r.hold(2.2);

        r.say("terms match independently · title, repo, branch, agent");
        r.key("ctrl+u", 0.22);
        r.type_text("back harbor", 0.15);
        r.hold(2.0);

        r.say("nothing matching says so, and tells you the way out");
        r.key("ctrl+u", 0.22);
        r.type_text("zzz", 0.15);
        r.hold(1.6);

        r.say("! narrows to the agents waiting on you");
        r.key("ctrl+u", 0.25);
        r.type_text("!", 0.2);
        r.hold(2.1);

        r.say("% is the panes you named · ⏎ focuses that exact pane");
        r.key("ctrl+u", 0.25);
        r.type_text("%", 0.2);
        r.hold(1.7);
        r.repeat("down", 2, 0.6);
        r.hold(0.9);

        r.say("⇥ cycles the scopes · everything, agents, panes, shells, needs you");
        r.key("ctrl+u", 0.25);
        r.repeat("tab", 4, 1.1);
        r.key("tab", 0.8);

        r.say("^r renames the tab behind any row · enter saves, esc keeps");
        r.key("ctrl+r", 0.7);
        r.type_text("retry budget", 0.13);
        r.hold(1.2);
        r.key("enter", 0.8);
        r.hold(1.2);

        r.say("^o folds the preview away when you want the room");
        r.key("ctrl+o", 1.7);
        r.key("ctrl+o", 0.9);

        r.say("and it keeps updating while it is open");
        r.hold(0.8);
        r.restatus("w1:p7", "blocked");
        r.hold(2.3);

        r.say("⌦ closes a tab · the footer asks first, esc calls it off");
        r.key("ctrl+u", 0.25);
        r.type_text("logs", 0.16);
        r.hold(0.9);
        r.key("delete", 1.7);
        r.key("esc", 1.1);

        r.say("⏎ jumps · tab focused, pane focused, popup gone");
        r.key("ctrl+u", 0.22);
        r.type_text("limit", 0.16);
        r.hold(1.3);
        r.key("enter", 0.1);
        r.hold(2.0);
    }

    /// The screen model, the caption and the chip, frozen into one frame.
    fn snapshot(screen: &Screen, live: &Live, outcome: &str) -> Frame {
        let mut cells = Vec::with_capacity(ROWS * COLS);
        for y in 0..ROWS {
            for x in 0..COLS {
                let (c, fg, bg, bold, underline, reverse) = screen.cell(y, x);
                let mut front = sgr_color(fg).unwrap_or(DEFAULT_FG);
                let mut back = sgr_color(bg);
                if reverse {
                    (front, back) = (back.unwrap_or(PANEL_BG), Some(front));
                }
                cells.push(Cell {
                    ch: c.unwrap_or('\0'),
                    fg: front,
                    bg: back,
                    bold,
                    underline,
                });
            }
        }
        let (chip, chip_age) = match &live.chip {
            Some((label, at)) => {
                let age = at.elapsed().as_secs_f32();
                ((age < CHIP_TTL).then(|| label.clone()), age)
            }
            None => (None, 99.0),
        };
        Frame {
            cells,
            caption: live.caption.clone(),
            chip,
            chip_age,
            outcome: outcome.to_string(),
        }
    }

    struct Options {
        out: PathBuf,
        gif: bool,
        stills: bool,
        compact: bool,
    }

    fn options() -> Options {
        let mut options = Options {
            out: Path::new(ROOT).join("demo"),
            gif: true,
            stills: false,
            compact: true,
        };
        let mut args = std::env::args().skip(1);
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--no-gif" => options.gif = false,
                "--stills" => options.stills = true,
                // The README GIF is compact; this is the roomier framing.
                "--roomy" => options.compact = false,
                "--out" => {
                    options.out = PathBuf::from(args.next().expect("--out needs a directory"))
                }
                "-h" | "--help" => {
                    eprintln!("usage: record_demo [--out DIR] [--no-gif] [--stills] [--roomy]");
                    std::process::exit(0);
                }
                other => panic!("unknown argument {}", other),
            }
        }
        options
    }

    fn ffmpeg(args: &[&str]) {
        let status = Command::new("ffmpeg")
            .args(["-y", "-loglevel", "error"])
            .args(args)
            .status()
            .expect("ffmpeg not found");
        assert!(status.success(), "ffmpeg {:?} failed", args);
    }

    /// Pipe frames into one ffmpeg as raw rgb24.
    fn pipe(painter: &mut Painter, frames: &[&Frame], output: &[&str]) {
        let size = format!("{}x{}", painter.width, painter.height);
        let mut child = Command::new("ffmpeg")
            .args([
                "-y",
                "-loglevel",
                "error",
                "-f",
                "rawvideo",
                "-pix_fmt",
                "rgb24",
                "-s",
                &size,
            ])
            .args(["-r", &FPS.to_string(), "-i", "-"])
            .args(output)
            .stdin(Stdio::piped())
            .spawn()
            .expect("ffmpeg not found");
        let mut stdin = child.stdin.take().unwrap();
        for (index, frame) in frames.iter().enumerate() {
            stdin
                .write_all(painter.render(frame))
                .expect("ffmpeg stopped reading");
            if index % 50 == 0 {
                eprint!("\r  rendering {}/{}", index, frames.len());
            }
        }
        drop(stdin);
        eprintln!("\r  rendering {}/{}", frames.len(), frames.len());
        assert!(child.wait().unwrap().success(), "ffmpeg failed");
    }

    fn slug(caption: &str) -> String {
        let mut out = String::new();
        for c in caption.to_lowercase().chars() {
            if c.is_ascii_lowercase() || c.is_ascii_digit() {
                out.push(c);
            } else if !out.ends_with('-') {
                out.push('-');
            }
        }
        out.trim_matches('-')
            .chars()
            .take(44)
            .collect::<String>()
            .trim_end_matches('-')
            .to_string()
    }

    pub fn main() {
        let options = options();
        let binary = binary();
        let session: Value = pyjson::loads(
            &std::fs::read_to_string(Path::new(ROOT).join("examples/record_demo/session.json"))
                .unwrap(),
        )
        .expect("session.json is not JSON");

        // -- the machine ------------------------------------------------------
        let tmp = scratch();
        let (bin, config, state_dir, home) = (
            tmp.join("bin"),
            tmp.join("config"),
            tmp.join("state"),
            tmp.join("home"),
        );
        std::fs::create_dir_all(&home).unwrap();
        let mut pids = BTreeMap::new();
        let mut etimes = BTreeMap::new();
        for (index, pane) in session
            .get("snapshot")
            .get("panes")
            .list_or_empty()
            .iter()
            .enumerate()
        {
            let pane_id = pane.get("pane_id").as_str().unwrap().to_string();
            let pid = 5001 + index as i128;
            if let Some(age) = session.get("ages").get(&pane_id).as_int() {
                etimes.insert(pid, etime(age));
            }
            pids.insert(pane_id, pid);
        }
        fake_tools(&bin, &etimes);
        write(&config.join("config.json"), CONFIG, false);
        // Where the demo has "been" before the bar opened, most recent first.
        let recents: Vec<String> = session
            .get("recents")
            .list_or_empty()
            .iter()
            .enumerate()
            .map(|(i, key)| {
                format!(
                    r#"{{"key": "{}", "title": "", "ts": {}}}"#,
                    key.as_str().unwrap(),
                    1_700_000_000 - i
                )
            })
            .collect();
        write(
            &state_dir.join("recent.json"),
            &format!(r#"{{"version": 1, "entries": [{}]}}"#, recents.join(", ")),
            false,
        );

        let previews: BTreeMap<String, String> = session
            .get("previews")
            .as_dict()
            .unwrap()
            .iter()
            .map(|(k, v)| (k.clone(), v.as_str().unwrap().to_string()))
            .collect();
        let after: BTreeMap<(String, String), String> = session
            .get("previews_after")
            .list_or_empty()
            .iter()
            .map(|e| {
                let s = |k: &str| e.get(k).as_str().unwrap().to_string();
                ((s("pane_id"), s("status")), s("text"))
            })
            .collect();
        let socket = tmp.join("herdr.sock");
        let herdr = Herdr::start(
            &socket,
            State {
                data: session.get("snapshot").clone(),
                previews,
                fallback: session
                    .get("fallback_preview")
                    .as_str()
                    .unwrap_or("")
                    .to_string(),
                pids,
                jumped: None,
            },
        );

        let s = |p: &Path| p.to_str().unwrap().to_string();
        let mut command = Command::new(&binary);
        command.env_clear().current_dir(&home).envs([
            ("PATH", format!("{}:/usr/bin:/bin", s(&bin))),
            ("HOME", s(&home)),
            ("TERM", "xterm-256color".into()),
            ("LANG", "C.UTF-8".into()),
            ("LC_ALL", "C.UTF-8".into()),
            ("HERDR_SOCKET_PATH", s(&socket)),
            ("HERDR_PLUGIN_CONFIG_DIR", s(&config)),
            ("HERDR_PLUGIN_STATE_DIR", s(&state_dir)),
            ("HERDR_BIN_PATH", "/nonexistent/herdr".into()),
        ]);

        // -- the camera -------------------------------------------------------
        let screen = Arc::new(Mutex::new(Screen::new(COLS, ROWS)));
        let live = Arc::new(Mutex::new(Live {
            caption: String::new(),
            chip: None,
            scenes: Vec::new(),
        }));
        let frames: Arc<Mutex<Vec<Frame>>> = Arc::new(Mutex::new(Vec::new()));
        let rolling = Arc::new(AtomicBool::new(true));
        let start = Instant::now();
        let camera = {
            let (screen, live, frames, rolling, state) = (
                screen.clone(),
                live.clone(),
                frames.clone(),
                rolling.clone(),
                herdr.state.clone(),
            );
            std::thread::spawn(move || {
                let mut tick = 0u32;
                while rolling.load(Ordering::SeqCst) {
                    let target = start + Duration::from_secs_f64(tick as f64 / FPS as f64);
                    let now = Instant::now();
                    if target > now {
                        std::thread::sleep(target - now);
                    }
                    let outcome = state
                        .lock()
                        .unwrap()
                        .jumped
                        .clone()
                        .map(|t| format!("jumped to {}", t));
                    let frame = {
                        let screen = screen.lock().unwrap();
                        let live = live.lock().unwrap();
                        snapshot(&screen, &live, outcome.as_deref().unwrap_or(""))
                    };
                    frames.lock().unwrap().push(frame);
                    tick += 1;
                }
            })
        };

        let mut recorder = Recorder {
            start,
            at: 0.0,
            live: live.clone(),
            frames: frames.clone(),
            herdr,
            session: None,
            after,
        };
        let spawn_screen = screen.clone();
        perform(&mut recorder, || {
            Session::spawn(command, COLS as u16, ROWS as u16, spawn_screen)
        });
        rolling.store(false, Ordering::SeqCst);
        camera.join().unwrap();
        let exited = recorder
            .session
            .as_ref()
            .map(|s| s.closed.load(Ordering::SeqCst))
            .unwrap_or(false);
        if recorder.herdr.state.lock().unwrap().jumped.is_none() || !exited {
            eprintln!("warning: the script ended without the bar jumping and exiting");
        }
        drop(recorder.session.take());
        recorder.herdr.close();
        let _ = std::fs::remove_dir_all(&tmp);

        // -- the film ---------------------------------------------------------
        let frames = std::mem::take(&mut *frames.lock().unwrap());
        let scenes = std::mem::take(&mut live.lock().unwrap().scenes);
        let mut painter = Painter::new(options.compact);
        std::fs::create_dir_all(&options.out).unwrap();
        eprintln!(
            "{} frames · {:.1}s · {}x{}",
            frames.len(),
            frames.len() as f64 / FPS as f64,
            painter.width,
            painter.height
        );

        if options.stills {
            for (order, (start, caption)) in scenes.iter().enumerate() {
                let index = (start + (FPS as f64 * 1.1) as usize).min(frames.len() - 1);
                let path =
                    options
                        .out
                        .join(format!("scene-{:02}-{}.png", order + 1, slug(caption)));
                pipe(
                    &mut painter,
                    &[&frames[index]],
                    &["-frames:v", "1", path.to_str().unwrap()],
                );
                eprintln!("{}", path.display());
            }
            return;
        }

        // Lossless first: the GIF palette is built from it, not from h264,
        // whose chroma artefacts turn into palette noise once quantised.
        let master = options.out.join(".herdr-bar.mkv");
        let (master, mp4, gif) = (
            s(&master),
            s(&options.out.join("herdr-bar.mp4")),
            s(&options.out.join("herdr-bar.gif")),
        );
        let all: Vec<&Frame> = frames.iter().collect();
        pipe(&mut painter, &all, &["-c:v", "ffv1", &master]);
        ffmpeg(&[
            "-i",
            &master,
            "-c:v",
            "libx264",
            "-preset",
            "slow",
            "-crf",
            "18",
            "-pix_fmt",
            "yuv420p",
            "-movflags",
            "+faststart",
            &mp4,
        ]);
        eprintln!("{}", mp4);
        if options.gif {
            let palette = s(&options.out.join(".palette.png"));
            let chain = format!("fps={},scale={}:-1:flags=lanczos", GIF_FPS, GIF_WIDTH);
            ffmpeg(&[
                "-i",
                &master,
                "-vf",
                &format!("{},palettegen=stats_mode=diff:max_colors=192", chain),
                &palette,
            ]);
            let use_ = "paletteuse=dither=bayer:bayer_scale=3:diff_mode=rectangle";
            ffmpeg(&[
                "-i",
                &master,
                "-i",
                &palette,
                "-lavfi",
                &format!("{} [x]; [x][1:v] {}", chain, use_),
                "-loop",
                "0",
                &gif,
            ]);
            let _ = std::fs::remove_file(&palette);
            eprintln!("{}", gif);
        }
        let _ = std::fs::remove_file(&master);
    }
}
