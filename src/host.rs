//! Host operating system differences.
//!
//! Herdr runs on Linux, macOS and Windows. Everything the bar does differently
//! per platform lives here, so the modules above read the same everywhere:
//!
//! * exclusive file locks (flock on POSIX, a byte-range lock on Windows),
//! * how the detached title worker is started,
//! * the Windows named pipe behind `HERDR_SOCKET_PATH`,
//! * how old a process is.
//!
//! The terminal itself is the one exception: it has a backend per platform
//! under `term`.

use std::fs::File;
use std::io;
use std::process::Command;

pub const WINDOWS: bool = cfg!(windows);

/// Python's `str(OSError)`: "[Errno 2] No such file or directory".
pub fn os_error_str(error: &io::Error) -> String {
    if matches!(error.kind(), io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock) {
        // A socket timeout: Python's socket.timeout says just this.
        return "timed out".into();
    }
    match error.raw_os_error() {
        Some(code) => {
            let text = error.to_string();
            let suffix = format!(" (os error {})", code);
            let message = text.strip_suffix(&suffix).unwrap_or(&text);
            if cfg!(windows) {
                format!("[WinError {}] {}", code, message)
            } else {
                format!("[Errno {}] {}", code, message)
            }
        }
        None => error.to_string(),
    }
}

/// Python's `str(OSError)` for a call that names a file:
/// "[Errno 13] Permission denied: '/some/path'".
pub fn os_error_path(error: &io::Error, path: &std::path::Path) -> String {
    let base = os_error_str(error);
    if error.raw_os_error().is_none() {
        return base;
    }
    format!("{}: {}", base, crate::pyjson::str_repr(&path.to_string_lossy()))
}

// -- locks -------------------------------------------------------------------

/// Take an exclusive lock on an open file; `WouldBlock` when busy.
///
/// The lock lasts as long as the handle: closing the file releases it. POSIX
/// uses flock(2). Windows locks the first byte of the file, which conflicts
/// between processes and between two handles of one process just like flock
/// does, and with what the Python release's msvcrt.locking took.
#[cfg(unix)]
pub fn lock(file: &File, blocking: bool) -> io::Result<()> {
    use std::os::unix::io::AsRawFd;
    let flags = libc::LOCK_EX | if blocking { 0 } else { libc::LOCK_NB };
    loop {
        if unsafe { libc::flock(file.as_raw_fd(), flags) } == 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::Interrupted {
            continue;
        }
        return Err(error);
    }
}

#[cfg(unix)]
pub fn unlock(file: &File) {
    use std::os::unix::io::AsRawFd;
    unsafe {
        libc::flock(file.as_raw_fd(), libc::LOCK_UN);
    }
}

/// Open a lock file or a log for appending, creating it if needed.
///
/// The handle can read too: a Windows lock needs read or write access, and
/// Rust's append-only handle has neither (Python's "a" mode had write).
pub fn open_append(path: &std::path::Path) -> io::Result<File> {
    std::fs::OpenOptions::new().read(true).append(true).create(true).open(path)
}

