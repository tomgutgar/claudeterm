//! Reader for Claude Code's local files (`~/.claude`). Everything claudeterm
//! shows comes from here: no API, no network, read-only.
//!
//! - `sessions/<pid>.json`: one per running `claude` (busy/idle status).
//! - `projects/<dir>/<session>.jsonl`: the history; every model reply carries
//!   its `usage` (tokens).
//! - `projects/<dir>/<session>/subagents/agent-<id>.jsonl` + `.meta.json`:
//!   the subagents that session launched.

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use chrono::{DateTime, Datelike, Duration, Local, NaiveDate, NaiveTime, Timelike, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::i18n::tr;
use crate::platform;

/// Which agent CLI a `Store` reads.
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub enum Cli {
    #[default]
    Claude,
    Agy,
    Codex,
    Kimi,
}

impl Cli {
    /// In this order everywhere: home screen, hub, `Store`s.
    pub const ALL: [Cli; 4] = [Cli::Claude, Cli::Agy, Cli::Codex, Cli::Kimi];

    pub fn name(self) -> &'static str {
        match self {
            Cli::Claude => "Claude Code",
            Cli::Agy => "Antigravity",
            Cli::Codex => "Codex",
            Cli::Kimi => "Kimi Code",
        }
    }
    /// The command that starts it.
    pub fn prog(self) -> &'static str {
        match self {
            Cli::Claude => "claude",
            Cli::Agy => "agy",
            Cli::Codex => "codex",
            Cli::Kimi => "kimi",
        }
    }
    /// What goes before a session id to resume it (`codex resume <id>`).
    pub fn resume_flag(self) -> &'static str {
        match self {
            Cli::Claude => "--resume",
            Cli::Agy => "--conversation",
            Cli::Codex => "resume",
            Cli::Kimi => "--session",
        }
    }
    pub fn color(self) -> ratatui::style::Color {
        use ratatui::style::Color::Rgb;
        match self {
            Cli::Claude => Rgb(0xd9, 0x77, 0x57),
            Cli::Agy => Rgb(0x4f, 0x8f, 0xf7),
            // OpenAI's green: Codex's own accent is the terminal's default colour.
            Cli::Codex => Rgb(0x10, 0xa3, 0x7f),
            // The logo's colour in kimi's welcome box (dark theme `primary`).
            Cli::Kimi => Rgb(0x4f, 0xa8, 0xff),
        }
    }
    /// Has a price table (`PRICES`): the others show tokens only.
    pub fn priced(self) -> bool {
        self == Cli::Claude
    }
}

#[derive(Clone, Copy, Default, Debug, PartialEq, Serialize, Deserialize)]
pub struct Usage {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_5m: u64,
    pub cache_1h: u64,
}

impl Usage {
    pub fn total(&self) -> u64 {
        self.input + self.output + self.cache_read + self.cache_5m + self.cache_1h
    }

    pub fn add(&mut self, o: &Usage) {
        self.input += o.input;
        self.output += o.output;
        self.cache_read += o.cache_read;
        self.cache_5m += o.cache_5m;
        self.cache_1h += o.cache_1h;
    }

    /// What `self` has and `o` lacks, field by field (never negative).
    pub fn minus(&self, o: &Usage) -> Usage {
        Usage {
            input: self.input.saturating_sub(o.input),
            output: self.output.saturating_sub(o.output),
            cache_read: self.cache_read.saturating_sub(o.cache_read),
            cache_5m: self.cache_5m.saturating_sub(o.cache_5m),
            cache_1h: self.cache_1h.saturating_sub(o.cache_1h),
        }
    }

    /// Size of the conversation on that turn (everything that was sent).
    pub fn context(&self) -> u64 {
        self.input + self.cache_read + self.cache_5m + self.cache_1h
    }

    /// API-price-equivalent cost, in dollars. None = model without a price.
    pub fn cost(&self, model: &str) -> Option<f64> {
        let (i, o, r) = price(model)?;
        let usd = self.input as f64 * i
            + self.output as f64 * o
            + self.cache_read as f64 * r
            + self.cache_5m as f64 * i * 1.25
            + self.cache_1h as f64 * i * 2.0;
        Some(usd / 1e6)
    }
}

/// $ per million tokens: (input, output, cache read). Cache writes cost 1.25×
/// input (5 min) or 2× (1 h). Source: Anthropic's API pricing as of 2026-06-24.
/// ponytail: hand-written table; a new model needs one more row here. Models
/// not listed count tokens but no cost. Order: most specific first.
const PRICES: &[(&str, f64, f64, f64)] = &[
    ("fable-5-1", 10.0, 50.0, 0.25),
    ("mythos-5-1", 10.0, 50.0, 0.25),
    ("fable-5", 10.0, 50.0, 1.0),
    ("mythos-5", 10.0, 50.0, 1.0),
    ("opus-5-5", 4.0, 20.0, 0.20),
    ("opus-5", 5.0, 25.0, 0.50),
    ("opus-4-8", 5.0, 25.0, 0.50),
    ("opus-4-7", 5.0, 25.0, 0.50),
    ("opus-4-6", 5.0, 25.0, 0.50),
    ("opus-4-5", 5.0, 25.0, 0.50),
    ("sonnet-5", 2.0, 10.0, 0.20),
    ("sonnet-4-6", 3.0, 15.0, 0.30),
    ("sonnet-4-5", 3.0, 15.0, 0.30),
    ("haiku-4-5", 1.0, 5.0, 0.10),
];

fn price(model: &str) -> Option<(f64, f64, f64)> {
    PRICES
        .iter()
        .find(|(k, ..)| model.contains(k))
        .map(|&(_, i, o, r)| (i, o, r))
}

/// Estimated footprint of some tokens. Anthropic does NOT publish energy,
/// water or CO₂ per token: this is an order-of-magnitude approximation.
#[derive(Clone, Copy, Default, Debug, PartialEq)]
pub struct Footprint {
    pub wh: f64,
    pub water_ml: f64,
    pub co2_g: f64,
}

