//! Antigravity CLI (`agy`): reads `~/.gemini/antigravity-cli` the way data.rs
//! reads `~/.claude`, and fills the same `Store` so the views work for both.
//!
//! What agy leaves on disk (1.3):
//! - `log/cli-<date>.log`: one per process. First line "… process with pid N",
//!   then "workspaceDirs=[…]", "Streaming conversation <id>" and, on a clean
//!   exit, "Language server shutting down".
//! - `conversations/<id>.db`: SQLite whose rows are protobuf blobs without a
//!   published schema. The field numbers below were worked out from real files.
//! - `annotations/<id>.pbtxt`: `title:"…"` of each conversation.
//! - `history.jsonl`: every prompt typed, with its `conversationId`.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use chrono::{DateTime, Local, NaiveDateTime, Utc};
use rusqlite::{Connection, OpenFlags};
use serde_json::Value;

use crate::data::{Live, Rec, Session, Store, Usage, clip};
use crate::platform;

pub fn root() -> PathBuf {
    platform::home().join(".gemini").join("antigravity-cli")
}

/// A session counts as working if its log was written this recently (agy logs
/// every request to the model).
const LOG_BUSY: Duration = Duration::from_secs(4);

/// Whether a step is still running: 3 = done, 4/6/7 = error/cancelled, 0 = no
/// steps. Anything else is in flight (2 = tool running, 8 = generating, 9 seen
/// briefly mid-turn), so unknown statuses count as working too.
fn running(status: i64) -> bool {
    !matches!(status, 0 | 3 | 4 | 6 | 7)
}

#[derive(Default)]
pub struct State {
    /// Each log: its size when parsed and what it says.
    logs: HashMap<PathBuf, (u64, Log)>,
    /// Each conversation db: when it was last read (db or -wal mtime).
    dbs: HashMap<PathBuf, SystemTime>,
    /// Status of the last step of each conversation.
    last_status: HashMap<String, i64>,
}

#[derive(Clone, Default)]
struct Log {
    pid: u32,
    /// When the log was created (its name), Unix seconds: tells a reused pid apart.
    start: i64,
    cwd: String,
    conversation: Option<String>,
    closed: bool,
}

pub fn refresh(st: &mut Store) {
    read_conversations(st);
    st.live = live_sessions(st);
}

/// The `agy` processes running now, with their conversation.
pub fn live_sessions(st: &mut Store) -> Vec<Live> {
    let Ok(entries) = fs::read_dir(st.root.join("log")) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for e in entries.flatten() {
        let path = e.path();
        let Ok(meta) = e.metadata() else { continue };
        let len = meta.len();
        let log = match st.agy.logs.get(&path) {
            Some((l, log)) if *l == len => log.clone(),
            _ => {
                let log = parse_log(&path, &fs::read_to_string(&path).unwrap_or_default());
                st.agy.logs.insert(path.clone(), (len, log.clone()));
                log
            }
        };
        if log.closed || log.pid == 0 || !platform::started_near(log.pid, log.start) {
            continue;
        }
        let session = log
            .conversation
            .clone()
            .unwrap_or_else(|| format!("agy-{}", log.pid));
        let recent = meta
            .modified()
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|d| d < LOG_BUSY);
        let working = st
            .agy
            .last_status
            .get(&session)
            .is_some_and(|&s| running(s));
        // agy names the conversation after its db was last written, and then
        // the db may not change again: re-read the title of live ones.
        if let Some(t) = title(&st.root, &session)
            && let Some(s) = st.sessions.get_mut(&session)
        {
            s.title = t;
        }
        let sess = st.sessions.get(&session);
        out.push(Live {
            pid: log.pid,
            cwd: sess
                .map(|s| s.cwd.clone())
                .filter(|c| !c.is_empty())
                .unwrap_or(log.cwd),
            name: sess.map(|s| s.title.clone()).unwrap_or_default(),
            status: if recent || working { "busy" } else { "idle" }.into(),
            session,
            proc_start: String::new(),
        });
    }
    out.sort_by_key(|l| l.pid);
    out
}

