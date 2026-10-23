# herdr-core

`herdr-core` (alias `hc`) is a terminal runtime for coding agents: it runs
programs in terminals, keeps them alive, relays raw bytes to any viewer,
reads status from OSC 7501, and lets agents start, talk to, read, wait on and
get notified by other agents. It draws nothing.

Read the code. This file only holds what reading won't tell you: the rules,
the traps, and the handful of decisions that look arbitrary until you know
why.

Can (`ogulcancelik`) maintains it. His working notes (plans, the spec,
half-finished thinking) live in `.local/`, gitignored and often out of date:
if it exists, trust the code first and ask Can when they disagree.

## The rule

Core is a runtime, not a multiplexer. If a feature only makes sense with a
layout or a workspace to return to, it does not belong here.

## Rules that aren't in the code

- Fresh code. Do not copy from Herdr 1
  ([herdrdev/herdr](https://github.com/herdrdev/herdr)). Read it for ideas,
  lessons and edge cases.
- libghostty-vt comes from fresh upstream Ghostty master, not Herdr's
  vendored copy.
- Status comes only from OSC 7501. No screen detection, no agent registry,
  no vendor-specific logic. This is the one most likely to be helpfully
  added; don't.
- Every action reports what was observed, never what was assumed.
- One API; the CLI is one call per command. The schema is generated from one
  method table, so params and results in `api/types.rs` are also CLI args and
  the published schema. Change them and regenerate:
  `HC_UPDATE_SCHEMA=1 cargo test -p herdr-core --test m0`.
- Lean, KISS, always. Fewer lines, fewer files, fewer dependencies. No
  abstraction before a second real caller, no config for things nobody asked
  to configure.
- Deciding code stays separate from IO, so it tests without processes.
- TDD. A test first for every MUST in the spec. One good test per behavior,
  not five checking the same thing. A few end-to-end tests with real PTYs
  beat many mocks. The suite runs in seconds; keep it that way.
- Do not change behavior the spec defines silently. Spec changes need Can's
  agreement.
- Aim for source files under ~32 KB, hard cap 48 KB because read tools stop
  near 50. Split along a real seam when you next touch one that's over.

## Traps

- Tests and smokes run in isolated sessions (`HC_SESSION`) with scratch XDG
  paths under `/var/tmp`. Never touch the machine's real core, Herdr, or its
  running agents. Clean up scratch data.
- Real-agent smokes (Claude Code with haiku, Pi with a cheap model) cost
  money. Ask before running them.
- A dependency change (crates or Ghostty) needs `just notices`; a test fails
  if a crate is missing from the notices.
- `rustfmt.toml` width 120 is the format and CI checks it: `just fmt`.
  `just lint` is fmt, clippy, and clippy for Windows from Linux. `just check`
  is lint plus the whole suite. Run it before proposing a commit.
- `just build` and `just test` run Cargo with the lockfile. Windows recipes
  use PowerShell by default; `just --shell pwsh --shell-arg -Command <recipe>`
  uses pwsh. `just install` uses `cargo install` on Windows; notices and
  release recipes require Unix.

## Contributing

Work on a branch and open a pull request to `master`. CI must pass and Can
approves; merges are squashed. Commit messages: lowercase conventional
commits, single line, no AI co-author lines. Only Can releases (a `v*` tag).

## Only on Can's machines

Skip this section unless the checkout is Can's (`~/Projects/herdr-core` on
his Linux desktop or Mac).

- Propose the commit message first. No commit or push without his go.
- `just install` replaces `~/.local/bin/hc` and restarts his core. Only with
  his go.
- Releases only through `just release-prepare X.Y.Z`, then `just
  release-publish X.Y.Z` once he says go. Publish pushes master, waits for CI
  on that commit, tags only if it passed. Never push a release tag by hand.
- `.local/background.md` says where Herdr 1's lessons live on his machine.
