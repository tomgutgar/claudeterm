//! Everything that differs between Windows and Linux, in one place (as in
//! DevTerminal). Decided with `cfg!(windows)` rather than `#[cfg]`, so both
//! branches compile and pass the tests on any machine. The only exception is
//! the Windows API call, which would not link on Linux.

use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Result, bail};

pub const WIN: bool = cfg!(windows);

fn s(v: &[&str]) -> Vec<String> {
    v.iter().map(|x| x.to_string()).collect()
}

pub fn home() -> PathBuf {
    std::env::home_dir().unwrap_or_default()
}

/// Directory from the variable (XDG, %APPDATA%…) or `~/<default>`.
fn base_dir(var: &str, default: &str) -> PathBuf {
    std::env::var_os(var)
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(default))
}

/// Where claudeterm keeps its own usage history.
pub fn data_dir() -> PathBuf {
    if WIN {
        base_dir("APPDATA", "AppData/Roaming")
    } else {
        base_dir("XDG_DATA_HOME", ".local/share")
    }
}

/// Where the limits written by the statusline are kept.
pub fn cache_dir() -> PathBuf {
    if WIN {
        base_dir("LOCALAPPDATA", "AppData/Local")
    } else {
        base_dir("XDG_CACHE_HOME", ".cache")
    }
}

/// git writes paths with `/` on Windows too; Claude Code uses `\`.
pub fn native_path(p: &str) -> String {
    if WIN {
        p.replace('/', "\\")
    } else {
        p.to_string()
    }
}

/// System UI language, for when there is no `LANG` (Windows doesn't set it):
/// "es" if Windows is in Spanish, otherwise "en".
pub fn ui_lang() -> String {
    (if WIN && win::ui_is_es() { "es" } else { "en" }).to_string()
}

/// Keeps the console window always on top (for `cct --mini`). Windows only;
/// on Linux that's up to the window manager's rules.
pub fn stay_on_top() {
    if WIN {
        win::stay_on_top();
    }
}

/// Is that pid still alive and the same process? Claude Code stores when it
/// started in `procStart`: on Linux, field 22 of /proc/<pid>/stat; on Windows,
/// the creation time (FILETIME in decimal). A reused pid won't match.
pub fn alive(pid: u32, start: &str) -> bool {
    // Without procStart it can't be checked: better to hide it than to offer
    // killing a process that isn't Claude.
    if start.is_empty() {
        return false;
    }
    if WIN {
        return win::create_time(pid).is_some_and(|t| t.to_string() == start);
    }
    let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
        return false;
    };
    // The process name is in parentheses and may contain spaces.
    stat.rsplit_once(')')
        .and_then(|(_, rest)| rest.split_whitespace().nth(19))
        .is_some_and(|s| s == start)
}

/// Is that pid alive and did it start around `unix` (seconds)? For processes
/// that don't store their own start time (agy only logs its pid).
pub fn started_near(pid: u32, unix: i64) -> bool {
    const SLACK: i64 = 120;
    start_unix(pid).is_some_and(|s| (s - unix).abs() <= SLACK)
}

/// When a live process started, Unix seconds.
fn start_unix(pid: u32) -> Option<i64> {
    if WIN {
        // FILETIME: 100 ns since 1601.
        win::create_time(pid).map(|ft| (ft / 10_000_000) as i64 - 11_644_473_600)
    } else {
        // ponytail: assumes 100 clock ticks per second (sysconf(_SC_CLK_TCK) on
        // every mainstream Linux); read it via libc if that ever bites.
        let ticks = std::fs::read_to_string(format!("/proc/{pid}/stat"))
            .ok()
            .and_then(|s| {
                s.rsplit_once(')')?
                    .1
                    .split_whitespace()
                    .nth(19)?
                    .parse::<i64>()
                    .ok()
            });
        let boot = std::fs::read_to_string("/proc/stat").ok().and_then(|s| {
            s.lines()
                .find_map(|l| l.strip_prefix("btime "))?
                .trim()
                .parse::<i64>()
                .ok()
        });
        ticks.zip(boot).map(|(t, b)| b + t / 100)
    }
}