fn parse_log(path: &Path, text: &str) -> Log {
    let mut log = Log {
        start: path
            .file_stem()
            .and_then(|s| s.to_str())
            .and_then(|s| s.strip_prefix("cli-"))
            .and_then(|s| NaiveDateTime::parse_from_str(s, "%Y%m%d_%H%M%S").ok())
            .and_then(|t| t.and_local_timezone(Local).earliest())
            .map(|t| t.timestamp())
            .unwrap_or(0),
        ..Default::default()
    };
    for line in text.lines() {
        if let Some(pid) = after(line, "process with pid ") {
            log.pid = pid.trim().parse().unwrap_or(0);
        } else if let Some(dirs) = after(line, "workspaceDirs=[") {
            log.cwd = first_path(dirs.split(']').next().unwrap_or(""));
        } else if let Some(id) =
            after(line, "Streaming conversation ").or_else(|| after(line, "Created conversation "))
        {
            log.conversation = Some(id.trim().to_string());
        } else if line.contains("Language server shutting down") {
            log.closed = true;
        }
    }
    log
}

fn after<'a>(line: &'a str, marker: &str) -> Option<&'a str> {
    line.find(marker).map(|i| &line[i + marker.len()..])
}

/// "C:\a b C:/a b" → "C:\a b": agy lists each workspace twice, space-separated,
/// and paths may contain spaces, so cut where the second path starts.
fn first_path(dirs: &str) -> String {
    let b = dirs.as_bytes();
    let cut = (1..b.len()).find(|&i| {
        b[i - 1] == b' '
            && (b[i] == b'/' || (b.get(i + 1) == Some(&b':') && b[i].is_ascii_alphabetic()))
    });
    dirs[..cut.map_or(dirs.len(), |c| c - 1)].to_string()
}

fn read_conversations(st: &mut Store) {
    let Ok(entries) = fs::read_dir(st.root.join("conversations")) else {
        return;
    };
    let titles = prompts(&st.root);
    for p in entries.flatten().map(|e| e.path()) {
        if p.extension().is_none_or(|e| e != "db") {
            continue;
        }
        let Some(id) = p.file_stem().and_then(|s| s.to_str()).map(str::to_string) else {
            continue;
        };
        let mtime = |p: &Path| fs::metadata(p).and_then(|m| m.modified()).ok();
        let changed = mtime(&p).max(mtime(&p.with_extension("db-wal")));
        if changed.is_none() || st.agy.dbs.get(&p).copied() == changed {
            continue;
        }
        // Locked mid-write: try again on the next refresh.
        let Some((mut sess, recs, status)) = read_db(&p, &id) else {
            continue;
        };
        st.agy.dbs.insert(p.clone(), changed.unwrap());
        sess.title = title(&st.root, &id)
            .or_else(|| titles.get(&id).cloned())
            .unwrap_or_default();
        st.agy.last_status.insert(id.clone(), status);
        st.recs.retain(|r| r.session != id);
        st.recs.extend(recs);
        st.sessions.insert(id, sess);
    }
}

/// The session, its replies and the status of its last step.
fn read_db(path: &Path, id: &str) -> Option<(Session, Vec<Rec>, i64)> {
    let c = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .ok()?;
    let meta: Vec<u8> = c
        .query_row(
            "select data from trajectory_metadata_blob limit 1",
            [],
            |r| r.get(0),
        )
        .ok()?;
    let cwd = pb::string(&meta, &[1, 1])
        .map(uri_to_path)
        .unwrap_or_default();
    let created = pb::message(&meta, &[2]).and_then(timestamp);

    // When each step started: replies are dated by their step.
    let mut step_ts: HashMap<i64, DateTime<Utc>> = HashMap::new();
    let mut status = 0;
    let mut q = c
        .prepare("select idx, status, metadata from steps order by idx")
        .ok()?;
    let rows = q
        .query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, Option<Vec<u8>>>(2)?,
            ))
        })
        .ok()?;
    for (idx, s, m) in rows.flatten() {
        status = s;
        if let Some(t) = m
            .as_deref()
            .and_then(|m| pb::message(m, &[1]))
            .and_then(timestamp)
        {
            step_ts.insert(idx, t);
        }
    }

    let mut sess = Session {
        id: id.to_string(),
        path: path.to_path_buf(),
        cwd: cwd.clone(),
        branch: pb::string(&meta, &[1, 4]).unwrap_or_default().to_string(),
        last: created,
        ..Default::default()
    };
    let mut recs = Vec::new();
    let mut q = c
        .prepare("select data from gen_metadata order by idx")
        .ok()?;
    let rows = q.query_map([], |r| r.get::<_, Vec<u8>>(0)).ok()?;
    for data in rows.flatten() {
        let Some(m) = pb::message(&data, &[1]) else {
            continue;
        };
        let model = pb::string(m, &[19]).unwrap_or("gemini").to_string();
        let u = pb::message(m, &[4]).unwrap_or_default();
        let usage = Usage {
            input: pb::int(u, &[2]).unwrap_or(0),
            output: pb::int(u, &[3]).unwrap_or(0),
            cache_read: pb::int(u, &[1]).unwrap_or(0),
            ..Default::default()
        };
        let step: Option<i64> = pb::fields(m)
            .filter(|(f, _)| *f == 20)
            .filter_map(|(_, v)| v.bytes())
            .find(|kv| pb::string(kv, &[1]) == Some("last_step_index"))
            .and_then(|kv| pb::string(kv, &[2])?.parse().ok());
        let Some(ts) = step.and_then(|s| step_ts.get(&s).copied()).or(sess.last) else {
            continue;
        };
        sess.last = sess.last.max(Some(ts));
        sess.model = model.clone();
        sess.context = pb::int(m, &[9, 10, 1]).unwrap_or(0);
        recs.push(Rec {
            ts,
            session: id.to_string(),
            agent: None,
            model,
            cwd: cwd.clone(),
            usage,
        });
    }
    // A conversation that never got a reply isn't a session yet.
    if recs.is_empty() {
        sess.last = None;
    }
    Some((sess, recs, status))
}

