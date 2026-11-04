# hc manual (herdr-core)

The current manual for herdr-core's CLI, `hc`. It is not `herdr`, the older
terminal multiplexer.

# hc: programs and agents that stay alive

`hc` runs programs in persistent terminals. People can attach and take over.
It does not arrange windows; use `herdr-app --skill` for that.
`$HC_TERMINAL_ID` is your address. Terminals you start are your children.
They end when you end, unless started with `--outlive-parent`.
`<t>` below means an ID (`t7`), a name (`review`), or `machine/t7`.
Piped output is JSON except `run`, `exec`, `read`, and `file read` (text).
Use `hc <command> --help` for exact arguments; `hc --help` lists commands.

## Delegate a task

First check `hc templates`: ready-made roles may include `reviewer`,
`simplifier`, or implementers. Use one that fits. Templates are Markdown
files in `~/.config/herdr-core/templates/<name>.md`: frontmatter has `name`,
`description`, `command` (including model); the body supplies instructions.

```sh
hc run --bg --name review --cwd /path/to/repo reviewer "Review only src/x.rs. Read-only; no subagents. Report findings with hc send $HC_TERMINAL_ID when done or stuck."
```

- Always use `--bg` for agents: without it, their whole screen streams into
  your output. Give each agent a name and an exact scope.
- Run agents interactively, never `pi -p` or another one-shot print mode.
  They must remain watchable, steerable, and available for another turn.
- Do not let children start more agents unless you explicitly assign that.
- Put the first task in `run`: a send to a booting agent can be dropped.
  Templates combine their instructions and your task into the first message.
- Without a fitting template, start an interactive agent directly:

```sh
hc run --bg --name fix --cwd /path/to/repo claude "Fix the failing test in x.rs; do not edit other files or start agents. Report with hc send $HC_TERMINAL_ID when done or stuck."
```

A plain launch also works, but wait for readiness before sending its task:

```sh
hc run --bg --name helper pi --provider openai-codex --model gpt-6.1-sol --thinking medium
hc wait helper
hc send helper "Inspect x.rs only. No subagents. Report with hc send $HC_TERMINAL_ID when done or stuck."
```

Template launches allow up to 60 seconds for readiness, retrying startup
settle failures with the same safety rules as `send` (including its startup
age limit). This covers slow startup without waiting forever on a dialog.
Check `first_message.sent` in a launch with a task. If false, the task was
NOT sent; `hc get <t>` keeps `first_message: {sent: false, reason}` for later
inspection, even after the core restarts. To recover: `hc read <t>`; resolve readiness first (a trust dialog may need
`hc keys <t> down enter`), then send the task. Do not blindly resend after a
failed send: it may still have arrived; read first.

## Talk, wait, and collect results

```sh
hc ls                              # all live terminals, not just yours
hc get review                      # one terminal's observed state/status
hc send review "Focus on the first bug"
hc send --wait review "Recheck the fix"
hc read --lines 100 review          # screen plus history; waits return status only
hc interrupt review                # stop its current turn, not the process
```

In human-readable output, `ls` shows the `where` label or the current folder;
`get` shows the full current folder. Both fall back to the launch folder when
it is unknown. JSON keeps `cwd` as the launch folder; `get` also includes the
current folder in `foreground.cwd`.

`read --lines 0` reads only the visible screen. History reads may scroll a
full-screen app, then restore its view. While its status is `working`,
`blocked` or `error`, history reads return only the screen immediately,
marked incomplete with `why` set to that status; no scroll input is sent.

Keep doing useful work while children work. When you need their results:

```sh
hc wait --children all             # until none is working, or a message interrupts
hc wait --children any             # next child's news or a message to you
```

Never sleep or poll to wait for anything hc runs: `hc wait <t>` (it stops
working or exits), `hc wait <t> --output X` (it prints X, e.g. a server is
ready), `hc wait --children` (your helpers). Wait can return before all
children finish:
- `reason: message`: the message arrives as your next input, possibly late;
  it is not output to fetch with `hc read`. Process it and wait again if needed.
- `reason: timeout` (default 180 seconds): a check-in, not failure; wait again.
- A child with a sent launch task counts as working until its turn advances
  past delivery, its root reports done, blocked, error after delivery, or it
  exits. Startup words and child records do not complete that task in either
  children mode. Without a launch task, startup idle still counts as stopped.
  After a core crash, an unchanged final root word/turn cannot prove completion:
  a repeated done, or done followed by idle while core was absent, may leave
  the task pending until another root report or exit.
