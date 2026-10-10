//! OpenAI Codex CLI (`codex`): reads `~/.codex` (or `$CODEX_HOME`) the way
//! data.rs reads `~/.claude`, and fills the same `Store`.
//!
//! What codex leaves on disk (checked against openai/codex's source, Oct 2026):
//! - `sessions/YYYY/MM/DD/rollout-<time>-<thread id>.jsonl`: one per thread,
//!   each line `{"timestamp", "type", "payload"}`. `session_meta` opens it (id,
//!   `session_id` = the root thread, cwd, git branch; a subagent also has
//!   `agent_role`/`agent_nickname`), `turn_context` carries the model, and
//!   `event_msg` the events: `token_count` (CUMULATIVE usage of the thread),
//!   `task_started`/`task_complete`/`turn_aborted`, `user_message`…
//!   Rollouts untouched for a week get compressed to `.jsonl.zst`, and
//!   archived ones move to `archived_sessions/`.
//! - `thread-writer-locks/<thread id>.lock`: locked while a codex process has
//!   that thread open. That's how a session is known to be open: codex keeps
//!   no pid on disk.

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use chrono::{DateTime, Utc};
use serde_json::Value;

use crate::data::{Agent, Live, Rec, Store, Usage, clip, new_lines};
use crate::platform;

pub fn root() -> PathBuf {
    std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| platform::home().join(".codex"))
}

/// A session counts as working if its rollout was written this recently.
const BUSY: Duration = Duration::from_secs(4);

#[derive(Default)]
pub struct State {
    /// By thread id: a rollout that gets compressed or archived is the same
    /// thread at another path, and must not be counted twice.
    threads: HashMap<String, Thread>,
    /// Threads open in a codex process, from the last probe of their locks.
    open: Vec<String>,
}

#[derive(Default)]
struct Thread {
    path: PathBuf,
    off: u64,
    /// The root thread: itself, or the session a subagent belongs to.
    session: String,
    /// Cumulative usage on the last `token_count`, to get each reply's share.
    total: Usage,
    /// A turn started and hasn't finished.
    in_turn: bool,
    model: String,
    cwd: String,
}

pub fn refresh(st: &mut Store) {
    let mut files = Vec::new();
    for dir in ["sessions", "archived_sessions"] {
        rollouts(&st.root.join(dir), 4, &mut files);
    }
    for path in files {
        let Some(id) = thread_id(&path) else { continue };
        let zst = path.extension().is_some_and(|e| e == "zst");
        match st.codex.threads.get_mut(&id) {
            // Two copies at once (mid-move): stay with the one being read.
            Some(t) if t.path != path && t.path.exists() => continue,
            // Compressed or archived: the same thread somewhere else. Archived
            // carries on from the same offset; compressed was already read
            // (`off` > 0), so below it isn't read again.
            Some(t) if t.path != path => {
                t.path = path.clone();
                if let Some(s) = st.sessions.get_mut(&id) {
                    s.path = path.clone();
                }
                if let Some(a) = st.agents.get_mut(&id) {
                    a.path = path.clone();
                }
            }
            Some(_) => {}
            None => {
                st.codex.threads.insert(
                    id.clone(),
                    Thread {
                        path: path.clone(),
                        ..Default::default()
                    },
                );
            }
        }
        if zst {
            if st.codex.threads[&id].off == 0 {
                read_zst(st, &id, &path);
            }
        } else {
            let mut off = st.codex.threads[&id].off;
            new_lines(&path, &mut off, |line| ingest(st, &id, line));
            st.codex.threads.get_mut(&id).unwrap().off = off;
        }
        let in_turn = st.codex.threads[&id].in_turn;
        if let Some(a) = st.agents.get_mut(&id) {
            a.mtime = mtime(&path);
            a.done = !in_turn;
        }
    }
    st.codex.open = open_threads(&st.root.join("thread-writer-locks"));
    st.live = live_sessions(st);
}

/// Every `rollout-*.jsonl[.zst]` under `dir`, down to `depth` levels.
fn rollouts(dir: &Path, depth: u8, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for p in entries.flatten().map(|e| e.path()) {
        let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if name.starts_with("rollout-")
            && (name.ends_with(".jsonl") || name.ends_with(".jsonl.zst"))
        {
            out.push(p);
        } else if depth > 0 && p.is_dir() {
            rollouts(&p, depth - 1, out);
        }
    }
}