/// Wh per output token, data-centre overhead included: central figure and
/// range (sources reviewed in Sept. 2026):
/// - "Photons = Tokens" (arXiv 2603.06630, 2026): 0.1–2 mWh/token depending on size.
/// - Muxup (2026q1, measured Dec. 2025): DeepSeek-R1 671B ≈ 1.9 mWh/token in
///   fp8 and ≈ 0.35–0.6 in fp4.
/// - Joule (2026, peer-reviewed): >200B models on H100, median 0.31 Wh per
///   query; says public estimates overstate ×4–20 → low bound.
/// - Claude Code estimate (M. Dodkins' gist, 2026-03-09): 1.95 mWh/token → high bound.
const WH_PER_OUTPUT_TOKEN: f64 = 0.001;
pub const RANGE_LOW: f64 = 0.3; // → 0.3 mWh/token
pub const RANGE_HIGH: f64 = 2.0; // → 2 mWh/token
/// Weight of each token type relative to an output token. Assumption: compute
/// is proportional to API price (input 1/5 of output, cache write 1.25× input,
/// cache read 1/10 of input). The 1 h TTL costs more for being stored longer,
/// not for compute: it weighs the same as 5 min. The 2026 Claude Code
/// estimate (gist above) uses these same ratios.
const W_INPUT: f64 = 0.2;
const W_CACHE_WRITE: f64 = 0.25;
const W_CACHE_READ: f64 = 0.02;
/// mL of water per Wh, counting data-centre cooling (≈1.1 mL/Wh according to
/// Google, Aug. 2025: 0.24 Wh and 0.26 mL per Gemini prompt) AND the water
/// used to generate the electricity. Total ≈ 2 mL/Wh, the mid value of the
/// 2026 Claude Code gist (regional ranges from 1.8 to 12 L/kWh).
const WATER_ML_PER_WH: f64 = 2.0;
/// g CO₂e per Wh. Source: EIA, US grid average in 2023 ≈ 0.81 lb/kWh ≈
/// 370 g/kWh (location-based; lower with renewable purchases).
const CO2_G_PER_WH: f64 = 0.37;

impl Usage {
    pub fn footprint(&self) -> Footprint {
        let equiv_output = self.output as f64
            + self.input as f64 * W_INPUT
            + (self.cache_5m + self.cache_1h) as f64 * W_CACHE_WRITE
            + self.cache_read as f64 * W_CACHE_READ;
        let wh = equiv_output * WH_PER_OUTPUT_TOKEN;
        Footprint {
            wh,
            water_ml: wh * WATER_ML_PER_WH,
            co2_g: wh * CO2_G_PER_WH,
        }
    }
}

impl Footprint {
    /// Everything scales with energy: the range is the central figure times k.
    pub fn scaled(&self, k: f64) -> Footprint {
        Footprint {
            wh: self.wh * k,
            water_ml: self.water_ml * k,
            co2_g: self.co2_g * k,
        }
    }
}

/// The model's context window (for the % of context used). Checked against
/// real history: Opus sessions reached ~688k tokens before compacting, so in
/// Claude Code the window is 1M.
pub fn context_window(model: &str) -> u64 {
    // ponytail: agy stores 256k as the window of every Gemini model it has
    // used so far; read it from the conversation if that stops being true.
    // ponytail: Kimi's wire log doesn't store the window; K2/K3 have 256k.
    if model.contains("haiku") {
        200_000
    } else if model.starts_with("gemini") || model.contains("kimi") {
        256_000
    } else {
        1_000_000
    }
}

/// One model reply with its tokens.
pub struct Rec {
    pub ts: DateTime<Utc>,
    pub session: String,
    pub agent: Option<String>,
    pub model: String,
    pub cwd: String,
    pub usage: Usage,
}

/// A Claude Code session (live or not), as read from its transcript.
#[derive(Default)]
pub struct Session {
    pub id: String,
    pub path: PathBuf,
    pub cwd: String,
    pub branch: String,
    /// The user's first message: used as the title.
    pub title: String,
    pub last: Option<DateTime<Utc>>,
    pub model: String,
    /// Context tokens of the last turn.
    pub context: u64,
    /// The model's context window, when the CLI writes it down (Codex); 0 =
    /// guess it from the model with `context_window`.
    pub window: u64,
}

impl Session {
    /// % of the context window used on the last turn.
    pub fn context_pct(&self) -> u64 {
        let w = if self.window > 0 {
            self.window
        } else {
            context_window(&self.model)
        };
        self.context * 100 / w.max(1)
    }
}

pub struct Agent {
    pub id: String,
    pub session: String,
    pub kind: String,
    pub desc: String,
    pub path: PathBuf,
    pub mtime: SystemTime,
    /// Its last line is its final answer: finished.
    pub done: bool,
}

/// A command a session left running in the background (Bash with
/// `run_in_background`).
pub struct Shell {
    pub id: String,
    pub session: String,
    /// The subagent that launched it, if it wasn't the session itself.
    pub agent: Option<String>,
    pub desc: String,
    /// Where Claude Code writes its output: if it changes, it's working.
    pub output: PathBuf,
    pub done: bool,
}

/// A `claude` running right now (`~/.claude/sessions/<pid>.json`).
#[derive(Deserialize, Clone)]
pub struct Live {
    pub pid: u32,
    #[serde(rename = "sessionId")]
    pub session: String,
    #[serde(default)]
    pub cwd: String,
    #[serde(default)]
    pub name: String,
    /// "busy" (working) or "idle" (waiting for you).
    #[serde(default)]
    pub status: String,
    #[serde(rename = "procStart", default)]
    pub(crate) proc_start: String,
}

