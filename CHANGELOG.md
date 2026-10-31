# herdr-core changelog

## Unreleased

## [0.1.15] - 2026-10-30

### New
- Release archives include third-party license notices, also available with `hc --licenses`.

### Fixed
- Kept terminals and their spaces come back after a reboot or shutdown.
- On macOS, new terminals start as login shells, so Homebrew and the rest of your `.zprofile` load.
- The core starts on RHEL, AlmaLinux and Rocky Linux 8. (herdrdev/herdr-app#126)
- A core installed by a package manager, or in a folder you can't write, is no longer offered updates it can't apply, and `hc update` says how to update it. (#22)
- Adding a Windows machine works when Git's `sh` is on its path, and offers to install a missing core. (#23)
- Machines with long host names can be added. (#27)
- Renaming a machine keeps its agents in the window.

## [0.1.14] - 2026-10-29

### Fixed
- The core starts on older systemd versions, including Ubuntu 22.04 and Debian 11. (#18)

## [0.1.13] - 2026-10-29

### New
- Child terminals end when their parent ends by default. Use `--outlive-parent` to keep them running.
- An installable skill teaches agents when to use `hc` and where to find its manual.
- A connection helper lets you use terminals in Vercel sandboxes. (#10)

## [0.1.12] - 2026-10-26

### Fixed
- Joining a running terminal shows the screen the program is actually using, instead of a stale alternate screen.
- On Windows, SSH connections reuse the desktop core when possible, or explain how to connect to it.
- New Windows terminals pick up changes to your environment, such as an updated `PATH`.
- Checking status no longer starts a core that isn't running.

## [0.1.11] - 2026-10-26

### New
- `hc --skill` prints the agent manual, replacing `--agent-help`.
- Windows releases are signed.

### Fixed
- `hc update` restarts an older running core even when the installed binary is already up to date.
- Programs no longer get duplicate resize reports when a viewer is connected.
- On Windows, connecting over SSH no longer opens extra console windows. (#7, thanks @JJLiebig)
- Windows SSH connections no longer try to use unsupported control sockets. (#4, thanks @JJLiebig)
- Development commands run on Windows. (#1, thanks @JJLiebig)
- Joining a running Windows program preserves its keyboard input mode.
- On Windows, the terminal's folder follows the shell when you change directories.
- Machines added to each other no longer echo events back and forth.
- Busy terminals use less CPU and write their saved state less often.

## [0.1.10] - 2026-10-23

### New
- First public release: run terminals, keep them alive, and let agents start, read and talk to each other, locally or over SSH.

### Fixed
- Machines added to each other list their terminals right away.
- Dropped connections always try to reconnect.
- The agent picker groups finished agents with idle ones.