#[cfg(windows)]
pub fn lock(file: &File, blocking: bool) -> io::Result<()> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::{ERROR_IO_PENDING, ERROR_LOCK_VIOLATION};
    use windows_sys::Win32::Storage::FileSystem::{LockFileEx, LOCKFILE_EXCLUSIVE_LOCK, LOCKFILE_FAIL_IMMEDIATELY};
    use windows_sys::Win32::System::IO::OVERLAPPED;
    loop {
        let mut overlapped: OVERLAPPED = unsafe { std::mem::zeroed() };
        let ok = unsafe {
            LockFileEx(
                file.as_raw_handle() as _,
                LOCKFILE_EXCLUSIVE_LOCK | LOCKFILE_FAIL_IMMEDIATELY,
                0,
                1,
                0,
                &mut overlapped,
            )
        };
        if ok != 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        // Held elsewhere, as std's File::try_lock reads it. Any other error
        // is a failure to report, never a reason to wait.
        let held = matches!(error.raw_os_error(), Some(code) if code == ERROR_LOCK_VIOLATION as i32 || code == ERROR_IO_PENDING as i32);
        if !held {
            return Err(error);
        }
        if !blocking {
            return Err(io::Error::new(io::ErrorKind::WouldBlock, error));
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

#[cfg(windows)]
pub fn unlock(file: &File) {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::UnlockFileEx;
    use windows_sys::Win32::System::IO::OVERLAPPED;
    let mut overlapped: OVERLAPPED = unsafe { std::mem::zeroed() };
    unsafe {
        UnlockFileEx(file.as_raw_handle() as _, 0, 1, 0, &mut overlapped);
    }
}

pub fn is_busy(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::WouldBlock
}

// -- the title worker --------------------------------------------------------

/// Set up a command for a worker that outlives the popup starting it.
///
/// On POSIX the worker inherits the locked descriptor under the same number,
/// so the lock follows the file description into the child and no startup
/// hook can mistake the handoff for a stopped worker. Windows has no fork and
/// cannot pass a locked handle through CreateProcess, so the worker claims
/// the lock file by name as its first act instead; see `title_watcher::watch`.
#[cfg(unix)]
pub fn detach(command: &mut Command, lock_file: &File) {
    use std::os::unix::io::AsRawFd;
    use std::os::unix::process::CommandExt;
    let fd = lock_file.as_raw_fd();
    let limit = fd_limit();
    unsafe {
        command.pre_exec(move || {
            if libc::setsid() < 0 {
                return Err(io::Error::last_os_error());
            }
            // As Python's close_fds: the worker keeps stdio and the lock only.
            cloexec_all_except(fd, limit);
            let flags = libc::fcntl(fd, libc::F_GETFD);
            if flags < 0 || libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

/// One past the highest descriptor to look at; computed before any fork.
#[cfg(unix)]
pub fn fd_limit() -> i32 {
    let mut limit = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
    let soft = if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) } == 0 {
        limit.rlim_cur
    } else {
        1024
    };
    // An unlimited soft limit would make the fallback loop endless.
    soft.clamp(256, 65_536) as i32
}

/// Mark every descriptor from 3 up as close-on-exec, except `keep`.
///
/// Async-signal-safe: no allocation, only fcntl (or close_range on Linux),
/// so it may run between fork and exec. Marking rather than closing keeps
/// std's own exec-error pipe working.
#[cfg(unix)]
pub fn cloexec_all_except(keep: i32, limit: i32) {
    #[cfg(target_os = "linux")]
    unsafe {
        const CLOSE_RANGE_CLOEXEC: libc::c_uint = 1 << 2;
        let mut ok = true;
        if keep > 3 {
            ok &= libc::syscall(libc::SYS_close_range, 3 as libc::c_uint, (keep - 1) as libc::c_uint, CLOSE_RANGE_CLOEXEC) == 0;
        }
        let from = if keep >= 3 { keep + 1 } else { 3 };
        ok = ok && libc::syscall(libc::SYS_close_range, from as libc::c_uint, libc::c_uint::MAX, CLOSE_RANGE_CLOEXEC) == 0;
        if ok {
            return;
        }
    }
    for fd in 3..limit {
        if fd == keep {
            continue;
        }
        unsafe {
            let flags = libc::fcntl(fd, libc::F_GETFD);
            if flags >= 0 && flags & libc::FD_CLOEXEC == 0 {
                libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC);
            }
        }
    }
}

#[cfg(windows)]
pub fn detach(command: &mut Command, _lock_file: &File) {
    use std::os::windows::process::CommandExt;
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    command.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
}

/// The descriptor number a POSIX worker is told to adopt.
#[cfg(unix)]
pub fn handle_number(file: &File) -> i64 {
    use std::os::unix::io::AsRawFd;
    file.as_raw_fd() as i64
}

#[cfg(windows)]
pub fn handle_number(_file: &File) -> i64 {
    -1
}

// -- the socket --------------------------------------------------------------

pub const PIPE_PREFIX: &str = "\\\\.\\pipe\\";

/// The named pipe behind a Windows socket path.
///
/// Herdr serves its Windows API endpoint in the pipe namespace under the
/// socket path string itself: `C:\Users\me\AppData\Roaming\herdr\herdr.sock`
/// is reached at `\\.\pipe\C:\Users\me\AppData\Roaming\herdr\herdr.sock`.
/// The file at the path is only a marker that says which server owns it.
pub fn pipe_name(socket_path: &str) -> String {
    if socket_path.starts_with(PIPE_PREFIX) {
        return socket_path.to_string();
    }
    format!("{}{}", PIPE_PREFIX, socket_path)
}

#[cfg(windows)]
mod pipe {
    use std::io;
    use std::time::{Duration, Instant};
    use windows_sys::Win32::Foundation::{
        CloseHandle, GetLastError, ERROR_BROKEN_PIPE, ERROR_FILE_NOT_FOUND, ERROR_IO_PENDING, ERROR_MORE_DATA,
        ERROR_PIPE_BUSY, GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE, WAIT_OBJECT_0,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, ReadFile, WriteFile, FILE_FLAG_OVERLAPPED, OPEN_EXISTING, SECURITY_IDENTIFICATION,
        SECURITY_SQOS_PRESENT,
    };
    use windows_sys::Win32::System::Pipes::WaitNamedPipeW;
    use windows_sys::Win32::System::Threading::{CreateEventW, WaitForSingleObject};
    use windows_sys::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};

    /// How long a pipe may have no listening instance before the server counts
    /// as gone: Herdr opens the next instance right after accepting a client.
    const PIPE_RELISTEN_GRACE: Duration = Duration::from_millis(250);

    struct Handle(HANDLE);

    impl Drop for Handle {
        fn drop(&mut self) {
            unsafe {
                CloseHandle(self.0);
            }
        }
    }

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(std::iter::once(0)).collect()
    }

    fn remaining_ms(deadline: Instant) -> u32 {
        deadline.saturating_duration_since(Instant::now()).as_millis().min(u32::MAX as u128) as u32
    }

    /// One request over a named pipe; the first reply line, without newline.
    ///
    /// Every step shares one deadline, as a socket timeout bounds each call:
    /// the pipe is opened for overlapped I/O, because a plain read would wait
    /// forever on a server that stopped answering.
    pub fn request(name: &str, payload: &[u8], timeout: f64) -> io::Result<Vec<u8>> {
        let deadline = Instant::now() + Duration::from_secs_f64(timeout.max(0.0));
        let handle = open(name, deadline)?;
        transfer(&handle, true, payload.as_ptr() as *mut u8, payload.len() as u32, deadline)?;
        let mut incoming = vec![0u8; 65536];
        let mut buffered: Vec<u8> = Vec::new();
        while !buffered.contains(&b'\n') {
            let count = transfer(&handle, false, incoming.as_mut_ptr(), incoming.len() as u32, deadline)?;
            if count == 0 {
                break; // the server closed its end
            }
            buffered.extend_from_slice(&incoming[..count as usize]);
        }
        let end = buffered.iter().position(|&b| b == b'\n').unwrap_or(buffered.len());
        buffered.truncate(end);
        Ok(buffered)
    }

    fn open(name: &str, deadline: Instant) -> io::Result<Handle> {
        let grace = Instant::now() + PIPE_RELISTEN_GRACE;
        let flags = FILE_FLAG_OVERLAPPED | SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION;
        let name = wide(name);
        loop {
            let handle = unsafe {
                CreateFileW(
                    name.as_ptr(),
                    GENERIC_READ | GENERIC_WRITE,
                    0,
                    std::ptr::null(),
                    OPEN_EXISTING,
                    flags,
                    std::ptr::null_mut(),
                )
            };
            if handle != INVALID_HANDLE_VALUE {
                return Ok(Handle(handle));
            }
            let code = unsafe { GetLastError() };
            let now = Instant::now();
            if code == ERROR_PIPE_BUSY && now < deadline {
                // Every instance is serving someone; wait for the server to offer one.
                unsafe {
                    WaitNamedPipeW(name.as_ptr(), remaining_ms(deadline).max(1));
                }
            } else if code == ERROR_FILE_NOT_FOUND && now < deadline.min(grace) {
                // Between two instances, or gone for good: only a short wait tells.
                std::thread::sleep(Duration::from_millis(10));
            } else {
                return Err(io::Error::from_raw_os_error(code as i32));
            }
        }
    }

    /// One overlapped ReadFile or WriteFile; bytes moved, 0 once the pipe closed.
    fn transfer(handle: &Handle, write: bool, buffer: *mut u8, size: u32, deadline: Instant) -> io::Result<u32> {
        let event = unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) };
        if event.is_null() {
            return Err(io::Error::last_os_error());
        }
        let event = Handle(event);
        let mut overlapped: OVERLAPPED = unsafe { std::mem::zeroed() };
        overlapped.hEvent = event.0;
        let mut done: u32 = 0;
        let ok = unsafe {
            if write {
                WriteFile(handle.0, buffer, size, std::ptr::null_mut(), &mut overlapped)
            } else {
                ReadFile(handle.0, buffer, size, std::ptr::null_mut(), &mut overlapped)
            }
        };
        if ok == 0 {
            let code = unsafe { GetLastError() };
            if code == ERROR_BROKEN_PIPE {
                return Ok(0);
            }
            if code != ERROR_IO_PENDING {
                return Err(io::Error::from_raw_os_error(code as i32));
            }
            if unsafe { WaitForSingleObject(event.0, remaining_ms(deadline)) } != WAIT_OBJECT_0 {
                unsafe {
                    CancelIoEx(handle.0, &overlapped);
                    // The kernel owns the buffer until the cancelled call lands.
                    GetOverlappedResult(handle.0, &overlapped, &mut done, 1);
                }
                return Err(io::Error::new(io::ErrorKind::TimedOut, "timed out waiting for herdr"));
            }
        }
        if unsafe { GetOverlappedResult(handle.0, &overlapped, &mut done, 0) } == 0 {
            let code = unsafe { GetLastError() };
            if code == ERROR_BROKEN_PIPE {
                return Ok(0);
            }
            if code != ERROR_MORE_DATA {
                // a message-mode pipe hands it over in parts
                return Err(io::Error::from_raw_os_error(code as i32));
            }
        }
        Ok(done)
    }
}