/// google.protobuf.Timestamp: 1 = seconds, 2 = nanos.
fn timestamp(m: &[u8]) -> Option<DateTime<Utc>> {
    DateTime::from_timestamp(
        pb::int(m, &[1])? as i64,
        pb::int(m, &[2]).unwrap_or(0) as u32,
    )
}

/// "file:///C:/Users/x%20y" → "C:\Users\x y"; "file:///home/x" → "/home/x".
fn uri_to_path(uri: &str) -> String {
    let p = uri.strip_prefix("file://").unwrap_or(uri);
    let mut bytes = Vec::with_capacity(p.len());
    let b = p.as_bytes();
    let mut i = 0;
    while i < b.len() {
        match (
            b[i],
            p.get(i + 1..i + 3)
                .and_then(|h| u8::from_str_radix(h, 16).ok()),
        ) {
            (b'%', Some(v)) => {
                bytes.push(v);
                i += 3;
            }
            (c, _) => {
                bytes.push(c);
                i += 1;
            }
        }
    }
    let p = String::from_utf8_lossy(&bytes).into_owned();
    // "/C:/…" on Windows.
    match p.as_bytes() {
        [b'/', d, b':', ..] if d.is_ascii_alphabetic() => p[1..].replace('/', "\\"),
        _ => p,
    }
}

/// The title agy gave the conversation (`annotations/<id>.pbtxt`).
fn title(root: &Path, id: &str) -> Option<String> {
    let text = fs::read_to_string(root.join("annotations").join(format!("{id}.pbtxt"))).ok()?;
    let line = text.lines().find_map(|l| l.trim().strip_prefix("title:"))?;
    let t = line.trim().trim_matches('"').replace("\\\"", "\"");
    (!t.is_empty()).then(|| clip(&t, 90))
}

/// The first prompt of each conversation, for those without a title.
fn prompts(root: &Path) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for (id, text) in history(root) {
        out.entry(id).or_insert_with(|| clip(&text, 90));
    }
    out
}

/// (conversation, prompt) of every message typed, oldest first.
fn history(root: &Path) -> Vec<(String, String)> {
    fs::read_to_string(root.join("history.jsonl"))
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter_map(|v| {
            let id = v["conversationId"].as_str()?.to_string();
            let text = v["display"].as_str()?.lines().next()?.trim().to_string();
            (!text.is_empty()).then_some((id, text))
        })
        .collect()
}

/// What you asked in that conversation (agy's answers are protobuf without a
/// schema: only the prompts are readable).
pub fn transcript(id: &str) -> String {
    let lines: Vec<String> = history(&root())
        .into_iter()
        .filter(|(c, _)| c == id)
        .map(|(_, t)| format!("> {t}\n"))
        .collect();
    if lines.is_empty() {
        crate::tr!("(sin mensajes)", "(no messages)")
    } else {
        lines.join("\n")
    }
}

/// Just enough of the protobuf wire format to read fields by number.
mod pb {
    pub enum Val<'a> {
        Int(u64),
        Bytes(&'a [u8]),
    }