/// "rollout-2026-10-09T10-00-00-<uuid>.jsonl[.zst]" → the uuid (36 chars).
fn thread_id(path: &Path) -> Option<String> {
    let name = path.file_name()?.to_str()?;
    let stem = name
        .strip_suffix(".zst")
        .unwrap_or(name)
        .strip_suffix(".jsonl")?;
    let id = stem.get(stem.len().checked_sub(36)?..)?;
    (id.bytes().filter(|&b| b == b'-').count() == 4).then(|| id.to_string())
}

fn mtime(p: &Path) -> SystemTime {
    fs::metadata(p)
        .and_then(|m| m.modified())
        .unwrap_or(SystemTime::UNIX_EPOCH)
}

/// A compressed rollout: cold, so it won't change. Read once, whole.
fn read_zst(st: &mut Store, id: &str, path: &Path) {
    let Some(r) = zst_reader(path) else { return };
    for line in r.lines().map_while(Result::ok) {
        ingest(st, id, line.trim_end());
    }
    // Anything non-zero: it's been read.
    st.codex.threads.get_mut(id).unwrap().off = 1;
}

fn zst_reader(path: &Path) -> Option<BufReader<Box<dyn Read>>> {
    let d = ruzstd::decoding::StreamingDecoder::new(BufReader::new(File::open(path).ok()?)).ok()?;
    Some(BufReader::new(Box::new(d)))
}

fn ingest(st: &mut Store, id: &str, line: &str) {
    // Cheap filter before parsing: most lines are model output or tool calls.
    const WANTED: [&str; 8] = [
        "\"session_meta\"",
        "\"turn_context\"",
        "\"token_count\"",
        "\"task_started\"",
        "\"task_complete\"",
        "\"turn_aborted\"",
        "\"user_message\"",
        "\"role\":\"user\"",
    ];
    if !WANTED.iter().any(|w| line.contains(w)) {
        return;
    }
    let Ok(v) = serde_json::from_str::<Value>(line) else {
        return;
    };
    let p = &v["payload"];
    let ts = v["timestamp"]
        .as_str()
        .and_then(|t| t.parse::<DateTime<Utc>>().ok());
    let t = st.codex.threads.get_mut(id).unwrap();
    match (
        v["type"].as_str().unwrap_or(""),
        p["type"].as_str().unwrap_or(""),
    ) {
        ("session_meta", _) => {
            t.session = p["session_id"].as_str().unwrap_or(id).to_string();
            t.cwd = p["cwd"].as_str().unwrap_or("").to_string();
            let session = t.session.clone();
            if session == id {
                let s = st.sessions.entry(id.to_string()).or_default();
                s.id = id.to_string();
                s.path = t.path.clone();
                s.cwd = t.cwd.clone();
                s.branch = v["payload"]["git"]["branch"]
                    .as_str()
                    .or(v["git"]["branch"].as_str())
                    .unwrap_or("")
                    .to_string();
            } else {
                let field = |k: &str| p[k].as_str().filter(|s| !s.is_empty()).map(str::to_string);
                st.agents.insert(
                    id.to_string(),
                    Agent {
                        id: id.to_string(),
                        session,
                        kind: field("agent_role").unwrap_or_else(|| "subagent".into()),
                        desc: field("agent_nickname").unwrap_or_default(),
                        path: t.path.clone(),
                        mtime: mtime(&t.path),
                        done: false,
                    },
                );
            }
        }
        ("turn_context", _) => {
            if let Some(m) = p["model"].as_str() {
                t.model = m.to_string();
            }
        }
        ("event_msg", "task_started") => t.in_turn = true,
        ("event_msg", "task_complete" | "turn_aborted") => t.in_turn = false,
        // Your message: a `response_item` message (every version) or, in older
        // ones, also a `user_message` event.
        ("response_item", "message") | ("event_msg", "user_message") => {
            if let Some(text) = typed(p)
                && let Some(s) = st.sessions.get_mut(id)
                && s.title.is_empty()
            {
                s.title = clip(text.lines().next().unwrap_or(""), 90);
            }
        }
        ("event_msg", "token_count") => {
            let info = &p["info"];
            let (Some(ts), Some(total)) = (ts, usage(&info["total_token_usage"])) else {
                return;
            };
            // Only the growth is new: codex repeats the count on rate-limit updates.
            let mut u = total.minus(&t.total);
            if total.total() < t.total.total() {
                // The count went back (a rollback): trust the last reply alone.
                u = usage(&info["last_token_usage"]).unwrap_or_default();
            }
            t.total = total;
            if u.total() == 0 {
                return;
            }
            let session = if t.session.is_empty() {
                id.to_string()
            } else {
                t.session.clone()
            };
            let agent = (session != id).then(|| id.to_string());
            let (model, cwd) = (t.model.clone(), t.cwd.clone());
            if agent.is_none()
                && let Some(s) = st.sessions.get_mut(id)
            {
                s.last = Some(ts);
                s.model = model.clone();
                s.context = info["last_token_usage"]["total_tokens"]
                    .as_u64()
                    .unwrap_or(0);
                s.window = info["model_context_window"].as_u64().unwrap_or(0);
            }
            st.recs.push(Rec {
                ts,
                session,
                agent,
                model: if model.is_empty() {
                    "gpt".into()
                } else {
                    model
                },
                cwd,
                usage: u,
            });
        }
        _ => {}
    }
}