/// Start time (Unix seconds) of every running process whose executable is
/// called one of `names` (without ".exe"). For CLIs that leave no pid on disk.
pub fn procs(names: &[&str]) -> Vec<i64> {
    let pids: Vec<u32> = if WIN {
        win::processes()
            .into_iter()
            .filter(|(_, exe)| {
                let stem = exe.strip_suffix(".exe").unwrap_or(exe);
                names.iter().any(|n| stem.eq_ignore_ascii_case(n))
            })
            .map(|(pid, _)| pid)
            .collect()
    } else {
        let Ok(dir) = std::fs::read_dir("/proc") else {
            return Vec::new();
        };
        dir.flatten()
            .filter_map(|e| e.file_name().to_str()?.parse::<u32>().ok())
            .filter(|pid| {
                // comm is the executable (cut at 15 chars); a Node app that sets
                // process.title shows it as argv[0] instead.
                let comm = std::fs::read_to_string(format!("/proc/{pid}/comm")).unwrap_or_default();
                let argv0 = std::fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default();
                let argv0 = String::from_utf8_lossy(argv0.split(|&b| b == 0).next().unwrap_or(&[]))
                    .rsplit('/')
                    .next()
                    .unwrap_or("")
                    .to_string();
                names.iter().any(|n| comm.trim() == *n || argv0 == *n)
            })
            .collect()
    };
    pids.into_iter().filter_map(start_unix).collect()
}

#[cfg(windows)]
mod win {
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn OpenProcess(access: u32, inherit: i32, pid: u32) -> isize;
        fn GetExitCodeProcess(h: isize, code: *mut u32) -> i32;
        // A FILETIME is two u32 (low, high): in little endian, a u64.
        fn GetProcessTimes(
            h: isize,
            create: *mut u64,
            exit: *mut u64,
            kernel: *mut u64,
            user: *mut u64,
        ) -> i32;
        fn CloseHandle(h: isize) -> i32;
        fn GetUserDefaultUILanguage() -> u16;
        fn GetConsoleWindow() -> isize;
        fn CreateToolhelp32Snapshot(flags: u32, pid: u32) -> isize;
        fn Process32FirstW(snap: isize, entry: *mut ProcessEntry) -> i32;
        fn Process32NextW(snap: isize, entry: *mut ProcessEntry) -> i32;
    }

    /// PROCESSENTRY32W.
    #[repr(C)]
    struct ProcessEntry {
        size: u32,
        usage: u32,
        pid: u32,
        heap: usize,
        module: u32,
        threads: u32,
        parent: u32,
        priority: i32,
        flags: u32,
        exe: [u16; 260],
    }

    /// (pid, executable name) of every process.
    pub fn processes() -> Vec<(u32, String)> {
        const TH32CS_SNAPPROCESS: u32 = 2;
        let mut out = Vec::new();
        // SAFETY: `e` is a local with `size` set as the API requires; the
        // snapshot handle is always closed.
        unsafe {
            let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
            if snap == -1 {
                return out;
            }
            let mut e: ProcessEntry = std::mem::zeroed();
            e.size = size_of::<ProcessEntry>() as u32;
            let mut ok = Process32FirstW(snap, &mut e);
            while ok != 0 {
                let len = e.exe.iter().position(|&c| c == 0).unwrap_or(e.exe.len());
                out.push((e.pid, String::from_utf16_lossy(&e.exe[..len])));
                ok = Process32NextW(snap, &mut e);
            }
            CloseHandle(snap);
        }
        out
    }

    #[link(name = "user32")]
    unsafe extern "system" {
        fn SetWindowPos(
            hwnd: isize,
            after: isize,
            x: i32,
            y: i32,
            cx: i32,
            cy: i32,
            flags: u32,
        ) -> i32;
        fn GetAncestor(hwnd: isize, flags: u32) -> isize;
    }

    /// In Windows Terminal the console window is an invisible pseudo-window
    /// owned by the real one: walk up to it (in conhost it owns itself).
    pub fn stay_on_top() {
        const HWND_TOPMOST: isize = -1;
        // Without NOACTIVATE it would steal focus from whatever you're using (called every 2 s).
        const SWP_NOSIZE_NOMOVE_NOACTIVATE: u32 = 0x1 | 0x2 | 0x10;
        const GA_ROOTOWNER: u32 = 3;
        // SAFETY: no pointers; if there is no console window, it does nothing.
        unsafe {
            let h = GetConsoleWindow();
            if h != 0 {
                let top = GetAncestor(h, GA_ROOTOWNER);
                SetWindowPos(
                    if top != 0 { top } else { h },
                    HWND_TOPMOST,
                    0,
                    0,
                    0,
                    0,
                    SWP_NOSIZE_NOMOVE_NOACTIVATE,
                );
            }
        }
    }

    /// LANGID: the low 10 bits are the language; 0x0a = Spanish.
    pub fn ui_is_es() -> bool {
        // SAFETY: no arguments or pointers.
        unsafe { GetUserDefaultUILanguage() & 0x3ff == 0x0a }
    }

    /// Creation time of a live process.
    pub fn create_time(pid: u32) -> Option<u64> {
        const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
        const STILL_ACTIVE: u32 = 259;
        let (mut create, mut t, mut code) = (0u64, [0u64; 3], 0u32);
        // SAFETY: pointers to local variables; the handle is always closed.
        unsafe {
            let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if h == 0 {
                return None;
            }
            let ok = GetExitCodeProcess(h, &mut code) != 0
                && code == STILL_ACTIVE
                && GetProcessTimes(h, &mut create, &mut t[0], &mut t[1], &mut t[2]) != 0;
            CloseHandle(h);
            ok.then_some(create)
        }
    }
}

