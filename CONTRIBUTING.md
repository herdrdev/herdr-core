# Contributing

Report bugs and request features in [issues](https://github.com/herdrdev/herdr-core/issues). Discuss bigger changes there before writing a PR. Keep PRs small and add tests for behavior changes.

Core is an agent-neutral terminal runtime, not a multiplexer. We don't add per-agent integrations or hooks. Agent status comes only from OSC 7501; see [core's status docs](schema/README.md#status). Agent-specific status support belongs in the program or its integration, not core.

## Setup

Install Rust (CI pins 1.95.0), Zig 0.16.0, [just](https://just.systems/), and a C linker/toolchain for your OS. On Windows, use the MSVC toolchain and Git for Windows' `sh`.

```sh
rustup component add rustfmt clippy
cargo install cargo-deny --locked
git submodule update --init --depth 1
```

On Linux and macOS, also install the target used to type-check Windows code:

```sh
rustup target add x86_64-pc-windows-msvc
```

## Checks

```sh
cargo build --locked -p herdr-core
just check
just docs-check
```

`just check` runs formatting, cargo-deny, clippy, and the full test suite. `cargo test --locked` runs just the tests. On Windows, use `just test` to fetch the pinned ConPTY before testing. Tests use isolated sessions; don't point them at a running core.

Keep OS-specific code behind `platform/` and its shared interface, without `cfg` branches in shared code. See [AGENTS.md](AGENTS.md) for the project rules.

After changing dependencies, run `just notices` (Unix; needs `jq`) and review the generated licenses. Keep the [non-Cargo notices](distribution/notices/README.md) current too. `hc --licenses` and every release archive carry these texts.

PRs target `master`. Only the maintainer publishes releases.