// Only the fields we use; serde skips the rest without storing it.
#[derive(Deserialize)]
struct Line {
    #[serde(rename = "type", default)]
    kind: String,
    timestamp: Option<DateTime<Utc>>,
    #[serde(default)]
    cwd: String,
    #[serde(rename = "gitBranch", default)]
    branch: String,
    #[serde(rename = "isSidechain", default)]
    sidechain: bool,
    #[serde(rename = "isMeta", default)]
    meta: bool,
    message: Option<Msg>,
}

#[derive(Deserialize)]
struct Msg {
    id: Option<String>,
    #[serde(default)]
    model: String,
    usage: Option<RawUsage>,
}

#[derive(Deserialize, Default)]
struct RawUsage {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
    #[serde(default)]
    cache_read_input_tokens: u64,
    #[serde(default)]
    cache_creation_input_tokens: u64,
    cache_creation: Option<RawCache>,
}

#[derive(Deserialize, Default)]
struct RawCache {
    #[serde(default)]
    ephemeral_5m_input_tokens: u64,
    #[serde(default)]
    ephemeral_1h_input_tokens: u64,
}

impl From<RawUsage> for Usage {
    fn from(r: RawUsage) -> Self {
        // No per-TTL breakdown (older versions): assume the 5 min cache.
        let (c5, c1) = match r.cache_creation {
            Some(c) => (c.ephemeral_5m_input_tokens, c.ephemeral_1h_input_tokens),
            None => (r.cache_creation_input_tokens, 0),
        };
        Usage {
            input: r.input_tokens,
            output: r.output_tokens,
            cache_read: r.cache_read_input_tokens,
            cache_5m: c5,
            cache_1h: c1,
        }
    }
}

pub struct Store {
    pub cli: Cli,
    pub root: PathBuf,
    /// Bytes already read from each jsonl: each refresh only reads what's new.
    offsets: HashMap<PathBuf, u64>,
    /// message id → index in `recs`. Claude Code writes one line per block of
    /// the reply, all with the same id and the same `usage`: without this it
    /// would be counted several times.
    seen: HashMap<String, usize>,
    pub recs: Vec<Rec>,
    pub sessions: HashMap<String, Session>,
    pub agents: HashMap<String, Agent>,
    pub live: Vec<Live>,
    /// Tokens from days whose transcripts Claude Code already deleted, taken
    /// from our own history. Only for the Usage stats.
    pub archived: Vec<Rec>,
    archive: HashMap<DayKey, Usage>,
    archive_path: Option<PathBuf>,
    /// Background commands by task id.
    pub shells: HashMap<String, Shell>,
    /// tool_use id → description of a background Bash not yet started.
    pending_shells: HashMap<String, String>,
    pub agy: crate::agy::State,
    pub codex: crate::codex::State,
    pub kimi: crate::kimi::State,
}

/// (local day, model, directory)
type DayKey = (NaiveDate, String, String);

/// One row of our own history (`~/.local/share/claudeterm/history.json`).
#[derive(Serialize, Deserialize)]
struct DayRow {
    day: NaiveDate,
    model: String,
    cwd: String,
    #[serde(flatten)]
    usage: Usage,
}

/// Reads the history. Returns whether it's safe to write it back: an existing
/// file that can't be read (locked, permissions) is left alone this run, and
/// one that can't be parsed (corrupt, newer format) is moved aside first, so
/// a save never wipes out days only it remembers.
fn load_history(path: &Path) -> (Vec<DayRow>, bool) {
    let text = match fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return (Vec::new(), true),
        // Not UTF-8: corrupt, handled below like bad JSON.
        Err(e) if e.kind() == std::io::ErrorKind::InvalidData => String::new(),
        Err(_) => return (Vec::new(), false),
    };
    match serde_json::from_str(&text) {
        Ok(rows) => (rows, true),
        Err(_) => {
            let stamp = Local::now().format("%Y%m%d-%H%M%S");
            let bad = path.with_extension(format!("json.bad-{stamp}"));
            (Vec::new(), fs::rename(path, bad).is_ok())
        }
    }
}

fn local_day(t: DateTime<Utc>) -> NaiveDate {
    t.with_timezone(&Local).date_naive()
}

impl Store {
    pub fn new() -> Self {
        let mut st = Self::at(claude_root());
        let dir = platform::data_dir().join("claudeterm");
        let path = dir.join("history.json");
        // ponytail: name used before the English release; drop this after a while.
        let legacy = dir.join("historial.json");
        let (rows, writable) =
            if !path.exists() && legacy.exists() && fs::rename(&legacy, &path).is_err() {
                // Couldn't migrate (file locked?): use the old one read-only this run,
                // so a new history.json can't shadow it forever.
                (load_history(&legacy).0, false)
            } else {
                load_history(&path)
            };
        st.archive = rows
            .into_iter()
            .map(|r| ((r.day, r.model, r.cwd), r.usage))
            .collect();
        st.archive_path = writable.then_some(path);
        st
    }

    /// The store of each CLI, in `Cli::ALL` order. Only Claude Code's keeps a
    /// history of its own: the others don't delete their old sessions.
    pub fn all() -> [Self; 4] {
        let other = |cli, root| Self {
            cli,
            ..Self::at(root)
        };
        [
            Self::new(),
            other(Cli::Agy, crate::agy::root()),
            other(Cli::Codex, crate::codex::root()),
            other(Cli::Kimi, crate::kimi::root()),
        ]
    }

    pub fn at(root: PathBuf) -> Self {
        Self {
            cli: Cli::Claude,
            root,
            offsets: HashMap::new(),
            seen: HashMap::new(),
            recs: Vec::new(),
            sessions: HashMap::new(),
            agents: HashMap::new(),
            live: Vec::new(),
            archived: Vec::new(),
            archive: HashMap::new(),
            archive_path: None,
            shells: HashMap::new(),
            pending_shells: HashMap::new(),
            agy: Default::default(),
            codex: Default::default(),
            kimi: Default::default(),
        }
    }

