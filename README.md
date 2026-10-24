# herdr-core (`hc`)

A terminal runtime for coding agents. `hc` runs programs in terminals that
stay alive, lets agents start, talk to, read, wait on and hear back from
other agents, and lets a person attach to any of them and take over. It
draws nothing: status comes from the programs themselves (OSC 7501).

Closed beta.

## Install

Linux and macOS:

```sh
curl -fsSL https://releases.herdr.dev/core/install.sh | sh
```

Windows (PowerShell):

```powershell
powershell -c "irm https://releases.herdr.dev/core/install.ps1 | iex"
```

Updating later: `hc update` (running terminals keep running).

Builds: Linux x86_64 and aarch64 (static, any distro), macOS aarch64 and
x86_64, Windows x86_64. Windows notes: the exe is unsigned (Smart App
Control may block it); templates and quoted shell lines need an `sh` on
PATH (Git for Windows has one).

## Use

```sh
hc run cargo test                  # runs it and waits, like a shell
hc run --bg --name web bun dev     # starts it and returns
hc ls                              # what's running
hc attach web                      # take it over (ctrl+\ detaches)
hc run reviewer "review src/x.rs"  # an agent from a template, with a task
```

People: `hc --help`. Agents: `hc --agent-help` (everything in one read).
Other machines: `hc machine add <ssh-target>`, then `hc run --on <name> …`.
App builders: `hc schema` (OpenRPC), and `schema/README.md` for streams and
events.

## Build

Rust (see `.github/workflows/ci.yml` for the version), Zig 0.16, and the
Ghostty submodule:

```sh
git submodule update --init --depth 1
just check     # format, clippy (here and for Windows), the whole suite
just install   # install this build; running terminals keep running
```

Windows `just install` uses `%LOCALAPPDATA%\Programs\herdr-core`, adds it to
PATH, and updates the running core without stopping its terminals.
`HC_INSTALL_DIR` chooses another install directory.

License: Apache-2.0. `hc --licenses` prints the licenses of everything hc
includes (regenerate with `just notices` after a dependency change).
