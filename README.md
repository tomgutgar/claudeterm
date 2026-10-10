<img src="assets/logo.svg" width="96" alt="claudeterm logo" align="right">

# claudeterm

**A terminal dashboard for Claude Code, Antigravity, Codex and Kimi Code.** A TUI (`cct`) that shows at a glance which Claude Code, Antigravity CLI (`agy`), OpenAI Codex CLI (`codex`) and Kimi Code CLI (`kimi`) sessions you have open, which ones are working and which are waiting for your reply, which subagents and background shells they launched, which git worktree each one is in, and how many tokens (and how much API-equivalent money) you've used.

Little sibling of [DevTerminal](https://github.com/tomgutgar/DevTerminal): same skeleton, same keys. Runs on **Linux** and **Windows**.

## How it works

Claude Code keeps everything it does in `~/.claude/`. claudeterm **only reads** those files (apart from its own history, see below): no network, no API keys, and it never touches Claude's data.

| File | What's taken from it |
|---|---|
| `~/.claude/sessions/<pid>.json` | Which `claude` processes are running right now and whether they're **working** or **waiting** |
| `~/.claude/projects/<dir>/<session>.jsonl` | Each session's history: messages, model, git branch and the **tokens** of every reply |
| `…/<session>/subagents/agent-*.jsonl` + `.meta.json` | The **subagents** each session launched (type, purpose, tokens) |
| `git worktree list` | The **worktrees** of the repos where you've used Claude |

Each refresh (every 2 s) only reads what's new in each file, so it starts fast even with a long history.

Background shells (Bash run with `run_in_background`) are taken from the transcripts too: the result that starts them, the `<task-notification>` that ends them, and their output file (if it changed in the last 5 s, the shell is working).

### Antigravity

Antigravity CLI keeps its data in `~/.gemini/antigravity-cli/`, also read-only:

| File | What's taken from it |
|---|---|
| `log/cli-<date>.log` | One per `agy` process: its pid, workspace and conversation. A process still alive and without "shutting down" in its log is an **open session**; it counts as **working** while its log is being written or its last step is still running |
| `conversations/<id>.db` | SQLite with protobuf rows: workspace, branch, model, **tokens** of every reply and the status of each step |
| `annotations/<id>.pbtxt` | The conversation's title |
| `history.jsonl` | Your prompts (the "transcript" of an Antigravity session) |

These formats aren't documented: the field numbers were worked out from real files (agy 1.3), and anything unexpected is skipped instead of breaking.

### Codex

Codex CLI keeps its data in `~/.codex/` (or `$CODEX_HOME`), read-only. Both CLIs below are open source, so their formats were taken from their code ([openai/codex](https://github.com/openai/codex), [MoonshotAI/kimi-code](https://github.com/MoonshotAI/kimi-code)), and the tests use records shaped like their own test fixtures.

| File | What's taken from it |
|---|---|
| `sessions/YYYY/MM/DD/rollout-<time>-<id>.jsonl` | One per thread: directory, branch, model, your messages and the **tokens** (`token_count` events, cumulative: each reply is the growth). A thread with a `session_id` other than its own is a **subagent** of that session. After a week codex compresses them (`.jsonl.zst`), also read; `archived_sessions/` too |
| `thread-writer-locks/<id>.lock` | Locked while a `codex` process has that thread open: an **open session**. It counts as **working** while a turn is running (`task_started` without `task_complete`) or the rollout is being written |

### Kimi Code

Kimi Code CLI keeps its data in `~/.kimi-code/` (or `$KIMI_CODE_HOME`), read-only:

| File | What's taken from it |
|---|---|
| `session_index.jsonl`, `sessions/<dir>/<id>/state.json` | Each session's directory and title |
| `…/<id>/agents/main/wire.jsonl` | The session's events: the **tokens** of every model request (`usage.record`), turns starting and ending (**working** in between), and the **subagents** it launched (`subagent.spawned`…) |
| `…/<id>/agents/agent-N/wire.jsonl` | Each subagent's own tokens |
| `…/<id>/tasks/<task>.json` + `output.log` | Its **background shells** |

Kimi writes no pid or lock per session, so the open ones are a guess: as many sessions as `kimi` processes are running, the most recently written since the oldest of those processes started.

## Home screen and agent hub

`cct` opens on a home screen with two tabs:

1. **Home**: the four CLIs' logos in a 2×2 grid (Claude Code, Antigravity and Kimi drawn from their own startup banners; Codex as OpenAI's ChatGPT logo), with how many sessions of each are working / waiting. Arrows and `Enter` (or a double click) open that CLI's dashboard; `Esc` (or the **← Esc home** button) comes back. `cct --claude` / `--agy` / `--codex` / `--kimi` skip it.
2. **Hub**: a live tree of everything running. This PC at the root; under it every open session (Claude's orange mascot, Antigravity's blue logo, Codex's green ChatGPT logo, Kimi's light-blue logo); under each session its **subagents** (its CLI's logo, same colour) and **background shells** (a small white monitor); one level further down, shells launched by a subagent show as tags `S1`, `S2`…
   - Each branch has the colour of its child. **Waiting**: a solid line. **Working**: a dotted line whose dots march from parent to child.
   - Icons move too: working, all the time (the mascot types, the Antigravity logo floats, Kimi's bobs, Codex's floats, the shell's cursor runs); waiting, a short twitch every 5 s.
   - Arrows walk the tree (`←`/`→` in order, `↑` parent, `↓` first child); the bottom line describes the selection; `Enter` opens its CLI's dashboard.

### Its own history

Claude Code **deletes transcripts older than 30 days** (the `cleanupPeriodDays` setting), and your usage history would go with them. So claudeterm keeps a per-day summary by model and directory in `~/.local/share/claudeterm/history.json` (on Windows, `%APPDATA%\claudeterm\history.json`) and adds it up in the Usage tab. That's the **only thing it writes**. If the file ever gets corrupted, it is renamed to `history.json.bad-<date>` instead of being overwritten.

To avoid gaps, open `cct` at least once a month, or raise Claude Code's limit in `~/.claude/settings.json`: `"cleanupPeriodDays": 365`.

## Tabs

| # | Tab | What it shows |
|---|---|---|
| 1 | **Sessions** | Open ones on top: ● working / ◌ waiting, title, directory, branch, model, **% of context** used, tokens (`+X ag` = its subagents') and cost. Below them, past sessions titled with their first message |
| 2 | **Agents** | Subagents of every session, newest first (● = active in the last 30 s), with tokens and cost |
| 3 | **Worktrees** | Repos with sessions in the last 7 days, their worktrees and which live session is in each one |
| 4 | **Usage** | **% used of the 5-hour and weekly limits** (with the time each one resets), tokens and cost of the current block, today / 30 days / total, daily cost chart, tables by model and by project, and an estimated energy/water/CO₂ footprint |

Antigravity's dashboard has the same tabs except **Agents** (agy doesn't record subagents). Antigravity's, Codex's and Kimi's Usage tabs show tokens only (there's no price table for their models) and have no limits.

Also, **even while you're on another tab**, when a session goes from working to waiting you get a desktop notification ("Claude Code is waiting for you" / "Antigravity is waiting for you").

## Statusline and limit percentages

Your limit percentages (5-hour and weekly) **aren't stored in any file**: Claude Code only hands them to the *statusline*, the line under the prompt. That's why `cct` doubles as a statusline. It saves the figures in `~/.cache/claudeterm/limits.json` (on Windows, `%LOCALAPPDATA%\claudeterm\limits.json`) and prints something like:

```
Opus 5.5 · ctx 29% · 5h 71% · 7d 41%
```

Enable it in `~/.claude/settings.json`:

```json
"statusLine": { "type": "command", "command": "/home/YOUR_USER/.cargo/bin/cct --statusline", "padding": 0 }
```

On Windows: `"command": "C:\\Users\\YOUR_USER\\.cargo\\bin\\cct.exe --statusline"` (or wherever you put `cct.exe`).

The figures update after every reply from any session. With no session open, the last known value is shown (0 % once its reset time has passed).

## Estimated footprint (electricity, water, CO₂)

The Usage tab shows an **order-of-magnitude approximation** for the 5-hour block, the week and the total. **Anthropic doesn't publish this data**, so it's estimated like this (constants in `src/data.rs`, next to `Footprint`):

| Step | Value | Source / assumption |
|---|---|---|
| Energy per output token | **1 mWh** (range 0.3–2 mWh) | [Photons = Tokens, arXiv 2026](https://arxiv.org/pdf/2603.06630): 0.1–2 mWh/token depending on size · [Muxup 2026q1](https://muxup.com/2026q1/per-query-energy-consumption-of-llms): DeepSeek-R1 671B ≈ 1.9 mWh (fp8) / 0.35–0.6 (fp4) · [Joule 2026](https://www.sciencedirect.com/science/article/pii/S2542435126001145) (peer-reviewed): median 0.31 Wh/query for >200B models, warning that public figures overstate ×4–20 → low bound · [Claude Code estimate, Mar. 2026](https://gist.github.com/mdodkins/9b49624855cc41570c9d1012e0d5d157): 1.95 mWh → high bound |
| Weight of other tokens | input ×0.2 · cache write ×0.25 · cache read ×0.02 | Assumption: compute is proportional to API price. The 2026 Claude Code estimate uses the same ratios |
| Water | 2 mL per Wh | Data-centre cooling (≈1.1 mL/Wh, [Google, Aug. 2025](https://arxiv.org/pdf/2508.15734)) + water used to generate the electricity; 2 mL/Wh is the mid value of the 2026 Claude Code estimate (regional ranges from 1.8 to 12 L/kWh) |
| CO₂e | 370 g per kWh | EIA: US grid average in 2023 (≈0.81 lb/kWh) |
| Equivalences | phone charge 15 Wh · car 150 g CO₂/km · glass 250 mL | Typical values |

The central figure is shown with the min–max range in parentheses (×0.3 to ×2). Treat it as a rough indication, not a measurement (sources reviewed in September 2026).

## Compact mode (`cct --mini`)

A small **always-on-top** window with the logo and the 5-hour and weekly limits (with their reset times). It updates every 2 s; `q` or `Esc` closes it. It needs the cct statusline to have data.

- **Windows:** `install.ps1` puts a **claudeterm mini** shortcut on the desktop that opens a 48×6 Windows Terminal window. `cct` keeps it on top by itself.
- **Linux:** open it in a small terminal window (48×6) and keep it floating and on top with your window manager's rules.

## Waybar module

`cct --waybar` prints JSON for a Waybar module: `✳ 1● 2◌ · 5h 71%`. Hovering shows the sessions with their titles and both limits. It's orange normally, yellow if a session is waiting for you and red if the 5-hour limit is above 80 %.

```jsonc
"custom/claude": {
    "exec": "~/.cargo/bin/cct --waybar",
    "return-type": "json",
    "interval": 3,
    "on-click": "kitty -e ~/.cargo/bin/cct"
}
```

Style it in `style.css` with `#custom-claude`, `#custom-claude.idle` and `#custom-claude.warn`.

### About cost

It's the **API-price equivalent** (what it would cost paying per token). With a Pro/Max subscription you don't pay that, but it's handy to compare sessions and models. Prices live in a table in `src/data.rs` (`PRICES`); a new model needs one more line. A model missing from the table counts tokens but no cost.

## Keys

| Key | Action |
|---|---|
| `Tab` / `D` · `Shift+Tab` / `A` | Next · previous tab |
| `1`–`4` | Jump to a tab |
| `Esc` / `Backspace` | Back to the home screen (from a dashboard) |
| `W` / `S` (or arrows), `PgUp` / `PgDn` | Move through the list |
| `/` or `F` | Filter the list (`Esc` clears the filter) |
| `y` | Copy the selection (session id, path…) |
| `R` | Refresh now |
| `Q` | Quit |

**Mouse:** click a tab to switch, click a row to select it, double-click = `Enter`, wheel to scroll lists and transcripts, click outside a floating window to close it, and click **confirm** / **cancel**. Since claudeterm captures the mouse, hold `Shift` to select text by dragging (kitty, Windows Terminal and most terminals).

Per-tab keys (also shown in the bottom bar):

| Tab | Key | Action |
|---|---|---|
| Sessions | `Enter` | View the transcript (opens at the end; `g`/`G` start/end) |
| | `o` | Resume the session in a new terminal (`claude --resume` / `agy --conversation` / `codex resume` / `kimi --session`); if it's already open, it tells you instead of running it twice |
| | `n` | Open a new Claude (or agy, codex, kimi) in that directory |
| | `k` | Close the session (asks for confirmation; Codex and Kimi store no pid, so not for them) |
| Agents | `Enter` | View the subagent's transcript |
| Worktrees | `n` | Open Claude in that worktree |
| | `c` | Create a new worktree (asks for the branch name; created at `<repo>-<branch>`, with `/` turned into `-`) |
| | `x` | Delete the worktree (asks for confirmation; git refuses if there are uncommitted changes) |
| Usage | `v` | Change the table period: today / 7 days / 30 days / all |

## Installation

### Prebuilt binaries

Download `cct` for Linux or Windows (x86_64) from the [releases page](https://github.com/tomgutgar/claudeterm/releases), extract it and put it somewhere on your `PATH`.

### From source

You need Rust (rustup) ≥ 1.88.

```bash
cargo install --git https://github.com/tomgutgar/claudeterm   # puts `cct` in ~/.cargo/bin
cct
```

On **Windows**, `install.ps1` does the same from a clone and also puts desktop shortcuts **claudeterm** and **claudeterm mini** (with the logo) that open it in Windows Terminal:

```powershell
git clone https://github.com/tomgutgar/claudeterm.git
cd claudeterm
powershell -ExecutionPolicy Bypass -File install.ps1
```

### Optional tools

- **Linux:** a terminal to open sessions in (`kitty` by default; pick another with `CCT_TERMINAL`, e.g. `export CCT_TERMINAL="alacritty -e"` or `"gnome-terminal --"`; cct tells you if it can't find it), `notify-send` (notifications), `wl-copy` or `xclip` (copy) and `git` (worktrees).
- **Windows:** `git`. Sessions open in Windows Terminal (`wt`) with the Command Prompt profile, notifications are Windows toasts and copying uses `clip`. Building with Rust's GNU toolchain needs mingw-w64 on the PATH; the default MSVC toolchain works as is.

## Language

The UI is in **English** or **Spanish**, picked from `LC_ALL` / `LC_MESSAGES` / `LANG` (`es_*` → Spanish; anything else → English) or, if none is set (the usual case on Windows), from the Windows display language. Force it with `CCT_LANG=en cct` or `CCT_LANG=es cct`.

## Tests

```bash
cargo test
# Check the totals against your real ~/.claude:
cargo test -- --ignored --nocapture real_totals
# What it reads from a real ~/.codex or ~/.kimi-code (or $CODEX_HOME / $KIMI_CODE_HOME):
cargo test -- --ignored --nocapture codex::tests::real_home
cargo test -- --ignored --nocapture kimi::tests::real_home
```

## Known limitations

- The format of the files in `~/.claude` is not a public Claude Code API: an update may change it. The reader skips what it doesn't understand instead of breaking.
- A subagent can't be closed on its own: it runs inside its session's `claude` process.
- Limit percentages only arrive through the statusline: without it, Usage shows tokens but no %.
- Antigravity's working / waiting is inferred (log activity and the last step's status), and its token fields are read from undocumented protobuf: a new agy version may need an update in `src/agy.rs`.
- Which Kimi sessions are open is a guess (see above): a resumed session shows up after its first message, and a `kimi` installed with npm runs as `node` and isn't seen.
- Codex and Kimi formats change with their versions (checked against their code in October 2026): an update may need a change in `src/codex.rs` / `src/kimi.rs`.
- A background shell killed without Claude Code noticing (e.g. the session crashed) stays in the hub until its session closes.

## Logo

The claudeterm logo (`assets/logo.svg`) is **original**: a terminal with the prompt in orange. In the Usage tab it's drawn with characters (half blocks), so it works in any terminal without an image protocol.

The home screen and the hub draw the Claude Code mascot, the Antigravity logo and the Kimi Code logo exactly as each CLI prints them in its own terminal banner, and Codex as OpenAI's logo (from Simple Icons), only to tell them apart. They are trademarks of Anthropic, Google, OpenAI and Moonshot AI respectively; claudeterm isn't affiliated with any of them. The PNGs and the ICO are rebuilt with `uv run --with playwright --with pillow python assets/build.py`.

claudeterm is an independent project, not affiliated with or endorsed by Anthropic.

## License

[GPL-3.0-or-later](LICENSE).