    pub fn refresh(&mut self) {
        match self.cli {
            Cli::Claude => {}
            Cli::Agy => return crate::agy::refresh(self),
            Cli::Codex => return crate::codex::refresh(self),
            Cli::Kimi => return crate::kimi::refresh(self),
        }
        self.live = live_sessions(&self.root);
        self.scan_projects();
        self.sync_archive();
    }

    /// Claude Code deletes transcripts older than 30 days (`cleanupPeriodDays`)
    /// and the Usage history would go with them. So we keep a per-day summary
    /// of finished days. If a day now has fewer tokens than saved, something
    /// was deleted: the difference goes to `archived`.
    fn sync_archive(&mut self) {
        let today = Local::now().date_naive();
        let mut now: HashMap<DayKey, Usage> = HashMap::new();
        for r in &self.recs {
            now.entry((local_day(r.ts), r.model.clone(), r.cwd.clone()))
                .or_default()
                .add(&r.usage);
        }
        let totals = |m: &HashMap<DayKey, Usage>| {
            let mut t: HashMap<NaiveDate, u64> = HashMap::new();
            for ((d, ..), u) in m {
                *t.entry(*d).or_default() += u.total();
            }
            t
        };
        let (now_t, old_t) = (totals(&now), totals(&self.archive));

        // Finished days with more data than saved: update the history.
        let better: Vec<NaiveDate> = now_t
            .iter()
            .filter(|&(d, t)| *d < today && *t > old_t.get(d).copied().unwrap_or(0))
            .map(|(d, _)| *d)
            .collect();
        if !better.is_empty() {
            self.archive.retain(|(d, ..), _| !better.contains(d));
            for (k, u) in &now {
                if better.contains(&k.0) {
                    self.archive.insert(k.clone(), *u);
                }
            }
            self.save_archive();
        }

        // What's saved but no longer in the transcripts.
        let noon = NaiveTime::from_hms_opt(12, 0, 0).unwrap();
        self.archived = self
            .archive
            .iter()
            .filter_map(|((d, model, cwd), u)| {
                let missing = u.minus(
                    &now.get(&(*d, model.clone(), cwd.clone()))
                        .copied()
                        .unwrap_or_default(),
                );
                if missing.total() == 0 {
                    return None;
                }
                Some(Rec {
                    ts: d
                        .and_time(noon)
                        .and_local_timezone(Local)
                        .earliest()?
                        .with_timezone(&Utc),
                    session: String::new(),
                    agent: None,
                    model: model.clone(),
                    cwd: cwd.clone(),
                    usage: missing,
                })
            })
            .collect();
    }

    fn save_archive(&self) {
        let Some(path) = &self.archive_path else {
            return;
        };
        let mut rows: Vec<DayRow> = self
            .archive
            .iter()
            .map(|((day, model, cwd), usage)| DayRow {
                day: *day,
                model: model.clone(),
                cwd: cwd.clone(),
                usage: *usage,
            })
            .collect();
        rows.sort_by(|a, b| (a.day, &a.model, &a.cwd).cmp(&(b.day, &b.model, &b.cwd)));
        if let Some(dir) = path.parent() {
            let _ = fs::create_dir_all(dir);
        }
        // Write elsewhere and rename: an interrupted write can't lose the history.
        let tmp = path.with_extension("json.tmp");
        if let Ok(json) = serde_json::to_string_pretty(&rows)
            && fs::write(&tmp, json).is_ok()
        {
            let _ = fs::rename(&tmp, path);
        }
    }

    fn scan_projects(&mut self) {
        let Ok(projects) = fs::read_dir(self.root.join("projects")) else {
            return;
        };
        for dir in projects.flatten().map(|e| e.path()) {
            let Ok(entries) = fs::read_dir(&dir) else {
                continue;
            };
            for p in entries.flatten().map(|e| e.path()) {
                let Some(stem) = p.file_stem().and_then(|s| s.to_str()).map(str::to_string) else {
                    continue;
                };
                if p.extension().is_some_and(|e| e == "jsonl") {
                    self.sessions
                        .entry(stem.clone())
                        .or_insert_with(|| Session {
                            id: stem.clone(),
                            path: p.clone(),
                            ..Default::default()
                        });
                    self.scan(&p, &stem, None);
                } else if p.is_dir() {
                    self.scan_agents(&p.join("subagents"), &stem);
                }
            }
        }
    }