/// First text of a message's content (`input_text` / `output_text` parts).
fn text_of(content: &Value) -> Option<&str> {
    content
        .as_array()?
        .iter()
        .find_map(|c| c["text"].as_str())
        .map(str::trim)
        .filter(|t| !t.is_empty())
}

/// What you typed, from a user message: not the context codex injects as
/// user messages too (`<environment_context>`, "# AGENTS.md instructions…").
fn typed(p: &Value) -> Option<&str> {
    let text = match p["type"].as_str() {
        Some("user_message") => p["message"].as_str().map(str::trim)?,
        _ if p["role"] == "user" => text_of(&p["content"])?,
        _ => return None,
    };
    (!text.is_empty() && !text.starts_with('<') && !text.starts_with("# AGENTS.md")).then_some(text)
}

/// codex's TokenUsage. `input_tokens` includes the cached ones and
/// `output_tokens` the reasoning ones (OpenAI's convention).
fn usage(v: &Value) -> Option<Usage> {
    let n = |k: &str| v[k].as_u64().unwrap_or(0);
    v.is_object().then(|| Usage {
        input: n("input_tokens").saturating_sub(n("cached_input_tokens")),
        output: n("output_tokens"),
        cache_read: n("cached_input_tokens"),
        ..Default::default()
    })
}

/// The threads a codex process holds right now. Probed under a shared lock of
/// codex's own coordination file: codex takes that one (exclusive) before
/// creating or opening a thread lock, so our probe can never make it fail.
/// If codex is holding it at this very moment, nothing is probed.
fn open_threads(dir: &Path) -> Vec<String> {
    let Ok(coord) = File::open(dir.join(".coordination.lock")) else {
        return Vec::new();
    };
    if coord.try_lock_shared().is_err() {
        return Vec::new();
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|e| {
            let id = e.file_name().to_str()?.strip_suffix(".lock")?.to_string();
            let f = File::open(e.path()).ok()?;
            matches!(f.try_lock_shared(), Err(std::fs::TryLockError::WouldBlock)).then_some(id)
        })
        .collect()
    // `coord` and each probe are unlocked when dropped.
}

/// The codex sessions open right now (subagents are under their session).
pub fn live_sessions(st: &Store) -> Vec<Live> {
    let mut out: Vec<Live> = st
        .codex
        .open
        .iter()
        .filter_map(|id| {
            // A subagent is shown under its session. A lock whose rollout isn't
            // written yet can't be told apart from a subagent starting up, so
            // it waits until it has one.
            // ponytail: a brand-new codex shows up once its rollout exists.
            let t = st.codex.threads.get(id)?;
            if !t.session.is_empty() && t.session != *id {
                return None;
            }
            let s = st.sessions.get(id);
            let busy = t.in_turn || mtime(&t.path).elapsed().is_ok_and(|d| d < BUSY);
            Some(Live {
                pid: 0,
                session: id.clone(),
                cwd: s.map(|s| s.cwd.clone()).unwrap_or_default(),
                name: s.map(|s| s.title.clone()).unwrap_or_default(),
                status: if busy { "busy" } else { "idle" }.into(),
                proc_start: String::new(),
            })
        })
        .collect();
    out.sort_by(|a, b| a.session.cmp(&b.session));
    out
}

