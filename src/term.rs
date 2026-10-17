//! The terminal the bar draws on: one backend per platform, same surface.
//!
//! `Terminal` owns raw mode and the alternate screen while it lives, reports
//! its size, says when input is waiting and hands over the raw bytes.
//! `detect_appearance` asks for the background color before any of that
//! starts. POSIX has a pty; Windows has a ConPTY console.

use crate::sequences::{appearance_from_reply, enter_sequence, exit_sequence, reply_complete, APPEARANCE_QUERY};

/// shutil.get_terminal_size's environment override and fallback, for when
/// the terminal itself will not say.
fn fallback_size(query: impl Fn() -> Option<(i64, i64)>) -> (i64, i64) {
    let env = |name: &str| std::env::var(name).ok().and_then(|v| crate::pystr::parse_int(&v)).map(|v| v as i64).unwrap_or(0);
    let mut columns = env("COLUMNS");
    let mut lines = env("LINES");
    if columns <= 0 || lines <= 0 {
        let (c, l) = query().unwrap_or((80, 24));
        if columns <= 0 {
            columns = if c != 0 { c } else { 80 };
        }
        if lines <= 0 {
            lines = if l != 0 { l } else { 24 };
        }
    }
    (columns, lines)
}

#[cfg(unix)]
mod backend {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    static RESIZED: AtomicBool = AtomicBool::new(false);

    extern "C" fn on_winch(_signal: libc::c_int) {
        RESIZED.store(true, Ordering::SeqCst);
    }

    pub fn isatty(fd: i32) -> bool {
        unsafe { libc::isatty(fd) == 1 }
    }

    fn tcgetattr(fd: i32) -> Option<libc::termios> {
        let mut mode: libc::termios = unsafe { std::mem::zeroed() };
        if unsafe { libc::tcgetattr(fd, &mut mode) } != 0 {
            return None;
        }
        Some(mode)
    }

    /// tty.setraw as Python 3.12+ spells it, applied with TCSAFLUSH.
    fn setraw(fd: i32, mut mode: libc::termios) -> bool {
        mode.c_iflag &= !(libc::IGNBRK
            | libc::BRKINT
            | libc::IGNPAR
            | libc::PARMRK
            | libc::INPCK
            | libc::ISTRIP
            | libc::INLCR
            | libc::IGNCR
            | libc::ICRNL
            | libc::IXON
            | libc::IXANY
            | libc::IXOFF);
        mode.c_oflag &= !libc::OPOST;
        mode.c_cflag &= !(libc::PARENB | libc::CSIZE);
        mode.c_cflag |= libc::CS8;
        mode.c_lflag &= !(libc::ECHO
            | libc::ECHOE
            | libc::ECHOK
            | libc::ECHONL
            | libc::ICANON
            | libc::IEXTEN
            | libc::ISIG
            | libc::NOFLSH
            | libc::TOSTOP);
        mode.c_cc[libc::VMIN] = 1;
        mode.c_cc[libc::VTIME] = 0;
        unsafe { libc::tcsetattr(fd, libc::TCSAFLUSH, &mode) == 0 }
    }

    pub fn write_all(fd: i32, mut data: &[u8]) -> std::io::Result<()> {
        while !data.is_empty() {
            let written = unsafe { libc::write(fd, data.as_ptr() as *const libc::c_void, data.len()) };
            if written < 0 {
                let error = std::io::Error::last_os_error();
                if error.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(error);
            }
            data = &data[written as usize..];
        }
        Ok(())
    }

    /// Wait for `fd` to be readable; false on timeout or a signal.
    fn readable(fd: i32, timeout: f64) -> bool {
        let mut poll = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
        let ms = (timeout.max(0.0) * 1000.0).ceil().min(i32::MAX as f64) as i32;
        let ready = unsafe { libc::poll(&mut poll, 1, ms) };
        ready > 0
    }