    fn scan_agents(&mut self, dir: &Path, session: &str) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for p in entries.flatten().map(|e| e.path()) {
            if p.extension().is_none_or(|e| e != "jsonl") {
                continue;
            }
            let id = p.file_stem().and_then(|s| s.to_str()).unwrap_or_default();
            let id = id.trim_start_matches("agent-").to_string();
            let mtime = fs::metadata(&p)
                .and_then(|m| m.modified())
                .unwrap_or(SystemTime::UNIX_EPOCH);
            if let Some(a) = self.agents.get_mut(&id) {
                a.mtime = mtime;
            } else {
                let meta: Value = fs::read_to_string(p.with_extension("meta.json"))
                    .ok()
                    .and_then(|s| serde_json::from_str(&s).ok())
                    .unwrap_or_default();
                let field = |k: &str| meta[k].as_str().unwrap_or("?").to_string();
                self.agents.insert(
                    id.clone(),
                    Agent {
                        id: id.clone(),
                        session: session.to_string(),
                        kind: field("agentType"),
                        desc: field("description"),
                        path: p.clone(),
                        mtime,
                        done: false,
                    },
                );
            }
            self.scan(&p, session, Some(&id));
        }
    }

    /// Reads what was appended to the file since last time (a file rewritten
    /// shorter is reread whole: the dedupe avoids double counting).
    fn scan(&mut self, path: &Path, session: &str, agent: Option<&str>) {
        let mut off = self.offsets.get(path).copied().unwrap_or(0);
        new_lines(path, &mut off, |text| {
            if let Some(a) = agent.and_then(|a| self.agents.get_mut(a)) {
                a.done = text.contains("\"stop_reason\":\"end_turn\"");
            }
            self.track_shells(text, session, agent);
            self.ingest(text, session, agent);
        });
        self.offsets.insert(path.to_path_buf(), off);
    }

    /// Background commands: launched (Bash with `run_in_background`), started
    /// (its result carries `backgroundTaskId`) and finished (a
    /// `<task-notification>` with its status).
    fn track_shells(&mut self, line: &str, session: &str, agent: Option<&str>) {
        if line.contains("\"run_in_background\":true")
            && let Ok(v) = serde_json::from_str::<Value>(line)
        {
            for b in v["message"]["content"].as_array().into_iter().flatten() {
                if b["type"] == "tool_use" && b["input"]["run_in_background"] == true {
                    let input = &b["input"];
                    let desc = input["description"]
                        .as_str()
                        .or(input["command"].as_str())
                        .unwrap_or("?");
                    self.pending_shells.insert(
                        b["id"].as_str().unwrap_or_default().to_string(),
                        clip(desc.lines().next().unwrap_or(""), 60),
                    );
                }
            }
        }
        if line.contains("\"backgroundTaskId\"")
            && let Ok(v) = serde_json::from_str::<Value>(line)
            && let Some(id) = v["toolUseResult"]["backgroundTaskId"].as_str()
        {
            let block = &v["message"]["content"][0];
            let text = match &block["content"] {
                Value::String(s) => s.clone(),
                Value::Array(a) => a.iter().filter_map(|b| b["text"].as_str()).collect(),
                _ => String::new(),
            };
            // "…written to: <dir>/tasks/<id>.output. You will be notified…"
            let output = text
                .split_once("Output is being written to: ")
                .map(|(_, p)| match p.find(".output") {
                    Some(i) => &p[..i + ".output".len()],
                    None => p.lines().next().unwrap_or("").trim_end_matches('.'),
                })
                .unwrap_or_default();
            let desc = block["tool_use_id"]
                .as_str()
                .and_then(|t| self.pending_shells.remove(t))
                .unwrap_or_default();
            self.shells.entry(id.to_string()).or_insert(Shell {
                id: id.to_string(),
                session: session.to_string(),
                agent: agent.map(str::to_string),
                desc,
                output: PathBuf::from(output),
                done: false,
            });
        }
        if line.contains("<task-notification>") {
            for part in line.split("<task-id>").skip(1) {
                let Some((id, rest)) = part.split_once("</task-id>") else {
                    continue;
                };
                let status = rest
                    .split_once("<status>")
                    .and_then(|(_, s)| s.split_once("</status>"))
                    .map_or("", |(s, _)| s);
                if let Some(sh) = self.shells.get_mut(id)
                    && matches!(status, "completed" | "failed" | "killed" | "stopped")
                {
                    sh.done = true;
                }
            }
        }
    }

    fn ingest(&mut self, line: &str, session: &str, agent: Option<&str>) {
        let has_usage = line.contains("\"usage\"");
        let wants_title = agent.is_none()
            && line.contains("\"type\":\"user\"")
            && self
                .sessions
                .get(session)
                .is_some_and(|s| s.title.is_empty());
        // Cheap filter before parsing: most lines are of no interest.
        if !has_usage && !wants_title {
            return;
        }
        let Ok(l) = serde_json::from_str::<Line>(line) else {
            return;
        };
        let Some(ts) = l.timestamp else { return };

        if wants_title
            && l.kind == "user"
            && !l.meta
            && let Some(t) = first_prompt(line)
        {
            let s = self.sessions.get_mut(session).unwrap();
            s.title = t;
            if s.cwd.is_empty() {
                s.cwd = l.cwd.clone();
            }
        }

        let Some(msg) = l.message else { return };
        let Some(raw) = msg.usage else { return };
        // "<synthetic>" = error messages generated by Claude Code itself.
        if l.kind != "assistant" || msg.model.starts_with('<') {
            return;
        }
        let usage = Usage::from(raw);

        if agent.is_none()
            && !l.sidechain
            && let Some(s) = self.sessions.get_mut(session)
        {
            s.cwd = l.cwd.clone();
            s.branch = l.branch.clone();
            s.last = Some(ts);
            s.model = msg.model.clone();
            s.context = usage.context();
        }

        let id = msg.id.unwrap_or_default();
        if let Some(&i) = self.seen.get(&id) {
            // Repeated: keep the one with the most output (the most complete).
            if usage.output > self.recs[i].usage.output {
                self.recs[i].usage = usage;
            }
            return;
        }
        if !id.is_empty() {
            self.seen.insert(id, self.recs.len());
        }
        self.recs.push(Rec {
            ts,
            session: session.to_string(),
            agent: agent.map(str::to_string),
            model: msg.model,
            cwd: l.cwd,
            usage,
        });
    }
}

/// Calls `f` with each line appended to the file since `off`, and moves `off`
/// past them. Only complete lines (a last one without '\n' is still being
/// written: next time); memory stays bounded by the longest line. Returns
/// whether the file was rewritten shorter: then it's reread from the start.
pub fn new_lines(path: &Path, off: &mut u64, mut f: impl FnMut(&str)) -> bool {
    let Ok(file) = File::open(path) else {
        return false;
    };
    let len = file.metadata().map(|m| m.len()).unwrap_or(0);
    let shrunk = len < *off;
    if shrunk {
        *off = 0;
    }
    let mut r = BufReader::new(file);
    if len <= *off || r.seek(SeekFrom::Start(*off)).is_err() {
        return shrunk;
    }
    let mut line = Vec::new();
    loop {
        line.clear();
        match r.read_until(b'\n', &mut line) {
            Ok(n) if n > 0 && line.ends_with(b"\n") => {
                *off += n as u64;
                f(String::from_utf8_lossy(&line).trim_end());
            }
            _ => break,
        }
    }
    shrunk
}