#[cfg(windows)]
pub fn pipe_request(name: &str, payload: &[u8], timeout: f64) -> io::Result<Vec<u8>> {
    pipe::request(name, payload, timeout)
}

#[cfg(not(windows))]
pub fn pipe_request(_name: &str, _payload: &[u8], _timeout: f64) -> io::Result<Vec<u8>> {
    Err(io::Error::new(io::ErrorKind::Unsupported, "named pipes exist only on Windows"))
}

// -- process age -------------------------------------------------------------

/// Seconds since `pid` started, or None if Windows will not say.
///
/// The kernel keeps the creation time of every process; no `ps` needed.
#[cfg(windows)]
pub fn process_age(pid: i128) -> Option<f64> {
    use windows_sys::Win32::Foundation::{CloseHandle, FILETIME};
    use windows_sys::Win32::System::SystemInformation::GetSystemTimeAsFileTime;
    use windows_sys::Win32::System::Threading::{GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};
    let pid: u32 = u32::try_from(pid).ok()?;
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if handle.is_null() {
        return None;
    }
    let zero = FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
    let (mut created, mut exited, mut kernel, mut user) = (zero, zero, zero, zero);
    let ok = unsafe { GetProcessTimes(handle, &mut created, &mut exited, &mut kernel, &mut user) };
    unsafe {
        CloseHandle(handle);
    }
    if ok == 0 {
        return None;
    }
    let mut now = zero;
    unsafe {
        GetSystemTimeAsFileTime(&mut now);
    }
    let value = |t: FILETIME| (t.dwLowDateTime as u64 | ((t.dwHighDateTime as u64) << 32)) as i128;
    // FILETIME counts 100-nanosecond intervals.
    Some(((value(now) - value(created)) as f64 / 1e7).max(0.0))
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;
    use std::time::{Duration, Instant};

    fn open(path: &std::path::Path) -> File {
        open_append(path).unwrap()
    }

    #[test]
    fn second_handle_is_refused_until_the_first_lets_go() {
        let temp = TempDir::new();
        let path = temp.path().join("singleton.lock");
        {
            let (first, second) = (open(&path), open(&path));
            lock(&first, false).unwrap();
            assert!(is_busy(&lock(&second, false).unwrap_err()));
            unlock(&first);
            lock(&second, false).unwrap();
        }
        // Closing released it. A child that another test forks meanwhile holds
        // a copy of every descriptor until its exec closes them, so allow a moment.
        let third = open(&path);
        let deadline = Instant::now() + Duration::from_secs(2);
        while let Err(error) = lock(&third, false) {
            assert!(is_busy(&error) && Instant::now() < deadline, "{}", error);
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn blocking_lock_waits_for_the_holder() {
        let temp = TempDir::new();
        let path = temp.path().join("singleton.lock");
        let (held, is_held) = std::sync::mpsc::channel();
        let holder_path = path.clone();
        let holder = std::thread::spawn(move || {
            let file = open(&holder_path);
            lock(&file, false).unwrap();
            held.send(()).unwrap();
            std::thread::sleep(Duration::from_millis(200));
        });
        is_held.recv_timeout(Duration::from_secs(2)).unwrap();
        let started = Instant::now();
        let waiter = open(&path);
        lock(&waiter, true).unwrap();
        assert!(started.elapsed() > Duration::from_millis(100));
        holder.join().unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn worker_options_detach_from_the_popup() {
        use std::os::unix::io::AsRawFd;
        let temp = TempDir::new();
        let file = open(&temp.path().join("singleton.lock"));
        let fd = file.as_raw_fd();
        // The worker sees the lock descriptor under the same number.
        let mut command = Command::new("/bin/sh");
        command.arg("-c").arg(format!("[ -e /dev/fd/{} ] || exit 3; read line", fd));
        command.stdin(std::process::Stdio::piped());
        detach(&mut command, &file);
        let mut child = command.spawn().unwrap();
        let pid = child.id() as libc::pid_t;
        // A session of its own: it leads it.
        let session = unsafe { libc::getsid(pid) };
        {
            use std::io::Write;
            let mut stdin = child.stdin.take().unwrap();
            stdin.write_all(b"done\n").unwrap();
        }
        let status = child.wait().unwrap();
        assert_eq!(session, pid);
        assert!(status.success(), "{:?}", status);
    }

    #[test]
    fn pipe_name_is_the_socket_path_under_the_pipe_namespace() {
        let path = r"C:\Users\me\AppData\Roaming\herdr\herdr.sock";
        assert_eq!(pipe_name(path), format!("{}{}", r"\\.\pipe\", path));
        assert_eq!(pipe_name(&pipe_name(path)), pipe_name(path));
    }

    #[cfg(windows)]
    #[test]
    fn own_process_has_an_age_and_a_missing_pid_has_none() {
        let age = process_age(std::process::id() as i128).unwrap();
        assert!((0.0..24.0 * 3600.0).contains(&age));
        assert!(process_age(0x7FFFFFF1).is_none());
    }
}
