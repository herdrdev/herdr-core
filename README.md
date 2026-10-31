# herdr-core (`hc`)

`hc` runs programs and coding agents in terminals that stay alive after you leave.
Agents can start other agents, send them work, and hear back; you can attach to any terminal and take over.

Core draws nothing. Use it from your shell, an agent, or your own app.

## Install

Linux and macOS:

```sh
curl -fsSL https://releases.herdr.dev/core/install.sh | sh
```

Windows (PowerShell):

```powershell
powershell -c "irm https://releases.herdr.dev/core/install.ps1 | iex"
```

Builds are available for Linux and macOS on x86_64 and aarch64, and Windows on x86_64.
Linux builds are static. On Windows, templates and quoted shell commands need `sh` on PATH, such as Git for Windows provides.

Run `hc update` to update. Running terminals keep running.
See [security](docs/security.md) for the trust and signing details.

## Run it and come back

```sh
hc run cargo test                  # stream output and wait for it
hc run --bg --name web bun dev     # start it and return
hc ls                             # see live terminals
hc attach web                     # take over; Ctrl+\ detaches
hc read web --lines 100            # read without attaching
```

Closing your viewer does not end the program.
A waiting run checks in after 180 seconds by default, leaving the program running.
It prints the terminal's address so you can read it or wait again.

For a terminal you want to keep even if its program is lost, use `hc run --keep`.
Recovery starts a fresh shell, never reruns the task.
A normal exit or an explicit kill still ends it.

## Default shell

With no program, `hc run --bg` starts the machine's shell: `$SHELL` (or
`/bin/sh`) on Unix, PowerShell 7 on Windows if available, else Windows PowerShell.
Override it in `~/.config/herdr-core/config.toml` (Windows:
`%APPDATA%\herdr-core\config.toml`, or `$XDG_CONFIG_HOME/herdr-core/config.toml`
when set):

```toml
shell = ["nu"]
# Windows Git Bash (literal strings keep backslashes):
# shell = ['C:\Program Files\Git\bin\bash.exe', '--login', '-i']
```

The list is an executable followed by separate arguments, without extra path
quoting. It must contain a nonempty executable. Changes apply to the next new
shell or `keep` recovery, including recovery after a restart; no restart is
needed. Running terminals, explicit programs, and quoted command lines keep
their behavior. A bad setting reports a config error; a shell that cannot start
reports `spawn_failed`. Recovery stays pending and retries, with errors in
`core.log` (config) or `holder.log` (spawn); it never silently falls back to another shell.

## Agents run agents

Give an agent `hc --skill`. It prints the agent docs in one read, including how to delegate, wait, report back, and clean up.

[Templates](docs/templates.md) give an agent a command and a role.
After saving the reviewer example from that page:

```sh
hc templates
hc run --bg --name review reviewer "Review src/x.rs. Read-only."
hc send review "Focus on the error path."
hc read review --lines 100
```

Agents started inside an hc terminal become its children.
They can report back with `hc send` and wait for their children with `hc wait --children all`.
The parent gets notes when a child needs attention or ends.
Children end with their parent unless started with `--outlive-parent`.

## Every machine

```sh
hc machine add my-ssh-host --name box
hc run --on box --cwd /srv/app cargo test
hc attach box/t3
```

Use the address returned by your run, not the example's `box/t3`.
Programs and files stay on that machine. Your local core brings its terminals into the same list.
[Machines](docs/machines.md) covers SSH, custom connections, remote files, and reconnecting.

## Status comes from the program

An agent can say it is working, done, blocked, or in error through OSC 7501, the Program Status Protocol.
Core stores what it reports. It does not guess from screen text or recognize agent brands.
A program that does not report status has none.

Use `hc get review` for a snapshot or `hc events` to follow changes.

## Build your own client

`hc schema` prints the request/response API as OpenRPC with JSON Schema.
[The API docs](schema/README.md) cover connecting, terminal streams, events, and status.
Use `hc connect` to carry the protocol over stdin and stdout, including through SSH.

## Docs

- [Commands](docs/commands.md): generated from the binary's help.
- [Machines](docs/machines.md): work across machines.
- [Templates](docs/templates.md): reusable agent roles.
- [Security](docs/security.md): access, state, and updates.
- [API](schema/README.md): build a client.

For a quick lookup, run `hc --help` or a command's help, such as `hc run --help`.
For agents, use `hc --skill`.

## Build from source

You need Rust, Zig 0.16, and just. CI pins the Rust version in [.github/workflows/ci.yml](.github/workflows/ci.yml).

```sh
git submodule update --init --depth 1
cargo build --locked -p herdr-core
just check
```

`just install` installs this checkout and restarts your core. Running terminals keep running.
`just docs` refreshes the command reference. `just docs-check` checks it and the examples; CI runs the same tests.

See [CONTRIBUTING.md](CONTRIBUTING.md) for contributor setup and checks.

License: [Apache-2.0](LICENSE). `hc --licenses` prints the included third-party licenses.
After a dependency change, regenerate them with `just notices`.