    fn read_fd(fd: i32, size: usize) -> Vec<u8> {
        let mut buffer = vec![0u8; size];
        loop {
            let count = unsafe { libc::read(fd, buffer.as_mut_ptr() as *mut libc::c_void, size) };
            if count < 0 {
                if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                return Vec::new();
            }
            buffer.truncate(count as usize);
            return buffer;
        }
    }

    fn winsize(fd: i32) -> Option<(i64, i64)> {
        let mut size: libc::winsize = unsafe { std::mem::zeroed() };
        if unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, &mut size) } != 0 {
            return None;
        }
        Some((size.ws_col as i64, size.ws_row as i64))
    }

    pub struct Terminal {
        in_fd: i32,
        mouse: bool,
        saved: Option<libc::termios>,
        previous_winch: Option<libc::sighandler_t>,
        out: Vec<u8>,
    }

    impl Terminal {
        pub fn enter(mouse: bool) -> std::io::Result<Terminal> {
            let in_fd = 0;
            let saved = tcgetattr(in_fd).ok_or_else(std::io::Error::last_os_error)?;
            let mut terminal = Terminal { in_fd, mouse, saved: Some(saved), previous_winch: None, out: Vec::new() };
            if !setraw(in_fd, saved) {
                return Err(std::io::Error::last_os_error());
            }
            terminal.write(&enter_sequence(mouse));
            terminal.flush();
            let handler = on_winch as extern "C" fn(libc::c_int) as libc::sighandler_t;
            let previous = unsafe { libc::signal(libc::SIGWINCH, handler) };
            if previous != libc::SIG_ERR {
                terminal.previous_winch = Some(previous);
            }
            Ok(terminal)
        }

        pub fn take_resize(&mut self) -> bool {
            RESIZED.swap(false, Ordering::SeqCst)
        }

        pub fn size(&self) -> (i64, i64) {
            let (columns, rows) = winsize(self.in_fd).unwrap_or_else(|| fallback_size(|| winsize(1)));
            (columns.max(1), rows.max(1))
        }

        pub fn write(&mut self, text: &str) {
            self.out.extend_from_slice(text.as_bytes());
        }

        pub fn flush(&mut self) {
            let data = std::mem::take(&mut self.out);
            let _ = write_all(1, &data);
        }

        /// Whether input is waiting, within `timeout` seconds. A resize
        /// interrupts the wait, so the new size is drawn at once.
        pub fn wait(&mut self, timeout: f64) -> bool {
            readable(self.in_fd, timeout)
        }

        pub fn read(&mut self) -> Vec<u8> {
            read_fd(self.in_fd, 4096)
        }
    }

    impl Drop for Terminal {
        fn drop(&mut self) {
            self.write(&exit_sequence(self.mouse));
            self.flush();
            if let Some(saved) = self.saved {
                unsafe {
                    libc::tcsetattr(self.in_fd, libc::TCSADRAIN, &saved);
                }
            }
            if let Some(previous) = self.previous_winch {
                unsafe {
                    libc::signal(libc::SIGWINCH, previous);
                }
            }
        }
    }

    /// Ask the terminal for its background color via OSC 11.
    ///
    /// "dark", "light", or None when the terminal stays quiet. Must run
    /// before raw-mode input handling starts so the reply cannot be mistaken
    /// for typing.
    pub fn detect_appearance(timeout: f64) -> Option<&'static str> {
        if !isatty(0) || !isatty(1) {
            return None;
        }
        let fd = 0;
        let saved = tcgetattr(fd)?;
        let mut buffer: Vec<u8> = Vec::new();
        let ok = (|| {
            if !setraw(fd, saved) {
                return false;
            }
            if write_all(1, APPEARANCE_QUERY.as_bytes()).is_err() {
                return false;
            }
            let mut parts = 0;
            while parts < 4 {
                if !readable(fd, timeout) {
                    break;
                }
                buffer.extend_from_slice(&read_fd(fd, 256));
                if reply_complete(&buffer) {
                    break;
                }
                parts += 1;
            }
            true
        })();
        unsafe {
            libc::tcsetattr(fd, libc::TCSANOW, &saved);
        }
        if !ok {
            return None;
        }
        appearance_from_reply(&buffer)
    }

    pub fn restore_console() {}
}