#[cfg(not(windows))]
mod win {
    pub fn create_time(_pid: u32) -> Option<u64> {
        None
    }

    pub fn processes() -> Vec<(u32, String)> {
        Vec::new()
    }

    pub fn ui_is_es() -> bool {
        false
    }

    pub fn stay_on_top() {}
}

/// Is `prog` an executable on the PATH (or an existing path)?
fn on_path(prog: &str) -> bool {
    if prog.contains(['/', '\\']) {
        return Path::new(prog).is_file();
    }
    let exts: &[&str] = if WIN { &["", ".exe"] } else { &[""] };
    std::env::var_os("PATH").is_some_and(|path| {
        std::env::split_paths(&path).any(|dir| {
            exts.iter()
                .any(|e| dir.join(format!("{prog}{e}")).is_file())
        })
    })
}

/// The terminal `open_cli` will use, if it isn't installed. Needed because
/// on Linux `setsid -f` returns at once, so a missing terminal would fail
/// silently.
pub fn terminal_missing() -> Option<String> {
    let prog = if WIN {
        "wt".to_string()
    } else {
        std::env::var("CCT_TERMINAL")
            .ok()
            .and_then(|t| t.split_whitespace().next().map(str::to_string))
            .unwrap_or_else(|| "kitty".into())
    };
    (!on_path(&prog)).then_some(prog)
}

/// Opens `prog` (claude or agy) in a new terminal. On Linux, `setsid -f` detaches it (the
/// window survives if you close claudeterm) and the terminal is
/// `$CCT_TERMINAL` (kitty by default; e.g. "alacritty -e" or
/// "gnome-terminal --"). On Windows, Windows Terminal with the cmd profile (its
/// GUID is the same on every install), so it doesn't open in conhost.
pub fn open_cli(prog: &str, cwd: &str, args: &[&str]) -> Vec<String> {
    let mut cmd = if WIN {
        // wt splits its command line on `;` (even inside a path); `\;` escapes it.
        let dir = cwd.replace(';', r"\;");
        s(&[
            "wt",
            "-p",
            "{0caa0dad-35be-5f56-a8ff-afceeeaa6101}",
            "-d",
            &dir,
            "cmd",
            "/k",
            prog,
        ])
    } else {
        // $0 = directory, $1 = program, "$@" = its arguments.
        s(&[
            "setsid",
            "-f",
            "sh",
            "-c",
            r#"cd "$0" && exec ${CCT_TERMINAL:-kitty} "$@""#,
            cwd,
            prog,
        ])
    };
    cmd.extend(s(args));
    cmd
}

