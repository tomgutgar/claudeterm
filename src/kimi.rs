//! Kimi Code CLI (`kimi`): reads `~/.kimi-code` (or `$KIMI_CODE_HOME`) the way
//! data.rs reads `~/.claude`, and fills the same `Store`.
//!
//! What kimi leaves on disk (checked against MoonshotAI/kimi-code's source,
//! wire protocol 1.5, Oct 2026):
//! - `session_index.jsonl`: `{sessionId, sessionDir, workDir}` per session.
//! - `sessions/<workDirKey>/<sessionId>/state.json`: title, lastPrompt, workDir.
//! - `…/<sessionId>/agents/<agentId>/wire.jsonl`: one per agent (`main`, and
//!   `agent-0`… for subagents). After a `metadata` line, each record is
//!   `{"type", …payload, "time"}` (time in ms). Every model request leaves a
//!   `usage.record` with its tokens; `turn.prompt`/`turn.ended` open and close
//!   a turn; `subagent.spawned`/`completed`/`failed`/`cancelled` describe the
//!   subagents, in their parent's wire.
//! - `…/<sessionId>/tasks/<taskId>.json` + `tasks/<taskId>/output.log`: the
//!   background tasks (`kind: "process"` = a shell).
//!
//! kimi keeps no pid or lock per session, so which ones are open is a guess:
//! see `live_sessions`.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use chrono::DateTime;
use serde_json::Value;

use crate::data::{Agent, Live, Rec, Session, Shell, Store, Usage, clip, new_lines};
use crate::platform;

