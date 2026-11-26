# herdr-triage

An agent picker for [Herdr](https://herdr.dev) with T3 Code-style triage. Finished threads can be
**settled** out of the way, **snoozed** until later, marked **unread**, or **pinned** to the top.
Settling an agent closes its pane, unless it is the last pane in its workspace or Herdr can neither
tell its state nor resume it. If Herdr can resume
its session (Claude, OpenCode, Pi, or Codex run with `--no-daemon`), snoozing closes it too, and the
thread stays in the picker and reopens with its conversation
when you unsettle, wake or open it. Herdr's native Agents sidebar
follows the same order and hides parked threads.

![The triage picker: a pinned agent first, then the blocked, done, idle and unread threads that need you, with working, snoozed and settled agents in collapsed sections below](docs/picker.png)

## Install

Requires Herdr 0.9.1+, Git, a Rust toolchain (`cargo`) and a C compiler (for the bundled SQLite;
Xcode Command Line Tools on macOS) on Linux or macOS.

```sh
herdr plugin install aryan-binazir/herdr-triage
```

Bind the actions in `~/.config/herdr/config.toml`:

```toml
[keys]
next_agent = "prefix+]"        # follow the triage order
previous_agent = "prefix+\\"

[[keys.command]]
key = "prefix+a"
type = "plugin_action"
command = "ar.triage.open"
description = "agent triage picker"

[[keys.command]]
key = "prefix+w"
type = "plugin_action"
command = "ar.triage.next"
description = "next agent that needs you"
```

To show pin and unread marks in the sidebar, add the `$triage` token to your agent rows, for example:

```toml
[ui.sidebar.agents]
rows = [
  ["state_icon", "machine", "workspace", "tab"],
  ["agent", { token = "$triage", rules = [
    { contains = "unread", bold = true },
    { contains = "woke", bold = true },
  ] }],
]
```

## Picker keys

| Key | List | While typing a search |
| --- | --- | --- |
| `j`/`k`, `↑`/`↓` | move (wraps) | `^j`/`^k`, `↑`/`↓` move |
| `enter` | focus agent · reopen closed thread · open/close section | same |
| `1`–`9`, `0` | open row 1–10 on screen | typed |
| `/` | search (edits the kept search) | — |
| `s` | settle / unsettle (asks first if it would close an agent for good) | typed |
| `z` | snooze (1h, 3h, today 18:00, tomorrow 09:00, next Mon 09:00; press the digit shown beside one) / wake | typed |
| `u` | mark read / unread | typed |
| `p` | pin / unpin | typed |
| `r` | rename the thread (`enter` saves; an empty title restores the automatic one) | typed |
| `U` | undo the last triage change or rename (the footer names it, e.g. `U undo settle “docs pass”`) | typed |
| `n` | new agent where the selected thread lives: `1`–`4` start claude, codex, opencode or pi in a new tab; `w` first asks for a branch and starts it in a new worktree | typed |
| `D` | forget a closed thread (asks `y`) | typed |
| `esc` | clear the search, else close | leave search, keeping it |
| `q` | close (cancels an open prompt) | typed |
| `^u` / `^w` | — | clear / delete word |

The picker opens on the agent you invoked it from. The Workspace column names each thread's project:
a Herdr worktree workspace leads with its repository (`acme-api/auth-fix`), a T3 thread shows its T3
project. Agents busy working (unless pinned) collect in a
`▸ working · N` section below the threads that need you; snoozed and settled threads collect in
`▸ snoozed · N` and `▸ settled · N` sections at the bottom. Sections stay collapsed until you open
them, and the picker remembers which are open. A search looks inside them too. After you settle or
snooze a thread, the cursor moves on to the next one that needs you: blocked first, then done or
unread. `z` swaps the footer for a snooze prompt; pick a time with the digit beside it, or `esc`/`q`
to cancel. Each time keeps its digit: from 30 minutes before 18:00, `today 18:00` is crossed out
and refused. A time that has passed while the prompt was open is refused too:

![The snooze prompt in the picker footer: 1 1h, 2 3h, 3 today 18:00, 4 tomorrow 09:00, 5 next Mon 09:00, esc cancel](docs/snooze.png)

## Triage rules

- **Order:** pinned → open threads by most recent status change → snoozed (soonest to wake first) →
  settled. Closed threads follow live ones by last activity.
- **Pin and settle** exclude each other, as in T3 Code: settling unpins a thread, and pinning a
  parked thread brings it back. A parked thread sinks even when pinned.
- **Blocked, working, done, idle** are Herdr's own states. *Done* means finished and not yet viewed.
- **Settle** (`s`) marks the thread read and parks it until it does something new (Herdr's change
  counter advancing).
- **Snooze** (`z`) parks it until the chosen time; then it returns marked *woke*. The picker shows
  the time left (`snoozed · 45m`). An agent that blocks on you wakes early, and so does one you go to
  from another agent; other activity, including a snoozed agent finishing its work, does not.
  Snoozing the agent you are on and closing the picker leaves it snoozed. Herdr shows a
  notification when a snooze comes due (`Woke: <thread>`). Viewing the agent clears *woke*.
- **Closing:** settling an agent closes its pane. If Herdr can resume its session (Claude, Codex,
  OpenCode or Pi with Herdr's integration), the thread stays listed as closed and snoozing closes
  it too. Settling an agent with no session to resume first asks: `y` closes it for good (Herdr
  notifies you, the thread leaves the picker and undo can't bring it back), `n` settles it and
  keeps its pane open. Snoozing one, or settling one whose
  state Herdr can't tell (`unknown`), leaves it open, and so does the last pane of a workspace;
  these are marked `stays open`. Snoozing a resumable agent that is working or blocked closes it
  once its turn ends (`snoozed · 45m · closes when done`), but never while you are on it: then it
  closes once you are on another agent. Codex only reports its session to Herdr when it runs
  without its shared app-server: start it with `codex --no-daemon` (or alias `codex` to that).
- **After a Herdr restart,** Herdr relaunches restored agents; state changes in the first 20 seconds
  are not treated as new activity, so parked threads stay parked.
- **Reopening:** unsettling, waking or opening a closed thread starts a new tab in its workspace (or
  the current one, if that workspace is gone) and runs `claude --resume <id>`, `codex resume <id>`,
  `opencode --session <id>` or `pi --session <id>`. A closed snooze that comes due shows as
  `woke · closed` and waits for you. OpenCode uses its configured server and creates a session if
  the saved ID no longer exists; reopening does not recover deleted conversation history.
  Herdr accepting the launch command does not guarantee that the application starts successfully;
  application errors remain visible in the new tab.
- **Unread** (`u`) on a done or unread thread marks it read without visiting it; on any other it marks
  it unread until you press `u` again, the agent's next turn, or you read it: go to another agent and
  come back to it, or reopen it if it was closed. Marking a parked thread unread brings it back
  without reopening it.
- A thread whose session ends some other way settles as closed, even if you hadn't seen its result,
  unless you marked it unread or a snooze woke it: then it stays open as `unread · closed` or
  `woke · closed`. Settling a working agent settles it when its turn ends (`working · settles
  when done`, closing its pane then if it can be resumed; `s` again calls it off), unless the turn
  ends blocked on you. Settle is refused while an agent is blocked, unread while it is working or
  blocked; snooze works in any state.
- **Undo** (`U`) takes back the last settle, snooze, wake, unread, pin or rename, once. It never closes
  a pane; undoing a settle or snooze that closed a session reopens it, unless it closed for good.
  Once the agent has done
  something new, undo can still bring a thread back but won't park it again. Forgetting a thread
  can't be undone.
- **Rename** (`r`) gives a thread its own title in the picker; search still finds the automatic one.
- **`next`** focuses blocked agents first, then done, unread or woke ones, reopening a closed thread
  among them; it skips parked threads.

Closed threads are forgotten after 30 days untouched unless pinned; `D` forgets one now. Agents
without a resumable session are forgotten once they exit.

## T3 Code threads

If [T3 Code](https://github.com/pingdotgg/t3code) runs on this machine, the picker also lists its
threads, so one list shows everything that needs you. A T3 thread still lives only in T3: its row
points at it, and `enter` (or its digit) opens it in the T3 desktop app through its
`t3code://threads/…` link (`xdg-open` on Linux, `open` on macOS).

- Rows show T3's own state in the picker's words: running is *working*, waiting on you is
  *blocked*, a finished run you haven't opened since is *done*, otherwise *idle*. T3's settles,
  snoozes and pins place the row as Herdr's do; working threads join the `working` section.
- T3 rows mix with Herdr's by rank and most recent change, ahead of closed Herdr threads of the
  same rank; snoozed rows of either kind wake in order. Settled T3 threads untouched for 30 days are left out. Subagent and side-chat
  threads stay under their parent, as in T3.
- `s`, `z`, `p` and `u` on a T3 row have T3 settle or unsettle, snooze (same presets) or wake,
  pin or unpin, and mark unread the thread; the row moves once T3 has done it. T3 decides what it
  allows and the footer shows its answer, for example that a thread with no finished run can't be
  marked unread, or that settling a working thread waits for its turn to end. T3 marks a thread
  read when you open it. Rename and forget T3 threads in T3; `U` undoes Herdr changes only.
  `next` and the sidebar cover Herdr's agents only.
- The first change signs herdr-triage in to T3, with no browser: the local `t3` command issues a
  one-time pairing code (`t3 auth pairing create`), which approves herdr-triage at T3's `auto`
  access level. T3 then issues a 30-day token, kept owner-only in the plugin state directory
  (`t3-token.json`); when it expires or is revoked, the next change signs in again. `auto` lets
  herdr-triage change threads that run in `approval-required` to `auto` mode; T3 refuses changes
  to `full-access` threads. The token can also message threads, so treat the file like a T3
  login. See and revoke it with `t3 auth session list` and `t3 auth session revoke <id>`.
- The picker reads T3's database (`$T3CODE_HOME`, else `~/.t3`, at `userdata/statev2.sqlite`)
  read-only, every 2 seconds, on a thread of its own, which also sends the picker's changes to
  T3's server. Only this machine's T3 threads appear, not those of other T3 environments the app
  is connected to. If T3 isn't installed, isn't running, or an update reshapes its database, the
  picker shows no T3 rows (or keeps the last ones it read) and T3 changes fail with a message;
  Herdr's rows are unaffected.

## MCP server

`herdr-triage mcp` is a [Model Context Protocol](https://modelcontextprotocol.io) server, so an
agent can start other agents in Herdr, message them, read them and triage them. Every agent it
starts is an ordinary thread: you see it in the picker and sidebar and can take it over at any
time. It speaks MCP over stdin and stdout; there is no daemon, port or login. Inspired by
[T3 Code](https://github.com/pingdotgg/t3code)'s MCP server.

| Tool | What it does |
| --- | --- |
| `herdr_projects` | The projects open in Herdr, one per repository, with their workspaces and what a launch there starts |
| `herdr_capabilities` | The agents it can launch and the models and efforts they default to |
| `herdr_launch` | Start an agent in a new Git worktree (its own workspace) of a project open in Herdr, optionally with its first message |
| `herdr_send` | Message an open thread's agent between turns |
| `herdr_read` | The recent text on a thread's terminal |
| `herdr_wait` | Wait until a thread's agent stops working |
| `herdr_threads` | Threads with their triage state, the caller's own first |
| `herdr_organize` | Everything the picker does to a thread: settle, unsettle, snooze, wake, reopen, pin, unpin, mark read or unread, rename, undo, forget |

### Connect a client

For Claude Desktop, the ChatGPT desktop app and the Codex command, quit the apps, run the
plugin's **Connect the MCP server to Claude Desktop, ChatGPT and Codex** action once per machine,
and start them again:

```sh
herdr plugin action invoke connect-mcp --plugin ar.triage
```

It adds a `herdr` server to each one whose settings folder is on the machine (open a newly
installed app once first) and leaves the rest of the settings as they were: `mcpServers` in
Claude's `claude_desktop_config.json`, and `[mcp_servers.herdr]` in `~/.codex/config.toml`
(`$CODEX_HOME` if set to an absolute path). The ChatGPT app and the Codex command share that
file, so every Codex session gets the tools, including the agents this server launches. Herdr
shows a notification saying what it connected. Running it again is safe; it refuses a `herdr`
entry that is some other server, and keeps the file's owner, group and mode.

The server is the plugin's own binary. Each plugin update installs it in a new directory, so
Herdr's hooks keep a link to the current one at `~/.local/state/herdr/plugins/ar.triage/herdr-triage`
(under `$XDG_STATE_HOME` if you set it to an absolute path; a relative one is ignored, so leave
it out of the command below). It appears the first time one of the plugin's hooks runs
after an install, such as when you switch panes. The action points the apps at that link, so
they keep working across updates.

These apps start a server with little of your environment, so the action also writes where the
plugin keeps its settings and state (`XDG_CONFIG_HOME` and `HERDR_PLUGIN_STATE_DIR`) into each
entry: the server reads the same off switch and triage database as the plugin, whatever started
it. In Codex's entry, `env_vars` lets `HERDR_PANE_ID` and `HERDR_SOCKET_PATH` through, and
`tool_timeout_sec = 300` leaves room for a large repository's worktree checkout (Codex allows 60
seconds otherwise; a launch takes at most four minutes).

For Claude Code, register it once:

```sh
claude mcp add --scope user herdr -- "${XDG_STATE_HOME:-$HOME/.local/state}/herdr/plugins/ar.triage/herdr-triage" mcp
```

Any client that starts a stdio MCP server works the same way. These environment variables matter:

- `HERDR_PANE_ID`, which Herdr sets in every pane, tells the server which terminal is calling.
  What it launches is then its own (`mine`), and a launch with no `project` goes to the project
  it runs in. A client outside Herdr owns what it launches for as long as its session lasts, and
  has to name a project.
- `HERDR_SOCKET_PATH` picks the Herdr session to drive when you run more than the default one.
  The desktop apps drive the default session; to aim one at another, add the variable to the
  `env` of its `herdr` entry, which the connect action leaves in place.
- `XDG_CONFIG_HOME`, and `XDG_STATE_HOME` or `HERDR_PLUGIN_STATE_DIR`, if you set them for
  Herdr: a client that does not pass them on has the server read other settings, with the off
  switch in them, and another triage database.

### Turn it off on a machine

The server is on wherever the plugin is installed. To turn it off on one machine, set this in
`~/.config/herdr-triage/mcp.yaml` there:

```yaml
enabled: false
```

A client that starts the server is then offered no tools, a tool called anyway is refused, and
the connect action does nothing. The file is read on every tool call, so a running server
refuses the next one; a launch already under way finishes. A client lists the tools when it
starts, so it shows the change, either way, after a restart. Set it back to `true`, or delete the
line, to turn the server on again. It is a switch for you, not a lock against an agent that can
edit your files.

### What it launches

`~/.config/herdr-triage/mcp.yaml` (under `$XDG_CONFIG_HOME` if set, on Linux and macOS alike). The
server writes this file with its defaults the first time it runs, as does the connect action, and
reads it on every tool call:

```yaml
# false turns the server off on this machine: a client that starts it is offered no tools.
enabled: true

# The agent to launch: pi, claude or codex.
default_harness: pi

# Per agent: its model and effort, and any extra command-line arguments.
# Left out, pi starts on claude-opus-5-5 at medium; claude and codex on their own defaults.
harnesses:
  pi:
    model: claude-opus-5-5      # any pattern `pi --model` accepts
    effort: medium              # off, minimal, low, medium, high, xhigh, max
    args: []
  claude:
    # model: opus
    # effort: medium            # low, medium, high, xhigh, max
    args: []
  codex:
    # model: gpt-5.2-codex
    # effort: medium            # minimal, low, medium, high, xhigh
    # Codex only has a session Herdr can reopen when it runs with --no-daemon. Add it here
    # unless your `codex` command already passes it: Codex refuses the flag twice.
    args: []

# Per project (the repository's name, or the workspace's label): override any of the above.
projects: {}
#  acme-api:
#    harness: codex
#    model: gpt-5.2-codex
#    effort: high

worktree:
  # The ref new worktrees start from. Left out, Herdr picks.
  # base: origin/main
  # Prepended to the branch name made from the thread's title, e.g. `agent/`.
  branch_prefix: ""
```

The caller can name a `harness`, `model` or `effort` for one launch; otherwise the project's
settings apply, then the harness's, then the built-in defaults above, so a file that only sets
`worktree` still launches pi on Opus 5.5 at medium. They become `pi --model M --thinking E`,
`claude --model M --effort E` or `codex --model M --config model_reasoning_effort=E`, followed by
`args`. Put flags you always want there, such as a permission mode. A file that does not parse
stops every tool with the parse error, rather than launching something you did not ask for or
missing that you turned the server off.

### What to expect

- **Launching** asks Herdr for a worktree off the project's own checkout (or its repository root,
  when only linked worktrees are open, which makes Herdr open a workspace for that checkout too),
  on a new branch named after the title with a short random
  suffix (or on `branch`), starts the agent in the new workspace and types the message. Nothing
  takes your focus. Herdr decides where worktrees go (`~/.herdr/worktrees/<repository>/<branch>`
  unless you configured it otherwise), and the new workspace is named after the repository, so the
  picker shows the thread's title beside its project.
- **The project** must be open in Herdr. It can be a workspace id, a path inside the repository
  (Herdr resolves it with Git), or a repository name or workspace label; a name two projects
  answer to is refused rather than guessed.
- **A launch waits for the agent to come up**, usually a few seconds and at most 40. If its
  command exits instead, for example on a flag it does not know, the error carries what the pane
  printed.
- **A repository Herdr has not been told to trust** is not launched into: Herdr's refusal is passed
  on, and the server never grants that trust for you.
- **An agent that stops at a dialog of its own** before taking input, such as trusting a new
  directory, is reported as launched but not ready, and the message is not sent. The server never
  answers an agent that is blocked on an approval or a question, and tells the calling agent to
  ask you.
- **A launch that fails partway** leaves what it opened (the worktree, the workspace) in place and
  names it in the error, branch and pane included; nothing is deleted for you. Retried with the
  same `request_id`, it answers with that error again instead of opening more, even when Herdr's
  answer to the worktree request never came.
- **Any thread can be messaged or triaged**, not only the caller's own, so one agent can pick up
  another's work when you ask. Threads the caller launched are marked `mine` and listed first, and
  the tools tell agents to prefer them. A message to an agent mid-turn is refused rather than
  queued behind the turn. Herdr cannot tie a turn to a message, so a turn someone else starts in
  the same instant would end first and satisfy the wait; the answer says so, and the reply shows
  which turn it was. A message holding control characters (other than line breaks and tabs) is
  refused, since the terminal would act on them as keys.
- **Triage follows the rules above**, with one difference: the caller names the state it wants, so
  settling a settled thread changes nothing where the picker's key would unsettle it. Settling or
  snoozing can close an agent's pane; settling a working or blocked agent is refused and snoozing
  one leaves its pane open, so an agent cannot close the pane it is running in. Reopening starts a
  tab in the background. `undo` takes back the last change whoever made it, yours included.
- **A thread launched into a worktree stays open when settled**, because its pane is the only one
  in its workspace. It leaves the list; closing the workspace and removing the worktree are yours
  to do.
- **Reading** returns the terminal's recent rows, not a transcript; an agent showing a full-screen
  dialog is read as it shows now.
- **Waiting** holds for 30 seconds by default (50 at most) and then says the agent is still
  working. An agent stopped at a prompt Herdr does not recognize as one also reads as working, so
  the answer tells the caller to read the thread. The server answers one request at a time, so a
  wait delays a call made alongside it.
- **Only Herdr's own agents** are listed or changed here, not the other threads the picker shows.
- A retried launch with the same `request_id` returns the first launch instead of starting a
  second, for as long as the server process lives.

## Sidebar

The plugin owns Herdr's Agents view while it is enabled. It hides snoozed and settled agents and
counts the snoozed ones in the header (`2 snoozed`); settled ones pile up for 30 days, so they are not
counted there. The picker lists both. Herdr's sort toggle is
disabled until the plugin is disabled. Each agent pane gets two tokens:

| Token | Values |
| --- | --- |
| `triage_rank` | `0` pinned, `1` open, `2` snoozed, `3` settled |
| `triage` | pin icon, `unread`, `woke`, or nothing |

The sidebar only displays triage; act on agents from the picker. Marking a done agent read in the
picker cannot clear Herdr's own done icon in the sidebar; viewing the agent does.

## Configuration

`~/.config/herdr/plugins/config/ar.triage/config.toml`:

```toml
pin_icon = ""   # default: Nerd Font nf-oct-pin (U+F435); use any text without a Nerd Font
```

## How it works

There is no resident daemon. Hooks never touch T3; only the open picker reads it or sends it changes. Herdr runs `herdr-triage sync` on agent status changes, focus changes,
agent detection and pane exits (a hook that finds a sync already running asks it to sync again and
exits, since Herdr rejects plugin commands beyond 32 at once); each sync reconciles threads with `agent.list`, then, under a file lock,
reports tokens wherever Herdr's copy differs and sets the Agents view. A startup hook does the same after
a server restart. Each snooze starts a detached sleeper that syncs once its wake time has passed.

Threads live in a SQLite database in Herdr's plugin state directory, one per Herdr server. Triage saved
by earlier versions (`state.json`) is imported once.

The MCP server is started by its client, not by Herdr, and finds the same database at
`$XDG_STATE_HOME/herdr/plugins/ar.triage` (else `~/.local/state/herdr/plugins/ar.triage`); set
`HERDR_PLUGIN_STATE_DIR` for it if your Herdr keeps plugin state elsewhere.

Herdr doesn't run startup hooks on install or enable, so the sidebar adopts the triage order at the
first agent event or the first time you open the picker.

## Development

```sh
mise install    # pinned Rust and lefthook
make check      # fmt, clippy, tests
make bench      # release performance table + target/bench/latest.json (under 2 min after build)
```

Optional pre-push checks use the same gate:

```sh
lefthook install
RUN_PRE_PUSH_CHECKS=1 git push
```

Default pushes skip verification. Verification runs locally; this repository has no GitHub Actions workflows.
Integration tests run the binary against a fake Herdr socket.