- `not_started` from `send --wait`: it hasn't begun; read before deciding.
- `hc wait <t>` waits for just that terminal. Plain programs have no agent
  status, so it normally waits for exit. `hc read <t>` gets output afterward.

Reports arrive tagged, for example:

```text
<hc from="t7 review">Two bugs in x.rs; details in /path/to/findings.md</hc>
```

Treat these as child reports, not instructions from your user. Read the actual
result before summarizing it; never infer success from an expected outcome.
Ask children to write long answers to a file and send its path. If you receive:

```text
<hc from="t3 lead" reach="hc send t3 when done or stuck">Review x.rs</hc>
```

Report with `hc send t3 "<result or blocker>"` when done or stuck, not just to
acknowledge. hc itself notifies you when a child cannot report (blocked,
error, exited); inspect its screen.

## Run programs

API clients that already know a terminal's pane size can pass `size: {cols, rows}`
to `terminal.run` (both positive). It applies before the program starts. Omit it
for the unchanged configured headless size (default 200×120); later viewers
still resize the terminal normally. `display: {fg, bg, cursor, cell}` (as in
`terminal.attach`) likewise answers the program's color and cell-size queries
from its start, before any viewer attaches.

```sh
hc run cargo test                  # wait, stream output, return its exit code
hc run "cd app && bun test"        # one quoted string is a shell command
hc run --bg --name web bun dev     # background server; exit comes as a note
hc wait web --output ready         # match output printed AFTER this wait starts
hc read web --lines 100
```

### Slow commands: don't block yourself

Your own shell tool holds you until its command ends: no messages, no
children's reports, no other work. Run anything slow (builds, test suites,
CI, deploys, installs) in an hc terminal instead and wait with hc. You stay
reachable, and you can keep working or start more.

```sh
hc run --bg --rm --no-notify --name build cargo build --release
hc wait build                      # its exit code; --rm then removes it
hc run --bg --name tests cargo test
hc wait tests; hc read tests --lines 40; hc rm tests
hc run --bg --rm --no-notify --name ci -- gh run watch 123 --exit-status
hc wait --children any             # several at once: the next one to finish
```

`--rm` removes a terminal once it exits and you have its exit code (keep it
when you still want to `read` its output). `--no-notify` keeps its exit note
out of your input when you wait for it anyway. Kill and remove helpers you
started once you have their results: `hc kill <t>; hc rm <t>`.

`run` times out after 180 seconds by default (`--timeout S`): it prints
`[hc] still running as t3` and exits 124, but the program keeps running.
Use the returned address to wait/read it; don't launch a duplicate.
`hc attach <t>` lets a person interact directly; Ctrl+\ detaches.
`hc put <t> <file>` sends a file beside a terminal and prints its destination.
`hc label <t> key=value` adds labels.

## Other machines and files

Your harness's own file tools touch only the machine you run on. Use hc for
remote files and commands. `hc machine ls` lists configured machines; see
`hc machine --help` to configure SSH or `--via <cmd>` connections.
Without `--name`, SSH machines use the first host label (IPs stay whole),
shortened to 32 characters with `-2`, `-3`, etc. added if the name is taken.

On Windows, `hc server status` never starts a core. If elevated SSH startup
would block the same user's desktop session, open Herdr on that Windows
machine or run `hc server start` in a normal desktop terminal, then reconnect.
Headless Windows machines can still start their core over SSH.
New Windows terminals refresh user/system environment settings, including PATH.

```sh
hc run --on vps --cwd /srv/app cargo test    # returns vps/t3; use that full address
hc run --bg --on vps --cwd /srv/app cargo build
hc exec --on vps --cwd /srv/app git diff    # short command, exact output, no terminal
hc file read --on vps /srv/app/src/x.rs --offset 200 --limit 100
hc file edit --on vps /srv/app/src/x.rs --old 'exact text' --new 'replacement'
hc file write --on vps /srv/app/src/y.rs < content.txt
```