/// What a store found, for the `real_home` tests run against a real home.
#[cfg(test)]
pub fn print_store(st: &Store) {
    let mut total = Usage::default();
    st.recs.iter().for_each(|r| total.add(&r.usage));
    println!(
        "{}: {} sessions, {} replies, {} tokens, {} subagents, {} open",
        st.root.display(),
        st.sessions.len(),
        st.recs.len(),
        total.total(),
        st.agents.len(),
        st.live.len()
    );
    let mut sessions: Vec<_> = st.sessions.values().filter(|s| s.last.is_some()).collect();
    sessions.sort_by_key(|s| std::cmp::Reverse(s.last));
    for s in sessions.iter().take(10) {
        println!(
            "  {} · {} · {} · ctx {}% · {}",
            s.last.unwrap().format("%Y-%m-%d %H:%M"),
            s.model,
            s.cwd,
            s.context_pct(),
            s.title
        );
    }
    for a in st.agents.values().take(10) {
        println!(
            "  agent {} · {} · {} · done {}",
            a.kind, a.desc, a.session, a.done
        );
    }
    for l in &st.live {
        println!("  OPEN {} · {} · {}", l.status, l.cwd, l.name);
    }
}

/// Text of the first human message (ignores those generated by Claude Code: `<command-…>`, caveats…).
fn first_prompt(line: &str) -> Option<String> {
    let v: Value = serde_json::from_str(line).ok()?;
    let c = &v["message"]["content"];
    let text = match c {
        Value::String(s) => s.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .find(|b| b["type"] == "text")
            .and_then(|b| b["text"].as_str())?
            .to_string(),
        _ => return None,
    };
    let text = text.trim();
    if text.is_empty() || text.starts_with('<') {
        return None;
    }
    Some(clip(text.lines().next().unwrap_or(""), 90))
}

/// Truncates to `n` characters (not bytes: there are accents).
pub fn clip(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(n - 1).collect::<String>())
    }
}

pub fn live_sessions(root: &Path) -> Vec<Live> {
    let Ok(entries) = fs::read_dir(root.join("sessions")) else {
        return Vec::new();
    };
    let mut out: Vec<Live> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "json"))
        .filter_map(|p| serde_json::from_str::<Live>(&fs::read_to_string(p).ok()?).ok())
        .filter(|l| platform::alive(l.pid, &l.proc_start))
        .collect();
    out.sort_by_key(|l| l.pid);
    out
}

pub fn claude_root() -> PathBuf {
    platform::home().join(".claude")
}

/// Subscription limits. Claude Code only passes them to the statusline
/// (`rate_limits` field); `cct --statusline` saves them here for the rest.
pub fn limits_path() -> PathBuf {
    platform::cache_dir().join("claudeterm/limits.json")
}

#[derive(Deserialize, Default)]
pub struct Limits {
    pub five_hour: Option<Limit>,
    pub seven_day: Option<Limit>,
    /// When they were saved (the file's mtime).
    #[serde(skip)]
    pub updated: Option<SystemTime>,
}

#[derive(Deserialize)]
pub struct Limit {
    #[serde(default)]
    pub used_percentage: f64,
    /// Unix seconds per the docs; ISO text is accepted too in case it changes.
    resets_at: Option<Value>,
}

impl Limit {
    pub fn resets(&self) -> Option<DateTime<Local>> {
        let t = match self.resets_at.as_ref()? {
            Value::Number(n) => DateTime::from_timestamp(n.as_i64()?, 0)?,
            Value::String(s) => s.parse::<DateTime<Utc>>().ok()?,
            _ => return None,
        };
        Some(t.with_timezone(&Local))
    }

    /// "resets at 21:00" / "resets Mon at 10:00" (or in Spanish).
    pub fn reset_text(&self) -> String {
        const DAYS_ES: [&str; 7] = ["lun", "mar", "mié", "jue", "vie", "sáb", "dom"];
        const DAYS_EN: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];
        let Some(r) = self.resets() else {
            return String::new();
        };
        let time = r.format("%H:%M");
        if r.date_naive() == Local::now().date_naive() {
            crate::tr!("se reinicia a las {time}", "resets at {time}")
        } else {
            let d = r.weekday().num_days_from_monday() as usize;
            crate::tr!(
                "se reinicia el {} a las {time}",
                "resets {} at {time}",
                tr(DAYS_ES[d], DAYS_EN[d])
            )
        }
    }

    /// Current percentage: 0 if the window has reset since it was saved.
    pub fn pct(&self) -> f64 {
        match self.resets() {
            Some(r) if r <= Local::now() => 0.0,
            _ => self.used_percentage,
        }
    }
}

pub fn read_limits() -> Option<Limits> {
    let path = limits_path();
    let mut l: Limits = serde_json::from_str(&fs::read_to_string(&path).ok()?).ok()?;
    l.updated = fs::metadata(&path).and_then(|m| m.modified()).ok();
    Some(l)
}

/// Start of the last 5-hour block. As in the subscription: the block starts at
/// the top of the hour of the first message and lasts 5 h; the first message
/// outside it opens another. `ts` must be sorted.
pub fn last_block_start(ts: &[DateTime<Utc>]) -> Option<DateTime<Utc>> {
    let mut start: Option<DateTime<Utc>> = None;
    for &t in ts {
        if start.is_none_or(|s| t >= s + Duration::hours(5)) {
            start = t
                .with_minute(0)
                .and_then(|t| t.with_second(0))
                .and_then(|t| t.with_nanosecond(0));
        }
    }
    start
}