pub fn kill(pid: u32) -> Vec<String> {
    let pid = pid.to_string();
    if WIN {
        s(&["taskkill", "/F", "/PID", &pid])
    } else {
        s(&["kill", &pid])
    }
}

/// "<who> is waiting for you" notification, in its own thread so a slow
/// notifier never stalls the UI: notify-send on Linux, a toast with
/// PowerShell's AppID on Windows.
pub fn notify_idle(who: &str, body: String) {
    let title = crate::tr!("{who} espera tu mensaje", "{who} is waiting for you");
    const PS: &str = r"
$m = [Windows.UI.Notifications.ToastNotificationManager, Windows.UI.Notifications, ContentType = WindowsRuntime]
$x = $m::GetTemplateContent([Windows.UI.Notifications.ToastTemplateType]::ToastText02)
$t = $x.GetElementsByTagName('text')
[void]$t.Item(0).AppendChild($x.CreateTextNode($env:CCT_TITLE))
[void]$t.Item(1).AppendChild($x.CreateTextNode($env:CCT_BODY))
$m::CreateToastNotifier('{1AC14E77-02E7-4E5D-B744-2EB1AE5198B7}\WindowsPowerShell\v1.0\powershell.exe').Show([Windows.UI.Notifications.ToastNotification]::new($x))
";
    std::thread::spawn(move || {
        if WIN {
            let _ = Command::new("powershell")
                .args(["-NoProfile", "-NonInteractive", "-Command", PS])
                .env("CCT_TITLE", &title)
                .env("CCT_BODY", body)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
            return;
        }
        let _ = Command::new("notify-send")
            .args(["-a", "claudeterm", &title, &body])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    });
}

/// Copies to the clipboard. clip.exe detects UTF-16LE (with a BOM, the BOM
/// would be copied too). On Linux, Wayland and otherwise X11.
pub fn copy(text: &str) -> Result<()> {
    let (progs, bytes): (&[&[&str]], Vec<u8>) = if WIN {
        (
            &[&["clip"]],
            text.encode_utf16().flat_map(u16::to_le_bytes).collect(),
        )
    } else {
        (
            &[&["wl-copy"], &["xclip", "-selection", "clipboard"]],
            text.as_bytes().to_vec(),
        )
    };
    let mut failed = None;
    for p in progs {
        let Ok(mut child) = Command::new(p[0])
            .args(&p[1..])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        else {
            continue;
        };
        // Dropping stdin closes it so the program sees EOF. BrokenPipe = it
        // exited early; its exit status below says whether it worked.
        if let Some(mut stdin) = child.stdin.take()
            && let Err(e) = stdin.write_all(&bytes)
            && e.kind() != ErrorKind::BrokenPipe
        {
            let _ = child.wait();
            return Err(e.into());
        }
        if child.wait()?.success() {
            return Ok(());
        }
        // E.g. wl-copy on an X11 session: try the next one.
        failed = Some(p[0]);
    }
    if let Some(prog) = failed {
        bail!("{}", crate::tr!("{prog} ha fallado", "{prog} failed"));
    }
    bail!(
        "{}",
        match (WIN, crate::i18n::es()) {
            (true, true) => "falta clip.exe",
            (true, false) => "clip.exe not found",
            (false, true) => "falta wl-copy (wl-clipboard) o xclip",
            (false, false) => "wl-copy (wl-clipboard) or xclip not found",
        }
    )
}
