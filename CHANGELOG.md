# herdr-core changelog

## Unreleased

## [0.1.17] - 2026-11-02

### Fixed
- On macOS, quitting Herdr no longer stops your agents, even when Herdr isn't allowed to run in the background. The core now runs apart from the app. Agents that were already running before the update pick this up the next time they start. (herdr-app#294)
- The core no longer freezes every terminal while it saves to disk.
- On Linux machines without polkit, terminals start right away instead of after 25 seconds. (#62)
- Reading a full-screen agent that waits at a prompt answers at once instead of after 25 seconds, and never scrolls the prompt. (#60)
- A template started on another machine that takes longer than 30 seconds to get ready no longer fails, and its agent can still report back. (#63)
- `hc send` with arguments that aren't valid text says so instead of crashing. (#50)
- Emoji like ✍️ and ❤️ take two cells, as in Ghostty. (#58, thanks @occultsound)

### New
- Add machine takes a port: `hc machine add can@server:2222`. (#66, thanks @sundarshahi)
- Watching a terminal without controlling it (`observe`) can ask to be told when its size changes. (#53)
- The macOS core is signed with Herdr's Developer ID, so permissions you give it stay across updates.
- `hc --skill` shows agents how to run slow commands without blocking themselves.

## [0.1.16] - 2026-10-31

### Fixed
- Terminals no longer freeze when the core falls behind or loses touch with its holder: it reconnects and keeps your programs running. (#47)
- Images in yazi and other programs no longer spill as text over a slow connection.
- Programs pick up the new colors when you change the theme or switch between light and dark. (#46, thanks @geoqiao)
- Adding a Windows machine works in any Windows language, and when its SSH shell is PowerShell. (#49, #23)

### New
- Choose the shell new terminals start with, with `shell = [...]` in config.toml. (#24, thanks @JJLiebig)

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