/// The last `max` messages of a transcript, readable.
pub fn transcript(path: &Path, max: usize) -> String {
    // A freshly opened session has no transcript until the first message.
    let Ok(text) = fs::read_to_string(path) else {
        return tr(
            "(sesión sin mensajes todavía)",
            "(no messages in this session yet)",
        )
        .into();
    };
    let mut out: Vec<String> = Vec::new();
    for line in text.lines() {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if v["isMeta"] == true {
            continue;
        }
        let who = match v["type"].as_str() {
            Some("user") => tr("» tú", "» you"),
            Some("assistant") => "« claude",
            _ => continue,
        };
        let c = &v["message"]["content"];
        if let Some(s) = c.as_str() {
            if !s.starts_with('<') {
                out.push(format!("{who}: {}", clip(s.trim(), 600)));
            }
            continue;
        }
        for b in c.as_array().into_iter().flatten() {
            match b["type"].as_str() {
                Some("text") => {
                    let t = b["text"].as_str().unwrap_or("").trim();
                    if !t.is_empty() && !t.starts_with('<') {
                        out.push(format!("{who}: {}", clip(t, 600)));
                    }
                }
                Some("tool_use") => {
                    let i = &b["input"];
                    let what = [
                        "description",
                        "command",
                        "file_path",
                        "pattern",
                        "prompt",
                        "url",
                    ]
                    .iter()
                    .find_map(|k| i[k].as_str())
                    .unwrap_or("");
                    out.push(format!(
                        "   ⚙ {}: {}",
                        b["name"].as_str().unwrap_or("?"),
                        clip(what.lines().next().unwrap_or(""), 120)
                    ));
                }
                _ => {}
            }
        }
    }
    let skip = out.len().saturating_sub(max);
    if out.is_empty() {
        tr("(sin mensajes)", "(no messages)").into()
    } else {
        out[skip..].join("\n\n")
    }
}