    impl<'a> Val<'a> {
        pub fn bytes(&self) -> Option<&'a [u8]> {
            match self {
                Val::Bytes(b) => Some(b),
                Val::Int(_) => None,
            }
        }
    }

    fn varint(b: &mut &[u8]) -> Option<u64> {
        let mut v = 0u64;
        for shift in (0..64).step_by(7) {
            let (&x, rest) = b.split_first()?;
            *b = rest;
            v |= u64::from(x & 0x7f) << shift;
            if x < 0x80 {
                return Some(v);
            }
        }
        None
    }

    /// The fields of a message, in order (stops at anything malformed).
    pub fn fields(mut b: &[u8]) -> impl Iterator<Item = (u64, Val<'_>)> {
        std::iter::from_fn(move || {
            let key = varint(&mut b)?;
            let val = match key & 7 {
                0 => Val::Int(varint(&mut b)?),
                1 | 5 => {
                    let n = if key & 7 == 1 { 8 } else { 4 };
                    let (v, rest) = b.split_at_checked(n)?;
                    b = rest;
                    Val::Int(v.iter().rev().fold(0, |a, &x| a << 8 | u64::from(x)))
                }
                2 => {
                    let n = usize::try_from(varint(&mut b)?).ok()?;
                    let (v, rest) = b.split_at_checked(n)?;
                    b = rest;
                    Val::Bytes(v)
                }
                _ => return None,
            };
            Some((key >> 3, val))
        })
    }

    fn get<'a>(b: &'a [u8], path: &[u64]) -> Option<Val<'a>> {
        let (first, rest) = path.split_first()?;
        let v = fields(b).find(|(f, _)| f == first)?.1;
        if rest.is_empty() {
            Some(v)
        } else {
            get(v.bytes()?, rest)
        }
    }

    pub fn message<'a>(b: &'a [u8], path: &[u64]) -> Option<&'a [u8]> {
        get(b, path)?.bytes()
    }

    pub fn string<'a>(b: &'a [u8], path: &[u64]) -> Option<&'a str> {
        std::str::from_utf8(message(b, path)?).ok()
    }

    pub fn int(b: &[u8], path: &[u64]) -> Option<u64> {
        match get(b, path)? {
            Val::Int(v) => Some(v),
            Val::Bytes(_) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protobuf() {
        // {1: {4: 150}, 2: "hi", 3: 7}
        let b = [
            0x0a, 0x03, 0x20, 0x96, 0x01, 0x12, 0x02, b'h', b'i', 0x18, 0x07,
        ];
        assert_eq!(pb::int(&b, &[1, 4]), Some(150));
        assert_eq!(pb::string(&b, &[2]), Some("hi"));
        assert_eq!(pb::int(&b, &[3]), Some(7));
        assert_eq!(pb::int(&b, &[9]), None);
        // Truncated: no panic, just nothing.
        assert_eq!(pb::int(&b[..4], &[1, 4]), None);
    }

    #[test]
    fn paths() {
        assert_eq!(uri_to_path("file:///C:/Users/a%20b/x"), r"C:\Users\a b\x");
        assert_eq!(uri_to_path("file:///home/me/x"), "/home/me/x");
        assert_eq!(first_path(r"C:\a b C:/a b"), r"C:\a b");
        assert_eq!(first_path(r"C:\Users\me"), r"C:\Users\me");
        assert_eq!(first_path("/home/a b /home/a b"), "/home/a b");
    }

    #[test]
    fn log() {
        let text = "I1006 server.go] Starting language server process with pid 6448\n\
            I1006 server.go] Creating CLI server backend: product=antigravity workspaceDirs=[C:\\w C:/w] appDataDir=x\n\
            I1006 server.go] Created conversation 67c9\n\
            I1006 conversation_manager.go] Streaming conversation 67c9\n";
        let l = parse_log(Path::new("cli-20261006_203420.log"), text);
        assert_eq!((l.pid, l.cwd.as_str(), l.closed), (6448, r"C:\w", false));
        assert_eq!(l.conversation.as_deref(), Some("67c9"));
        assert!(l.start > 0);
        let l = parse_log(
            Path::new("x.log"),
            &format!("{text}I1006 Language server shutting down\n"),
        );
        assert!(l.closed);
    }

    /// Generating (8) and running a tool (2) are working; done or failed aren't.
    #[test]
    fn step_status() {
        assert!([2, 8, 9].into_iter().all(running));
        assert!(![0, 3, 4, 6, 7].into_iter().any(running));
    }
}
