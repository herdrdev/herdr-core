# Agent templates

Save a command and a role once, then start an agent by name with a task.
Templates do not install agents or configure their status reporting.

[Back to the README](../README.md) · [Command reference](commands.md#hc-templates)

## Make a reviewer

Save this as `~/.config/herdr-core/templates/reviewer.md` on Linux or macOS:

```markdown
---
description: Read-only code review. Give it file paths and scope.
command: pi
---
You are a focused review agent. Read the code before giving findings.
Do not edit files or start other agents.
Report bugs with file paths, lines, and a short reason.
If a parent assigned the review, report back with hc send.
```

Use an installed interactive agent command. Put model choices and other agent options in that command.
On Windows, templates live under `%APPDATA%\herdr-core\templates` by default.
Setting `XDG_CONFIG_HOME` changes the config home on either platform.

```sh
hc templates
hc run --bg --name review --cwd /path/to/repo reviewer "Review src/x.rs only."
hc read review --lines 100
hc send review "Recheck the error path."
```

Use background mode for agents so their screen does not stream into your caller's output.
You can still attach and take over with `hc attach review`.

## File format

Each template is one Markdown file ending in `.md`.
The filename without that suffix is the template's name.
The optional name field is ignored; the filename wins.
Files are read fresh on each use.

Start the file with a line containing three hyphens. End the header the same way.
The header is a small line-based format, not full YAML:

| Field | Meaning |
| --- | --- |
| `command` | Required shell command that starts the agent. |
| `description` | Short text shown in the template list. |
| `ready` | Default: wait for an OSC 7501 status report. The value `quiet` waits for output to settle instead. |
| `submit` | Override the key sequence used to submit text. |
| `stop` | Override the key sequence used to interrupt a turn. |
| `paste_gap_ms` | Override the pause before submitting pasted text, in milliseconds. |

Values are literal text after the first colon, trimmed at each end.
Do not add YAML quotes around them or use multiline YAML values.
Unknown fields are ignored.
The body is the agent's opening instructions. Core appends your task after a blank line.
The command runs through this machine's shell line: `sh -c` on Linux/macOS, cmd on Windows,
just like `hc run "line"`. On Windows, use `pwsh -File` to run a PowerShell script:

```markdown
---
command: pwsh -NoProfile -File "C:\agents\code reviewer.ps1"
---
Review the assigned code.
```

For existing Windows templates using sh syntax, move the old command into a `.sh` file
and use `command: sh "C:\agents\reviewer.sh"` (sh must be on PATH).
An inline `sh -c "..."` command needs escaping for both cmd and sh: for example,
cmd expands `%OS%` even inside sh single quotes before sh receives it.

## Readiness and input

By default, core waits for the agent's first status report before sending the opening message.
If that does not arrive within 30 seconds by default, it leaves the screen alone.
A trust dialog might be waiting for you. Read the screen before sending again.
The run result reports whether the first message was sent.

For a program that does not report status, opt into quiet readiness:

```markdown
---
description: An interactive assistant without status reports.
command: my-assistant
ready: quiet
submit: enter
stop: ctrl+c
paste_gap_ms: 100
---
Stay within the assigned files. Ask when blocked.
```

Quiet output is not proof that an agent is ready. Use this only when you know its startup flow.
It does not give the program an agent status; core still never guesses status from the screen.
See `hc keys --help` for key notation and `hc --skill` for the delegation workflow.

When a terminal is launched on another machine, that machine resolves the template and runs its command.
Put the template and agent there first.