/// What was said in that rollout: your messages, codex's answers and the
/// commands it ran.
pub fn transcript(path: &Path) -> String {
    let lines: Box<dyn Iterator<Item = String>> = if path.extension().is_some_and(|e| e == "zst") {
        match zst_reader(path) {
            Some(r) => Box::new(r.lines().map_while(Result::ok)),
            None => Box::new(std::iter::empty()),
        }
    } else {
        match File::open(path) {
            Ok(f) => Box::new(BufReader::new(f).lines().map_while(Result::ok)),
            Err(_) => Box::new(std::iter::empty()),
        }
    };
    let you = crate::i18n::tr("» tú", "» you");
    let mut out = Vec::new();
    for line in lines {
        let Ok(v) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let p = &v["payload"];
        match (
            v["type"].as_str().unwrap_or(""),
            p["type"].as_str().unwrap_or(""),
        ) {
            ("response_item", "message") if p["role"] == "assistant" => {
                if let Some(t) = text_of(&p["content"]) {
                    out.push(format!("« codex: {}", clip(t, 600)));
                }
            }
            ("response_item", "message") => {
                if let Some(t) = typed(p) {
                    out.push(format!("{you}: {}", clip(t, 600)));
                }
            }
            ("response_item", "function_call" | "custom_tool_call" | "local_shell_call") => {
                // Arguments are a JSON string: show the command if there is one.
                let args = p["arguments"]
                    .as_str()
                    .or(p["input"].as_str())
                    .unwrap_or("");
                let parsed: Value = serde_json::from_str(args).unwrap_or_default();
                let what = match (&parsed["cmd"], &parsed["command"]) {
                    (Value::String(c), _) | (_, Value::String(c)) => c.clone(),
                    (_, Value::Array(a)) => a
                        .iter()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join(" "),
                    _ => args.to_string(),
                };
                out.push(format!(
                    "   ⚙ {}: {}",
                    p["name"].as_str().unwrap_or("shell"),
                    clip(what.lines().next().unwrap_or(""), 120)
                ));
            }
            _ => {}
        }
    }
    let skip = out.len().saturating_sub(200);
    if out.is_empty() {
        crate::i18n::tr("(sin mensajes)", "(no messages)").into()
    } else {
        out[skip..].join("\n\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "019a0000-0000-7000-8000-000000000001";
    const SUB: &str = "019a0000-0000-7000-8000-000000000002";

    /// Shaped like codex's own test fixtures (rollout/src/recorder_tests.rs).
    fn meta(id: &str, session: &str, extra: &str) -> String {
        format!(
            r#"{{"timestamp":"2026-10-09T10:00:00.000Z","type":"session_meta","payload":{{"session_id":"{session}","id":"{id}","timestamp":"2026-10-09T10:00:00Z","cwd":"/w","originator":"codex_cli_rs","cli_version":"0.150.0","source":"cli","model_provider":"openai","git":{{"branch":"main"}}{extra}}}}}"#
        )
    }

    fn tokens(ts: &str, input: u64, cached: u64, out: u64) -> String {
        format!(
            r#"{{"timestamp":"{ts}","type":"event_msg","payload":{{"type":"token_count","info":{{"total_token_usage":{{"input_tokens":{input},"cached_input_tokens":{cached},"output_tokens":{out},"reasoning_output_tokens":0,"total_tokens":{}}},"last_token_usage":{{"input_tokens":10,"cached_input_tokens":0,"output_tokens":5,"reasoning_output_tokens":0,"total_tokens":1500}},"model_context_window":272000}},"rate_limits":null}}}}"#,
            input + out
        )
    }

    fn event(kind: &str, extra: &str) -> String {
        format!(
            r#"{{"timestamp":"2026-10-09T10:00:01.000Z","type":"event_msg","payload":{{"type":"{kind}"{extra}}}}}"#
        )
    }

    /// A session with a subagent: cumulative counts become per-reply usage, a
    /// repeated count adds nothing, and the subagent bills to its session.
    #[test]
    fn rollouts_and_subagents() {
        let dir = std::env::temp_dir().join(format!("cct-codex-{}", std::process::id()));
        let day = dir.join("sessions/2026/10/09");
        fs::create_dir_all(&day).unwrap();
        let main = [
            meta(ID, ID, ""),
            r#"{"timestamp":"2026-10-09T10:00:00.500Z","type":"turn_context","payload":{"cwd":"/w","model":"gpt-5.5-codex"}}"#.into(),
            event("task_started", ""),
            // codex 0.162: the context it injects, then what you typed.
            r#"{"timestamp":"2026-10-09T10:00:01.000Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"<environment_context>\n  <cwd>/w</cwd>\n</environment_context>"}]}}"#.into(),
            r#"{"timestamp":"2026-10-09T10:00:01.100Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"arregla el test\nque falla"}]}}"#.into(),
            tokens("2026-10-09T10:00:02Z", 1000, 800, 50),
            tokens("2026-10-09T10:00:03Z", 1000, 800, 50),
            tokens("2026-10-09T10:00:04Z", 1500, 1200, 80),
        ];
        fs::write(
            day.join(format!("rollout-2026-10-09T10-00-00-{ID}.jsonl")),
            main.join("\n") + "\n",
        )
        .unwrap();
        let sub_file = day.join(format!("rollout-2026-10-09T10-00-05-{SUB}.jsonl"));
        let sub = [
            meta(
                SUB,
                ID,
                r#","agent_role":"explorer","agent_nickname":"Halley""#,
            ),
            event("task_started", ""),
            tokens("2026-10-09T10:00:05Z", 100, 0, 10),
            event("task_complete", ""),
        ];
        fs::write(&sub_file, sub.join("\n") + "\n").unwrap();
        let mut st = Store::at(dir.clone());
        st.cli = crate::data::Cli::Codex;
        st.refresh();
        // Compressed later by codex: the same thread, not read again, and its
        // transcript is now the compressed file.
        let zst = sub_file.with_extension("jsonl.zst");
        fs::rename(&sub_file, &zst).unwrap();
        st.refresh();
        assert_eq!(st.agents[SUB].path, zst);
        fs::remove_dir_all(&dir).unwrap();

        let mine: Vec<_> = st.recs.iter().filter(|r| r.agent.is_none()).collect();
        assert_eq!(mine.len(), 2, "the repeated count adds nothing");
        let u = |i: usize| {
            (
                mine[i].usage.input,
                mine[i].usage.cache_read,
                mine[i].usage.output,
            )
        };
        assert_eq!(u(0), (200, 800, 50));
        assert_eq!(u(1), (100, 400, 30));
        let s = &st.sessions[ID];
        assert_eq!(
            (
                s.title.as_str(),
                s.cwd.as_str(),
                s.branch.as_str(),
                s.model.as_str()
            ),
            ("arregla el test", "/w", "main", "gpt-5.5-codex")
        );
        assert_eq!((s.context, s.window), (1500, 272_000));
        let a = &st.agents[SUB];
        assert_eq!(
            (a.session.as_str(), a.kind.as_str(), a.desc.as_str(), a.done),
            (ID, "explorer", "Halley", true)
        );
        let sub_recs: Vec<_> = st.recs.iter().filter(|r| r.agent.is_some()).collect();
        assert_eq!((sub_recs.len(), sub_recs[0].session.as_str()), (1, ID));
        // Nothing holds the thread locks: no session is open.
        assert!(st.live.is_empty());
    }

    #[test]
    fn ids_from_file_names() {
        let p = |s: String| thread_id(Path::new(&s));
        assert_eq!(
            p(format!("rollout-2026-10-09T10-00-00-{ID}.jsonl")).as_deref(),
            Some(ID)
        );
        assert_eq!(
            p(format!("rollout-2026-10-09T10-00-00-{ID}.jsonl.zst")).as_deref(),
            Some(ID)
        );
        assert_eq!(p("rollout-short.jsonl".into()), None);
    }

    /// A lock held by another handle reads as an open thread; a free one doesn't.
    #[test]
    fn writer_locks() {
        let dir = std::env::temp_dir().join(format!("cct-codex-locks-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(".coordination.lock"), "").unwrap();
        let held = File::create(dir.join(format!("{ID}.lock"))).unwrap();
        held.lock().unwrap();
        fs::write(dir.join(format!("{SUB}.lock")), "").unwrap();
        assert_eq!(open_threads(&dir), vec![ID.to_string()]);
        // codex busy creating a thread right now: don't probe.
        let coord = File::create(dir.join(".coordination.lock")).unwrap();
        coord.lock().unwrap();
        assert!(open_threads(&dir).is_empty());
        drop((coord, held));
        fs::remove_dir_all(&dir).unwrap();
    }
    /// Against a real codex home (`$CODEX_HOME` or ~/.codex):
    /// `cargo test -- --ignored --nocapture codex::tests::real_home`
    #[test]
    #[ignore]
    fn real_home() {
        let mut st = Store::at(root());
        st.cli = crate::data::Cli::Codex;
        st.refresh();
        crate::data::print_store(&st);
    }
}