pub fn root() -> PathBuf {
    std::env::var_os("KIMI_CODE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| platform::home().join(".kimi-code"))
}

/// A session counts as working if its main wire was written this recently.
const BUSY: Duration = Duration::from_secs(4);
/// A process can start a little before it opens (or creates) its session.
const START_SLACK: u64 = 120;

#[derive(Default)]
pub struct State {
    /// Bytes already read from each wire.jsonl.
    offsets: HashMap<PathBuf, u64>,
    /// Each state.json: when it was last read.
    states: HashMap<PathBuf, SystemTime>,
    /// Sessions with a turn running.
    in_turn: HashMap<String, bool>,
    /// Each session's directory.
    dirs: HashMap<String, PathBuf>,
    /// Model alias → its context window, from config.toml.
    windows: HashMap<String, u64>,
}

pub fn refresh(st: &mut Store) {
    let index = session_index(&st.root);
    st.kimi.windows =
        context_windows(&fs::read_to_string(st.root.join("config.toml")).unwrap_or_default());
    let Ok(buckets) = fs::read_dir(st.root.join("sessions")) else {
        st.live.clear();
        return;
    };
    for dir in buckets.flatten().map(|e| e.path()).filter(|p| p.is_dir()) {
        let Ok(sessions) = fs::read_dir(&dir) else {
            continue;
        };
        for sdir in sessions.flatten().map(|e| e.path()).filter(|p| p.is_dir()) {
            let Some(id) = sdir
                .file_name()
                .and_then(|n| n.to_str())
                .map(str::to_string)
            else {
                continue;
            };
            read_state(st, &id, &sdir, index.get(&id));
            st.kimi.dirs.insert(id.clone(), sdir.clone());
            let Ok(agents) = fs::read_dir(sdir.join("agents")) else {
                continue;
            };
            // main first: it says what each subagent is before its tokens come.
            let mut agents: Vec<PathBuf> = agents.flatten().map(|e| e.path()).collect();
            agents.sort_by_key(|p| p.file_name().is_none_or(|n| n != "main"));
            for adir in agents {
                let agent = adir
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("")
                    .to_string();
                let wire = adir.join("wire.jsonl");
                let mut off = st.kimi.offsets.get(&wire).copied().unwrap_or(0);
                new_lines(&wire, &mut off, |line| ingest(st, &id, &agent, line));
                st.kimi.offsets.insert(wire.clone(), off);
                if let Some(a) = st.agents.get_mut(&format!("{id}/{agent}")) {
                    a.mtime = mtime(&wire);
                }
            }
        }
    }
    st.live = live_sessions(st);
    for l in st.live.clone() {
        read_tasks(st, &l.session);
    }
}

/// `max_context_size` of each `[models."<alias>"]` in config.toml: the
/// window kimi works with (its wire doesn't record it). Just that key, so no
/// TOML parser is needed.
fn context_windows(toml: &str) -> HashMap<String, u64> {
    let mut out = HashMap::new();
    let mut model: Option<String> = None;
    for line in toml.lines().map(str::trim) {
        if line.starts_with('[') {
            model = line
                .strip_prefix("[models.")
                .and_then(|m| m.strip_suffix(']'))
                .map(|m| m.trim_matches('"').to_string());
        } else if let (Some(m), Some(v)) = (&model, line.strip_prefix("max_context_size"))
            && let Some(n) = v
                .trim_start()
                .strip_prefix('=')
                .and_then(|n| n.trim().parse().ok())
        {
            out.insert(m.clone(), n);
        }
    }
    out
}

/// sessionId → workDir, from `session_index.jsonl`.
fn session_index(root: &Path) -> HashMap<String, String> {
    fs::read_to_string(root.join("session_index.jsonl"))
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter_map(|v| {
            Some((
                v["sessionId"].as_str()?.to_string(),
                v["workDir"].as_str()?.to_string(),
            ))
        })
        .collect()
}

fn mtime(p: &Path) -> SystemTime {
    fs::metadata(p)
        .and_then(|m| m.modified())
        .unwrap_or(UNIX_EPOCH)
}

/// Title and directory from `state.json`, when it changes.
fn read_state(st: &mut Store, id: &str, sdir: &Path, indexed: Option<&String>) {
    let path = sdir.join("state.json");
    let changed = mtime(&path);
    let s = st
        .sessions
        .entry(id.to_string())
        .or_insert_with(|| Session {
            id: id.to_string(),
            path: sdir.join("agents").join("main").join("wire.jsonl"),
            ..Default::default()
        });
    if s.cwd.is_empty()
        && let Some(w) = indexed
    {
        s.cwd = w.clone();
    }
    if st.kimi.states.get(&path) == Some(&changed) {
        return;
    }
    st.kimi.states.insert(path.clone(), changed);
    let Ok(v) = serde_json::from_str::<Value>(&fs::read_to_string(&path).unwrap_or_default())
    else {
        return;
    };
    let text = |k: &str| v[k].as_str().map(str::trim).filter(|t| !t.is_empty());
    if let Some(w) = text("workDir").or(text("cwd")) {
        s.cwd = w.to_string();
    }
    // kimi's untitled sessions are called "New Session" (isUntitled()).
    if let Some(t) = text("title")
        .filter(|t| *t != "New Session")
        .or(text("lastPrompt"))
    {
        s.title = clip(t.lines().next().unwrap_or(""), 90);
    }
}

/// A prompt you typed: `origin` is "user" now, `{"kind":"user"}` in older wires.
fn from_user(origin: &Value) -> bool {
    origin == "user" || origin["kind"] == "user"
}

/// First text part of a ContentPart[].
fn text_of(parts: &Value) -> Option<&str> {
    parts.as_array()?.iter().find(|p| p["type"] == "text")?["text"]
        .as_str()
        .map(str::trim)
        .filter(|t| !t.is_empty())
}

fn ingest(st: &mut Store, id: &str, agent: &str, line: &str) {
    // Cheap filter before parsing: most records are context and tool traffic.
    const WANTED: [&str; 4] = [
        "\"usage.record\"",
        "\"turn.",
        "\"subagent.",
        "\"config.update\"",
    ];
    if !WANTED.iter().any(|w| line.contains(w)) {
        return;
    }
    let Ok(v) = serde_json::from_str::<Value>(line) else {
        return;
    };
    let main = agent == "main";
    let key = |sub: &str| format!("{id}/{sub}");
    match v["type"].as_str().unwrap_or("") {
        "usage.record" => {
            let Some(ts) = v["time"].as_i64().and_then(DateTime::from_timestamp_millis) else {
                return;
            };
            let n = |k: &str| v["usage"][k].as_u64().unwrap_or(0);
            let usage = Usage {
                input: n("inputOther"),
                output: n("output"),
                cache_read: n("inputCacheRead"),
                cache_5m: n("inputCacheCreation"),
                ..Default::default()
            };
            let model = v["model"]
                .as_str()
                .filter(|m| !m.is_empty())
                .unwrap_or("kimi")
                .to_string();
            let s = st.sessions.get_mut(id).unwrap();
            if main {
                s.last = Some(ts);
                s.model = model.clone();
                s.context = usage.context();
                s.window = st.kimi.windows.get(&model).copied().unwrap_or(0);
            }
            let cwd = s.cwd.clone();
            st.recs.push(Rec {
                ts,
                session: id.to_string(),
                agent: (!main).then(|| key(agent)),
                model,
                cwd,
                usage,
            });
        }
        "turn.prompt" if main => {
            st.kimi.in_turn.insert(id.to_string(), true);
            let s = st.sessions.get_mut(id).unwrap();
            if s.title.is_empty()
                && from_user(&v["origin"])
                && let Some(t) = text_of(&v["input"])
            {
                s.title = clip(t.lines().next().unwrap_or(""), 90);
            }
        }
        // Older wires only say the directory here.
        "config.update" if main => {
            let s = st.sessions.get_mut(id).unwrap();
            if s.cwd.is_empty() {
                s.cwd = v["cwd"].as_str().unwrap_or("").to_string();
            }
        }
        "turn.ended" if main => {
            st.kimi.in_turn.insert(id.to_string(), false);
        }
        "subagent.spawned" => {
            let Some(sub) = v["subagentId"].as_str() else {
                return;
            };
            let wire = st.kimi.dirs.get(id).map_or_else(PathBuf::new, |d| {
                d.join("agents").join(sub).join("wire.jsonl")
            });
            st.agents.insert(
                key(sub),
                Agent {
                    id: key(sub),
                    session: id.to_string(),
                    kind: v["subagentName"].as_str().unwrap_or("subagent").to_string(),
                    desc: v["description"].as_str().unwrap_or("").to_string(),
                    mtime: mtime(&wire),
                    path: wire,
                    done: false,
                },
            );
        }
        "subagent.completed" | "subagent.failed" | "subagent.cancelled" => {
            if let Some(a) = v["subagentId"]
                .as_str()
                .and_then(|s| st.agents.get_mut(&key(s)))
            {
                a.done = true;
            }
        }
        _ => {}
    }
}

/// Background shells of a session (`tasks/<id>.json`, `kind: "process"`).
/// Newer files are camelCase, older ones snake_case.
fn read_tasks(st: &mut Store, id: &str) {
    let Some(dir) = st.kimi.dirs.get(id).map(|d| d.join("tasks")) else {
        return;
    };
    let Ok(entries) = fs::read_dir(&dir) else {
        return;
    };
    for p in entries.flatten().map(|e| e.path()) {
        if p.extension().is_none_or(|e| e != "json") {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(&fs::read_to_string(&p).unwrap_or_default())
        else {
            continue;
        };
        let Some(task) = v["taskId"].as_str().or(v["task_id"].as_str()) else {
            continue;
        };
        if v["kind"] == "agent" || task.starts_with("agent-") {
            continue;
        }
        let desc = v["description"]
            .as_str()
            .or(v["command"].as_str())
            .unwrap_or("?");
        st.shells.insert(
            format!("{id}/{task}"),
            Shell {
                id: task.to_string(),
                session: id.to_string(),
                agent: None,
                desc: clip(desc.lines().next().unwrap_or(""), 60),
                output: dir.join(task).join("output.log"),
                done: !matches!(v["status"].as_str(), Some("running" | "awaiting_approval")),
            },
        );
    }
}

/// The kimi sessions open right now. kimi leaves no pid on disk, so: count
/// the running `kimi` processes and take that many sessions, the most recently
/// written among those touched since the oldest of them started.
/// ponytail: a resumed session shows up after its first message, and an npm
/// install (a `node` process) isn't seen; a pid file from kimi would fix both.
pub fn live_sessions(st: &Store) -> Vec<Live> {
    let starts = platform::procs(&["kimi", "kimi-code"]);
    let Some(&oldest) = starts.iter().min() else {
        return Vec::new();
    };
    let since =
        UNIX_EPOCH + Duration::from_secs((oldest.max(0) as u64).saturating_sub(START_SLACK));
    let mut recent: Vec<(SystemTime, &String, &PathBuf)> = st
        .kimi
        .dirs
        .iter()
        .map(|(id, d)| {
            let main = d.join("agents").join("main").join("wire.jsonl");
            (mtime(&main).max(mtime(&d.join("state.json"))), id, d)
        })
        .filter(|(t, ..)| *t >= since)
        .collect();
    recent.sort_by_key(|r| std::cmp::Reverse(r.0));
    recent
        .into_iter()
        .take(starts.len())
        .map(|(_, id, d)| {
            let s = st.sessions.get(id);
            let wire = d.join("agents").join("main").join("wire.jsonl");
            let busy = st.kimi.in_turn.get(id).copied().unwrap_or(false)
                || mtime(&wire).elapsed().is_ok_and(|e| e < BUSY);
            Live {
                pid: 0,
                session: id.clone(),
                cwd: s.map(|s| s.cwd.clone()).unwrap_or_default(),
                name: s.map(|s| s.title.clone()).unwrap_or_default(),
                status: if busy { "busy" } else { "idle" }.into(),
                proc_start: String::new(),
            }
        })
        .collect()
}

/// "   ⚙ Bash: npm test": a tool call, by its most telling argument.
fn tool_line(name: &str, args: &Value) -> String {
    let what = [
        "command",
        "description",
        "path",
        "file_path",
        "pattern",
        "url",
    ]
    .iter()
    .find_map(|k| args[k].as_str())
    .map_or_else(|| args.to_string(), str::to_string);
    format!(
        "   ⚙ {name}: {}",
        clip(what.lines().next().unwrap_or(""), 120)
    )
}

/// What was said in that wire: your prompts, kimi's answers and its tool calls.
/// kimi streams each answer as `content.part` loop events between a
/// `step.begin` and a `step.end`: their text is joined into one message.
pub fn transcript(path: &Path) -> String {
    let text = fs::read_to_string(path).unwrap_or_default();
    let you = crate::i18n::tr("» tú", "» you");
    let mut out = Vec::new();
    let mut answer = String::new();
    let flush = |answer: &mut String, out: &mut Vec<String>| {
        if !answer.trim().is_empty() {
            out.push(format!("« kimi: {}", clip(answer.trim(), 600)));
        }
        answer.clear();
    };
    for line in text.lines() {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let e = &v["event"];
        match (
            v["type"].as_str().unwrap_or(""),
            e["type"].as_str().unwrap_or(""),
        ) {
            ("turn.prompt", _) if from_user(&v["origin"]) => {
                flush(&mut answer, &mut out);
                if let Some(t) = text_of(&v["input"]) {
                    out.push(format!("{you}: {}", clip(t, 600)));
                }
            }
            ("context.append_loop_event", "content.part") if e["part"]["type"] == "text" => {
                answer.push_str(e["part"]["text"].as_str().unwrap_or(""));
            }
            ("context.append_loop_event", "tool.call") => {
                flush(&mut answer, &mut out);
                out.push(tool_line(e["name"].as_str().unwrap_or("?"), &e["args"]));
            }
            ("context.append_loop_event", "step.begin" | "step.end") => {
                flush(&mut answer, &mut out);
            }
            // Whole messages (e.g. a compaction summary).
            ("context.append_message", _) if v["message"]["role"] == "assistant" => {
                flush(&mut answer, &mut out);
                let m = &v["message"];
                if let Some(t) = text_of(&m["content"]) {
                    out.push(format!("« kimi: {}", clip(t, 600)));
                }
                for tc in m["toolCalls"].as_array().into_iter().flatten() {
                    let args = serde_json::from_str(tc["arguments"].as_str().unwrap_or(""))
                        .unwrap_or_default();
                    out.push(tool_line(tc["name"].as_str().unwrap_or("?"), &args));
                }
            }
            _ => {}
        }
    }
    flush(&mut answer, &mut out);
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

    const ID: &str = "11111111-aaaa-4bbb-8ccc-111111111111";

    fn usage(agent: &str, time: i64, input: u64, out: u64) -> String {
        format!(
            r#"{{"type":"usage.record","agentId":"{agent}","model":"kimi-code/k3","usage":{{"inputOther":{input},"output":{out},"inputCacheRead":19200,"inputCacheCreation":0}},"usageScope":"turn","time":{time}}}"#
        )
    }

    /// A session with a subagent and a background shell, as kimi writes them.
    #[test]
    fn wires_and_tasks() {
        let root = std::env::temp_dir().join(format!("cct-kimi-{}", std::process::id()));
        let sdir = root.join("sessions/wd_w_0123456789ab").join(ID);
        fs::create_dir_all(sdir.join("agents/main")).unwrap();
        fs::create_dir_all(sdir.join("agents/agent-0")).unwrap();
        fs::create_dir_all(sdir.join("tasks")).unwrap();
        fs::write(
            root.join("session_index.jsonl"),
            format!("{{\"sessionId\":\"{ID}\",\"sessionDir\":\"x\",\"workDir\":\"/w\"}}\n"),
        )
        .unwrap();
        fs::write(
            sdir.join("state.json"),
            r#"{"title":"New Session","createdAt":1791540000000}"#,
        )
        .unwrap();
        let main = [
            r#"{"type":"metadata","protocol_version":"1.5","created_at":1791540000000}"#.to_string(),
            r#"{"type":"turn.prompt","agentId":"main","input":[{"type":"text","text":"revisa el login\ny los tests"}],"origin":"user","turnId":1,"time":1791540000100}"#.into(),
            usage("main", 1791540001000, 5904, 658),
            r#"{"type":"subagent.spawned","subagentId":"agent-0","subagentName":"coder","parentToolCallId":"t1","description":"Fix the login test","runInBackground":false,"time":1791540002000}"#.into(),
            r#"{"type":"turn.ended","agentId":"main","turnId":1,"reason":"completed","time":1791540009000}"#.into(),
        ];
        fs::write(sdir.join("agents/main/wire.jsonl"), main.join("\n") + "\n").unwrap();
        fs::write(
            sdir.join("agents/agent-0/wire.jsonl"),
            usage("agent-0", 1791540003000, 100, 10) + "\n",
        )
        .unwrap();
        fs::write(
            sdir.join("tasks/bash-1.json"),
            r#"{"taskId":"bash-1","kind":"process","description":"npm run dev","status":"running","command":"npm run dev"}"#,
        )
        .unwrap();
        let mut st = Store::at(root.clone());
        st.cli = crate::data::Cli::Kimi;
        st.refresh();
        read_tasks(&mut st, ID);
        let shell = st.shells.remove(&format!("{ID}/bash-1"));
        fs::remove_dir_all(&root).unwrap();

        let s = &st.sessions[ID];
        assert_eq!(
            (
                s.title.as_str(),
                s.cwd.as_str(),
                s.model.as_str(),
                s.context
            ),
            ("revisa el login", "/w", "kimi-code/k3", 5904 + 19200)
        );
        assert_eq!(st.recs.len(), 2);
        let sub = st.recs.iter().find(|r| r.agent.is_some()).unwrap();
        assert_eq!((sub.session.as_str(), sub.usage.input), (ID, 100));
        let a = &st.agents[&format!("{ID}/agent-0")];
        assert_eq!(
            (a.kind.as_str(), a.desc.as_str(), a.done),
            ("coder", "Fix the login test", false)
        );
        assert_eq!(st.kimi.in_turn.get(ID), Some(&false));
        let sh = shell.unwrap();
        assert_eq!((sh.desc.as_str(), sh.done), ("npm run dev", false));
        assert!(sh.output.ends_with("tasks/bash-1/output.log"));
    }
    /// An older wire (protocol 1.1), copied from kimi-code's own test fixtures
    /// (apps/vis/server/test/fixtures/sessions/sample-main, MIT): `origin` is an
    /// object, records carry no agentId and the answer streams as loop events.
    #[test]
    fn protocol_1_1_fixture() {
        let wire = [
            r#"{"type":"metadata","protocol_version":"1.1","created_at":1779256791085}"#,
            r#"{"type":"config.update","cwd":"/tmp/work","profileName":"agent","systemPrompt":"You are Kimi.","time":1779256791100}"#,
            r#"{"type":"turn.prompt","input":[{"type":"text","text":"hi"}],"origin":{"kind":"user"},"time":1779256800000}"#,
            r#"{"type":"context.append_message","message":{"role":"user","content":[{"type":"text","text":"hi"}],"toolCalls":[]},"time":1779256800001}"#,
            r#"{"type":"context.append_loop_event","event":{"type":"step.begin","uuid":"s1","turnId":"t1","step":0},"time":1779256800100}"#,
            r#"{"type":"context.append_loop_event","event":{"type":"content.part","uuid":"c1","turnId":"t1","step":0,"stepUuid":"s1","part":{"type":"text","text":"hello"}},"time":1779256800200}"#,
            r#"{"type":"context.append_loop_event","event":{"type":"step.end","uuid":"s1","turnId":"t1","step":0,"usage":{"inputOther":10,"output":5,"inputCacheRead":0,"inputCacheCreation":0},"finishReason":"end_turn"},"time":1779256800300}"#,
            r#"{"type":"usage.record","model":"kimi-k2","usage":{"inputOther":10,"output":5,"inputCacheRead":0,"inputCacheCreation":0},"usageScope":"turn","time":1779256800302}"#,
        ];
        let root = std::env::temp_dir().join(format!("cct-kimi11-{}", std::process::id()));
        let sdir = root.join("sessions/wd_work_0123456789ab/sample-main");
        fs::create_dir_all(sdir.join("agents/main")).unwrap();
        let file = sdir.join("agents/main/wire.jsonl");
        fs::write(
            &file,
            wire.join(
                "
",
            ) + "
",
        )
        .unwrap();
        let mut st = Store::at(root.clone());
        st.cli = crate::data::Cli::Kimi;
        st.refresh();
        let text = transcript(&file);
        fs::remove_dir_all(&root).unwrap();

        let s = &st.sessions["sample-main"];
        assert_eq!(
            (s.title.as_str(), s.model.as_str(), s.cwd.as_str()),
            ("hi", "kimi-k2", "/tmp/work")
        );
        assert_eq!(st.recs.len(), 1);
        assert_eq!(st.recs[0].usage.total(), 15);
        assert!(
            text.contains(": hi") && text.ends_with("« kimi: hello"),
            "{text}"
        );
    }

    #[test]
    fn windows_from_config() {
        let toml = "default_model = \"local/q\"
[providers.local]
type = \"openai\"

[models.\"local/q\"]
provider = \"local\"
max_context_size = 32768
[models.k2]
max_context_size=262144
";
        let w = context_windows(toml);
        assert_eq!((w["local/q"], w["k2"], w.len()), (32768, 262144, 2));
    }

    /// Against a real kimi home (`$KIMI_CODE_HOME` or ~/.kimi-code):
    /// `cargo test -- --ignored --nocapture kimi::tests::real_home`
    #[test]
    #[ignore]
    fn real_home() {
        let mut st = Store::at(root());
        st.cli = crate::data::Cli::Kimi;
        st.refresh();
        crate::data::print_store(&st);
    }
}
