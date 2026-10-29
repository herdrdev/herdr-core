# OpenBSD native port

Tested on OpenBSD 8.0 amd64, Rust 1.99.1 and Zig 0.16.0, at core version
0.1.13. Build alongside the existing Herdr 0.9.x TUI; this core reports `hc`
and is a separate program. Do not replace the TUI binary with it.

```sh
git submodule update --init --depth 1
PATH=/usr/local/bin:/usr/bin:/bin cargo build --locked
```

The new platform module uses sysctl for process enumeration, arguments,
environment and current working directories. Environment pointers are checked
against the returned buffer before accessing string bytes. No procfs or
privileged process access is required. Processes whose facts are inaccessible
are skipped, like on the other supported systems.

OpenBSD's wall-clock process timestamps cannot safely provide the monotonic
age optimization used on Linux/macOS. Tokens therefore use a zero age stamp,
and each sweep inspects all processes instead of applying that optimization.
Scope matching and descendant exclusion remain in the shared implementation.

Validation executed locally:

- Native build and CLI version: `hc 0.1.13`.
- 82 library tests passed; one existing test remained ignored. Includes the
  OpenBSD process-facts test and scope/descendant cleanup tests.
- Foreground (1), keep/recovery (14), and run/output (3) integration tests passed.
- Eight of nine exec integration tests passed. The stalled-reader/backpressure
  test still fails with a read timeout on OpenBSD and is explicitly unresolved.
- The accompanying herdr-app rendered on an isolated X11 display, started a
  terminal, read its output and sent a command through this core successfully.

The test harness's leak check uses sysctl on OpenBSD. Its helpers avoid sh's
built-in `r` alias, find packaged tools in `/usr/local/bin` on fake SSH hosts,
and generate binary output with Python instead of GNU `head -c`.

Tests use their existing private sessions and XDG directories. No installed
core or pre-existing Herdr session was stopped. Other platforms and the full
upstream CI/lint matrix have not been validated locally.