#[cfg(windows)]
mod backend {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, TryRecvError};
    use std::sync::{Mutex, OnceLock};
    use std::time::{Duration, Instant};
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::Console::{
        GetConsoleMode, GetConsoleScreenBufferInfo, GetStdHandle, ReadConsoleW, SetConsoleMode,
        CONSOLE_SCREEN_BUFFER_INFO, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentThreadId, OpenThread, THREAD_TERMINATE};
    use windows_sys::Win32::System::IO::CancelSynchronousIo;

    const ENABLE_PROCESSED_INPUT: u32 = 0x0001; // ctrl+c as a signal rather than a byte
    const ENABLE_LINE_INPUT: u32 = 0x0002;
    const ENABLE_ECHO_INPUT: u32 = 0x0004;
    const ENABLE_MOUSE_INPUT: u32 = 0x0010;
    const ENABLE_QUICK_EDIT_MODE: u32 = 0x0040; // drag-select eats the mouse unless this is off
    const ENABLE_EXTENDED_FLAGS: u32 = 0x0080; // required to change QUICK_EDIT at all
    const ENABLE_VIRTUAL_TERMINAL_INPUT: u32 = 0x0200;
    const ENABLE_PROCESSED_OUTPUT: u32 = 0x0001;
    const ENABLE_VIRTUAL_TERMINAL_PROCESSING: u32 = 0x0004;
    const DISABLE_NEWLINE_AUTO_RETURN: u32 = 0x0008;
    const READ_CHARS: usize = 4096;
    const SIZE_POLL: f64 = 0.1;

    fn std_handle(which: u32) -> HANDLE {
        unsafe { GetStdHandle(which) }
    }

    fn mode(handle: HANDLE) -> Option<u32> {
        let mut mode = 0u32;
        if unsafe { GetConsoleMode(handle, &mut mode) } == 0 {
            return None;
        }
        Some(mode)
    }

    fn set_mode(handle: HANDLE, mode: Option<u32>) {
        if let Some(mode) = mode {
            unsafe {
                SetConsoleMode(handle, mode);
            }
        }
    }

    pub fn raw_input_mode(mut mode: u32, mouse: bool) -> u32 {
        mode &= !(ENABLE_PROCESSED_INPUT | ENABLE_LINE_INPUT | ENABLE_ECHO_INPUT | ENABLE_QUICK_EDIT_MODE | ENABLE_MOUSE_INPUT);
        mode |= ENABLE_EXTENDED_FLAGS | ENABLE_VIRTUAL_TERMINAL_INPUT;
        if mouse {
            mode |= ENABLE_MOUSE_INPUT;
        }
        mode
    }

    pub fn vt_output_mode(mode: u32) -> u32 {
        mode | ENABLE_PROCESSED_OUTPUT | ENABLE_VIRTUAL_TERMINAL_PROCESSING | DISABLE_NEWLINE_AUTO_RETURN
    }

    /// The console's own modes: captured once, restored once.
    ///
    /// The appearance query switches to raw input before the bar's Terminal
    /// does, and the console stays raw from then on. Going back to line input
    /// in between would let a key pressed in that gap start a line-mode read
    /// in the reader thread, and only Enter would finish it. Whoever leaves
    /// first -- the Terminal, or the process on its way out -- puts the
    /// original modes back.
    struct Modes {
        saved: Option<(Option<u32>, Option<u32>)>,
    }

    fn modes() -> &'static Mutex<Modes> {
        static MODES: OnceLock<Mutex<Modes>> = OnceLock::new();
        MODES.get_or_init(|| Mutex::new(Modes { saved: None }))
    }

    fn is_console() -> bool {
        mode(std_handle(STD_INPUT_HANDLE)).is_some() && mode(std_handle(STD_OUTPUT_HANDLE)).is_some()
    }

    fn raw(mouse: bool) {
        let mut modes = modes().lock().unwrap();
        if modes.saved.is_none() {
            modes.saved = Some((mode(std_handle(STD_INPUT_HANDLE)), mode(std_handle(STD_OUTPUT_HANDLE))));
        }
        let (saved_in, saved_out) = modes.saved.unwrap();
        if let Some(saved_in) = saved_in {
            set_mode(std_handle(STD_INPUT_HANDLE), Some(raw_input_mode(saved_in, mouse)));
        }
        if let Some(saved_out) = saved_out {
            set_mode(std_handle(STD_OUTPUT_HANDLE), Some(vt_output_mode(saved_out)));
        }
    }

    pub fn restore_console() {
        let mut modes = modes().lock().unwrap();
        if let Some((saved_in, saved_out)) = modes.saved.take() {
            set_mode(std_handle(STD_INPUT_HANDLE), saved_in);
            set_mode(std_handle(STD_OUTPUT_HANDLE), saved_out);
        }
    }

    /// One thread reading the console for the life of the process.
    ///
    /// `ReadConsoleW` blocks without a timeout, so it runs on a thread that
    /// the appearance probe and the bar share: two readers would compete for
    /// the same keystrokes. Bytes typed before the bar is ready are kept in
    /// `pending` rather than dropped.
    struct Reader {
        receiver: Option<Receiver<Vec<u8>>>,
        pending: Vec<u8>,
        closed: bool,
        thread_id: &'static AtomicU32,
    }

    fn reader() -> &'static Mutex<Reader> {
        static READER: OnceLock<Mutex<Reader>> = OnceLock::new();
        static THREAD: AtomicU32 = AtomicU32::new(0);
        READER.get_or_init(|| Mutex::new(Reader { receiver: None, pending: Vec::new(), closed: false, thread_id: &THREAD }))
    }

    impl Reader {
        fn start(&mut self) {
            if self.receiver.is_some() {
                return;
            }
            let (sender, receiver) = channel::<Vec<u8>>();
            self.receiver = Some(receiver);
            let thread_id = self.thread_id;
            let handle = std_handle(STD_INPUT_HANDLE) as usize;
            std::thread::Builder::new()
                .name("herdr-bar-console".into())
                .spawn(move || {
                    thread_id.store(unsafe { GetCurrentThreadId() }, Ordering::SeqCst);
                    let mut buffer = vec![0u16; READ_CHARS];
                    let mut high: Option<u16> = None;
                    loop {
                        let mut count = 0u32;
                        let ok = unsafe {
                            ReadConsoleW(handle as HANDLE, buffer.as_mut_ptr() as _, READ_CHARS as u32, &mut count, std::ptr::null())
                        };
                        if ok == 0 {
                            let _ = sender.send(Vec::new());
                            return;
                        }
                        let mut units: Vec<u16> = high.take().into_iter().collect();
                        units.extend_from_slice(&buffer[..count as usize]);
                        // A character outside the BMP is two UTF-16 units
                        // that one read may split: hold a dangling high half.
                        if let Some(&last) = units.last() {
                            if (0xD800..=0xDBFF).contains(&last) {
                                high = Some(last);
                                units.pop();
                            }
                        }
                        if !units.is_empty() {
                            let text = String::from_utf16_lossy(&units);
                            if sender.send(text.into_bytes()).is_err() {
                                return;
                            }
                        }
                    }
                })
                .ok();
        }

        fn wait(&mut self, timeout: f64) -> bool {
            if !self.pending.is_empty() || self.closed {
                return true;
            }
            let Some(receiver) = &self.receiver else {
                std::thread::sleep(Duration::from_secs_f64(timeout.max(0.0)));
                return false;
            };
            match receiver.recv_timeout(Duration::from_secs_f64(timeout.max(0.0))) {
                Ok(data) => {
                    if data.is_empty() {
                        self.closed = true;
                    }
                    self.pending.extend_from_slice(&data);
                    true
                }
                Err(RecvTimeoutError::Timeout) => false,
                Err(RecvTimeoutError::Disconnected) => {
                    self.closed = true;
                    true
                }
            }
        }

        fn read(&mut self) -> Vec<u8> {
            let mut data = std::mem::take(&mut self.pending);
            let Some(receiver) = &self.receiver else { return data };
            loop {
                match receiver.try_recv() {
                    Ok(chunk) if chunk.is_empty() => {
                        self.closed = true;
                        return data;
                    }
                    Ok(chunk) => data.extend_from_slice(&chunk),
                    Err(TryRecvError::Empty) => return data,
                    Err(TryRecvError::Disconnected) => {
                        self.closed = true;
                        return data;
                    }
                }
            }
        }

        /// Abort the blocked read so the console can go back to its owner.
        fn cancel(&mut self) {
            let id = self.thread_id.load(Ordering::SeqCst);
            if id == 0 {
                return;
            }
            let handle = unsafe { OpenThread(THREAD_TERMINATE, 0, id) };
            if !handle.is_null() {
                unsafe {
                    CancelSynchronousIo(handle);
                    CloseHandle(handle);
                }
            }
        }
    }

    pub fn isatty(fd: i32) -> bool {
        use std::io::IsTerminal;
        match fd {
            0 => std::io::stdin().is_terminal(),
            1 => std::io::stdout().is_terminal(),
            _ => std::io::stderr().is_terminal(),
        }
    }

    fn window_size() -> Option<(i64, i64)> {
        let mut info: CONSOLE_SCREEN_BUFFER_INFO = unsafe { std::mem::zeroed() };
        if unsafe { GetConsoleScreenBufferInfo(std_handle(STD_OUTPUT_HANDLE), &mut info) } == 0 {
            return None;
        }
        let columns = info.srWindow.Right as i64 - info.srWindow.Left as i64 + 1;
        let rows = info.srWindow.Bottom as i64 - info.srWindow.Top as i64 + 1;
        Some((columns, rows))
    }

    fn write_out(data: &[u8]) {
        use std::io::Write;
        let mut out = std::io::stdout().lock();
        let _ = out.write_all(data);
        let _ = out.flush();
    }

    pub struct Terminal {
        mouse: bool,
        known: Option<(i64, i64)>,
        out: Vec<u8>,
    }

    impl Terminal {
        pub fn enter(mouse: bool) -> std::io::Result<Terminal> {
            raw(mouse);
            let mut terminal = Terminal { mouse, known: None, out: Vec::new() };
            terminal.write(&enter_sequence(mouse));
            terminal.flush();
            terminal.known = Some(terminal.size());
            reader().lock().unwrap().start();
            Ok(terminal)
        }

        pub fn take_resize(&mut self) -> bool {
            let current = self.size();
            if Some(current) == self.known {
                return false;
            }
            self.known = Some(current);
            true
        }

        pub fn size(&self) -> (i64, i64) {
            // The input handle has no screen buffer; measure the output's window.
            let (columns, rows) = window_size().unwrap_or_else(|| fallback_size(window_size));
            (columns.max(1), rows.max(1))
        }

        pub fn write(&mut self, text: &str) {
            self.out.extend_from_slice(text.as_bytes());
        }

        pub fn flush(&mut self) {
            let data = std::mem::take(&mut self.out);
            write_out(&data);
        }

        /// True when input is ready; false on timeout or when the size changed.
        pub fn wait(&mut self, timeout: f64) -> bool {
            let deadline = Instant::now() + Duration::from_secs_f64(timeout.max(0.0));
            loop {
                let remaining = deadline.saturating_duration_since(Instant::now()).as_secs_f64();
                if reader().lock().unwrap().wait(SIZE_POLL.min(remaining)) {
                    return true;
                }
                if Some(self.size()) != self.known {
                    return false; // the caller's take_resize() sees the new size
                }
                if remaining <= SIZE_POLL {
                    return false;
                }
            }
        }

        pub fn read(&mut self) -> Vec<u8> {
            reader().lock().unwrap().read()
        }
    }

    impl Drop for Terminal {
        fn drop(&mut self) {
            self.write(&exit_sequence(self.mouse));
            self.flush();
            reader().lock().unwrap().cancel();
            restore_console();
        }
    }

    /// `buffer` without the OSC reply in it, so only typed bytes remain.
    pub fn strip_reply(buffer: &[u8]) -> Vec<u8> {
        let find = |needle: &[u8], from: usize| buffer[from..].windows(needle.len()).position(|w| w == needle).map(|p| p + from);
        let Some(start) = find(b"\x1b]", 0) else { return buffer.to_vec() };
        let ends: Vec<(usize, usize)> =
            [(find(b"\x07", start), 1), (find(b"\x1b\\", start), 2)].into_iter().filter_map(|(e, w)| e.map(|e| (e, w))).collect();
        let Some(&(end, width)) = ends.iter().min() else {
            return buffer[..start].to_vec(); // cut short: nothing typed after it has arrived yet
        };
        let mut out = buffer[..start].to_vec();
        out.extend_from_slice(&buffer[end + width..]);
        out
    }

    /// Ask the terminal for its background color via OSC 11.
    ///
    /// Keys typed meanwhile stay queued for the bar, and the console stays
    /// in raw mode for it; see `Modes`.
    pub fn detect_appearance(timeout: f64) -> Option<&'static str> {
        if !isatty(0) || !isatty(1) || !is_console() {
            return None;
        }
        raw(false);
        let mut buffer: Vec<u8> = Vec::new();
        let mut reader = reader().lock().unwrap();
        reader.start();
        write_out(APPEARANCE_QUERY.as_bytes());
        let deadline = Instant::now() + Duration::from_secs_f64(timeout);
        while Instant::now() < deadline && !reply_complete(&buffer) {
            let left = deadline.saturating_duration_since(Instant::now()).as_secs_f64();
            if !reader.wait(left) || reader.closed {
                break;
            }
            buffer.extend_from_slice(&reader.read());
        }
        // Whatever is not the reply was typed: hand it back for the bar to read.
        let mut pending = strip_reply(&buffer);
        pending.extend_from_slice(&reader.pending);
        reader.pending = pending;
        appearance_from_reply(&buffer)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn typed_bytes_survive_the_appearance_reply() {
            assert_eq!(strip_reply(b"ab\x1b]11;rgb:0000/0000/0000\x07cd"), b"abcd");
            assert_eq!(strip_reply(b"\x1b]11;rgb:ffff/ffff/ffff\x1b\\"), b"");
            assert_eq!(strip_reply(b"x\x1b]11;rgb:00"), b"x");
            assert_eq!(strip_reply(b"plain typing"), b"plain typing");
        }

        #[test]
        fn raw_mode_turns_on_virtual_terminal_input_and_off_echo() {
            let mode = raw_input_mode(ENABLE_LINE_INPUT | ENABLE_ECHO_INPUT | ENABLE_QUICK_EDIT_MODE, true);
            assert_ne!(mode & ENABLE_VIRTUAL_TERMINAL_INPUT, 0);
            assert_ne!(mode & ENABLE_MOUSE_INPUT, 0);
            assert_ne!(mode & ENABLE_EXTENDED_FLAGS, 0);
            for flag in [ENABLE_LINE_INPUT, ENABLE_ECHO_INPUT, ENABLE_QUICK_EDIT_MODE, ENABLE_PROCESSED_INPUT] {
                assert_eq!(mode & flag, 0);
            }
            assert_eq!(raw_input_mode(0, false) & ENABLE_MOUSE_INPUT, 0);
        }
    }
}

pub use backend::{detect_appearance, isatty, restore_console, Terminal};