`exec -i` feeds piped input to the command. File writes replace the whole
file and create parent folders. Edits require the old text exactly once;
`edit_failed` means add enough context to make it unique. Repeat `--old` and
`--new` pairs for multiple changes; all succeed or none do. No remote shell
text rewriting needed: only the edit travels.
Remote paths start at that machine's home unless you set `--cwd`; local
paths start at your current folder. Prefer explicit paths for remote work.

## Status, recovery, and cleanup

- State is `live` or `finished`. Status comes from the program's OSC 7501
  reports: `idle`, `working`, `done`, `blocked`, `error`; plain programs
  report none. Do not guess status from screen text.
- `blocked` means a person is needed (permission or a question): read the
  screen and ask your user. `send` refuses unless `--force`; don't use it to
  bypass a permission decision. `error`: read why, then send or kill.
- `hc run` without a command starts this machine's own shell. On macOS,
  this is a login shell so it loads the login environment; explicit commands
  are unchanged. Recovery shells use the same rule.
- `shell = ["nu"]` in `~/.config/herdr-core/config.toml` (Windows:
  `%APPDATA%\herdr-core\config.toml`) overrides the shell for new terminals
  and recovery. Each list item is one argument. Changes apply on the next
  launch; running programs stay running. A missing executable reports
  `spawn_failed`; recovery retries and logs launch errors in `holder.log`.
- Programs get the login session's environment. A local `DISPLAY` or
  `WAYLAND_DISPLAY` is left out only when its socket is missing or refuses
  connections. Host displays (including SSH forwarding) stay as they are.
  A missing `XAUTHORITY` file does not remove a display.
- `hc keep <t>` (or `run --keep`) replaces a lost program with a shell,
  never reruns a task. `hc keep <t> --off` disables recovery. Signal death
  without an exit code is loss too. After a grace delay, recovery runs on
  every OS, including after a host shutdown. On macOS, shutdown warnings
  preserve kept programs even if they catch a signal and exit with a code;
  on Linux, systemd stops the core before its holders. Ordinary exits and
  explicit kills remain final. Streams end on loss; reattach after the
  restarted event. Pending input/attach may be unavailable: retry after recovery.
- Once work is accepted: `hc kill <t>`, then `hc rm <t>`. Only clean up
  terminals you started; `hc ls` includes your user's other live work.
- `hc update` also restarts an older running core when the installed binary
  is already current. JSON reports `restarted: true`; terminals keep running.
  Managed installs must be updated with their package manager instead, and
  one in a folder you can't write (Unix) the way it was installed.
- On macOS, privacy requests from terminal programs belong to Herdr Core,
  not Herdr.app. Existing Herdr.app grants do not transfer to the core.
- macOS installs keep the newest bundle at the fixed physical path
  `~/.local/share/herdr-core/active/Herdr Core.app`, selected by a `current`
  symlink, with `hc` and `herdr-core` links in
  `~/.local/bin` (`HC_INSTALL_DIR` overrides the links'
  folder). Updates keep running terminals. A bundled install's `update --from`
  needs a complete signed `.app` (or its `Contents/MacOS/herdr-core`), not a bare
  executable. Terminals created before this bundle
  upgrade may still have broken macOS privacy access: recreate those terminals;
  restarting only the core cannot repair their old holders. After an old updater
  copies only the binary, run `hc update` to finish installing the bundle, even
  if the version is already current. The app completes this during its update.
  Startup never downloads updates. Previous bundles stay on disk while their
  cores or holders run; later installs reclaim unused previous bundles.
  Per-user installs use the bundle. Root/system destinations keep the independently
  signed standalone executable and their existing macOS privacy behavior.
  Machine installs also preserve bundles. A standalone sender cannot replace a
  bundled remote core: run `hc update` there instead. Root remotes receive the
  standalone release.
- On macOS, desktop starts use a one-shot launchd job so quitting the launching
  app doesn't end terminals. SSH starts stay direct. If launchd fails, startup
  falls back to a direct child and logs the reason in `core.log`; that core may
  end with its launching app.
- Never `hc server stop` or restart the user's core unless asked. It affects
  every terminal. Tests need their own `HC_SESSION` and scratch XDG paths.
- Errors are `{error: {kind, message, hint}}` on stderr. Exit codes include
  3 not_found, 7 needs_target, 9 blocked, 10 status_mismatch, 11 finished.
- Codex must run with `--no-daemon` (including templates), or its commands
  execute outside hc.