/// `git worktree list --porcelain` → (path, branch). The first is the main repo.
pub fn parse_worktrees(porcelain: &str) -> Vec<(String, String)> {
    porcelain
        .split("\n\n")
        .filter_map(|block| {
            let mut path = None;
            let mut branch = "(detached)".to_string();
            for l in block.lines() {
                if let Some(p) = l.strip_prefix("worktree ") {
                    path = Some(platform::native_path(p));
                } else if let Some(b) = l.strip_prefix("branch ") {
                    branch = b.trim_start_matches("refs/heads/").to_string();
                } else if l == "bare" {
                    branch = "(bare)".into();
                }
            }
            Some((path?, branch))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(id: &str, ts: &str, out: u64) -> String {
        format!(
            r#"{{"type":"assistant","timestamp":"{ts}","sessionId":"s","cwd":"/p","gitBranch":"main","message":{{"id":"{id}","model":"claude-opus-5","content":[],"usage":{{"input_tokens":10,"output_tokens":{out},"cache_read_input_tokens":100,"cache_creation_input_tokens":40,"cache_creation":{{"ephemeral_5m_input_tokens":0,"ephemeral_1h_input_tokens":40}}}}}}}}"#
        )
    }

    fn store_with(lines: &[String]) -> Store {
        let dir =
            std::env::temp_dir().join(format!("cct-test-{}-{}", std::process::id(), lines.len()));
        let proj = dir.join("projects/-p");
        fs::create_dir_all(&proj).unwrap();
        let mut body = lines.join("\n");
        body.push('\n');
        fs::write(proj.join("s.jsonl"), body).unwrap();
        let mut st = Store::at(dir.clone());
        st.refresh();
        fs::remove_dir_all(dir).unwrap();
        st
    }

    #[test]
    fn dedupe_and_ignore_garbage() {
        let st = store_with(&[
            line("m1", "2026-01-01T10:00:00Z", 5),
            line("m1", "2026-01-01T10:00:00Z", 20), // same message, more complete
            "{this is not json".into(),
            line("m2", "2026-01-01T10:01:00Z", 7),
        ]);
        assert_eq!(st.recs.len(), 2);
        assert_eq!(st.recs[0].usage.output, 20);
        let s = &st.sessions["s"];
        assert_eq!(
            (s.cwd.as_str(), s.branch.as_str(), s.context),
            ("/p", "main", 150)
        );
    }

    /// Background shells: launched, started (with its output file) and finished.
    #[test]
    fn background_shells() {
        let dir = std::env::temp_dir().join(format!("cct-shells-{}", std::process::id()));
        let proj = dir.join("projects/-p");
        fs::create_dir_all(&proj).unwrap();
        let launch = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"toolu_1","name":"Bash","input":{"command":"npm run dev","description":"Dev server","run_in_background":true}}]}}"#;
        let started = r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"toolu_1","content":"Command running in background with ID: b1. Output is being written to: C:\\t\\tasks\\b1.output. You will be notified when it completes."}]},"toolUseResult":{"backgroundTaskId":"b1"}}"#;
        let done = r#"{"type":"user","message":{"content":"<task-notification>\n<task-id>b1</task-id>\n<status>completed</status>\n</task-notification>"}}"#;
        let file = proj.join("s.jsonl");
        fs::write(&file, format!("{launch}\n{started}\n")).unwrap();
        let mut st = Store::at(dir.clone());
        st.refresh();
        let sh = &st.shells["b1"];
        assert_eq!(
            (sh.desc.as_str(), sh.session.as_str(), sh.done),
            ("Dev server", "s", false)
        );
        assert_eq!(sh.output, PathBuf::from(r"C:\t\tasks\b1.output"));
        fs::write(&file, format!("{launch}\n{started}\n{done}\n")).unwrap();
        st.refresh();
        fs::remove_dir_all(dir).unwrap();
        assert!(st.shells["b1"].done);
    }

    #[test]
    fn partial_last_line_is_read_later() {
        let dir = std::env::temp_dir().join(format!("cct-partial-{}", std::process::id()));
        let proj = dir.join("projects/-p");
        fs::create_dir_all(&proj).unwrap();
        let file = proj.join("s.jsonl");
        let (l1, l2) = (
            line("m1", "2026-01-01T10:00:00Z", 5),
            line("m2", "2026-01-01T10:01:00Z", 7),
        );
        let (head, tail) = l2.split_at(40);
        fs::write(
            &file,
            format!(
                "{l1}
{head}"
            ),
        )
        .unwrap();
        let mut st = Store::at(dir.clone());
        st.refresh();
        assert_eq!(st.recs.len(), 1, "a half-written line must wait");
        let mut f = fs::OpenOptions::new().append(true).open(&file).unwrap();
        std::io::Write::write_all(
            &mut f,
            format!(
                "{tail}
"
            )
            .as_bytes(),
        )
        .unwrap();
        drop(f);
        st.refresh();
        fs::remove_dir_all(dir).unwrap();
        assert_eq!(st.recs.len(), 2);
    }

    #[test]
    fn cost_opus5() {
        let u = Usage {
            input: 1_000_000,
            output: 1_000_000,
            cache_read: 1_000_000,
            cache_5m: 1_000_000,
            cache_1h: 1_000_000,
        };
        // 5 + 25 + 0.5 + 6.25 + 10
        assert!((u.cost("claude-opus-5").unwrap() - 46.75).abs() < 1e-9);
        assert_eq!(price("claude-opus-5-5").unwrap().0, 4.0);
        assert!(u.cost("claude-unknown").is_none());
    }

    #[test]
    fn blocks_of_five_hours() {
        let t = |s: &str| s.parse::<DateTime<Utc>>().unwrap();
        let ts = [
            t("2026-01-01T10:20:00Z"),
            t("2026-01-01T14:59:00Z"),
            t("2026-01-01T15:05:00Z"),
            t("2026-01-01T16:00:00Z"),
        ];
        assert_eq!(last_block_start(&ts), Some(t("2026-01-01T15:00:00Z")));
        assert_eq!(last_block_start(&ts[..2]), Some(t("2026-01-01T10:00:00Z")));
        assert_eq!(last_block_start(&[]), None);
    }

    #[test]
    fn archive_fills_deleted_days() {
        let mut st = store_with(&[line("m1", "2026-01-01T10:00:00Z", 5)]);
        let day = local_day("2026-01-01T10:00:00Z".parse().unwrap());
        let now = st.recs[0].usage;
        // The history saved more than what's left (a transcript of that day was deleted).
        let saved = Usage {
            output: now.output + 100,
            ..now
        };
        st.archive
            .insert((day, "claude-opus-5".into(), "/p".into()), saved);
        // And a whole day that's no longer in any transcript.
        let old = local_day("2025-12-01T10:00:00Z".parse().unwrap());
        st.archive.insert(
            (old, "claude-opus-5".into(), "/q".into()),
            Usage {
                input: 7,
                ..Default::default()
            },
        );
        st.sync_archive();
        let mut got: Vec<(u64, &str)> = st
            .archived
            .iter()
            .map(|r| (r.usage.total(), r.cwd.as_str()))
            .collect();
        got.sort();
        assert_eq!(got, vec![(7, "/q"), (100, "/p")]);
    }

    #[test]
    fn limits_parse_and_reset() {
        let future = Utc::now().timestamp() + 3600;
        let l: Limits = serde_json::from_str(&format!(
            r#"{{"five_hour":{{"used_percentage":71,"resets_at":{future}}},"seven_day":{{"used_percentage":41,"resets_at":"2020-01-01T00:00:00Z"}}}}"#
        ))
        .unwrap();
        let five = l.five_hour.unwrap();
        assert_eq!(five.pct(), 71.0);
        let txt = five.reset_text();
        assert!(txt.starts_with("se reinicia") || txt.starts_with("resets"));
        // It has reset since it was saved: 0 %.
        assert_eq!(l.seven_day.unwrap().pct(), 0.0);
    }

    #[test]
    fn footprint_math() {
        // 1M output + 1M cache read = 1M + 20k equivalent tokens.
        let f = Usage {
            output: 1_000_000,
            cache_read: 1_000_000,
            ..Default::default()
        }
        .footprint();
        assert!((f.wh - 1020.0).abs() < 1e-6);
        assert!((f.water_ml - 2040.0).abs() < 1e-6);
        assert!((f.scaled(RANGE_HIGH).wh - 2040.0).abs() < 1e-6);
        assert!((f.co2_g - 377.4).abs() < 1e-6);
    }

    /// Against your real ~/.claude: `cargo test -- --ignored --nocapture real_totals`
    #[test]
    #[ignore]
    fn real_totals() {
        let t = std::time::Instant::now();
        let mut st = Store::new();
        st.refresh();
        let mut u = Usage::default();
        st.recs.iter().for_each(|r| u.add(&r.usage));
        println!(
            "TOTAL {} messages {} in {:?}",
            u.total(),
            st.recs.len(),
            t.elapsed()
        );
    }

    #[test]
    fn corrupt_history_is_kept_aside() {
        let dir = std::env::temp_dir().join(format!("cct-hist-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("history.json");
        assert!(load_history(&path).1, "missing file: ok to create");
        fs::write(&path, "[{\"day\":\"2026-01-01\"").unwrap();
        let (rows, writable) = load_history(&path);
        assert!(rows.is_empty() && writable);
        assert!(
            !path.exists(),
            "the corrupt file must have been moved aside"
        );
        // Not even UTF-8: also corrupt, also moved aside (not left unwritable).
        fs::write(&path, [0xff, 0xfe, 0x00]).unwrap();
        assert!(load_history(&path).1);
        assert!(!path.exists());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn worktrees_porcelain() {
        let out =
            "worktree /r\nHEAD abc\nbranch refs/heads/main\n\nworktree /r-x\nHEAD def\ndetached\n";
        assert_eq!(
            parse_worktrees(out),
            vec![
                (platform::native_path("/r"), "main".into()),
                (platform::native_path("/r-x"), "(detached)".into())
            ]
        );
    }
}
