set windows-shell := ["powershell.exe", "-NoLogo", "-NoProfile", "-Command"]

# Format, clippy here and for Windows, the whole suite: what CI runs on Linux.
check: lint test

# Refresh or check the help reference and command examples (no core is started).
[unix]
docs:
    sh scripts/docs.sh --write

[windows]
docs:
    $env:HC_UPDATE_DOCS='1'; cargo test --locked -p herdr-core --test cli_docs

[unix]
docs-check:
    sh scripts/docs.sh --check

[windows]
docs-check:
    Remove-Item Env:HC_UPDATE_DOCS -ErrorAction SilentlyContinue; cargo test --locked -p herdr-core --test cli_docs

[unix]
test:
    cargo test --locked

# Windows' own ConPTY swallows answers to programs: hc's tests need the one it ships.
[windows]
test:
    powershell.exe -NoProfile -ExecutionPolicy Bypass -File scripts/fetch-conpty.ps1 -Dest target/debug
    cargo test --locked

# Format and clippy only (seconds once built). Windows is type-checked with zig stubbed.
[unix]
lint:
    cargo fmt --all -- --check
    cargo deny check
    cargo clippy --locked --all-targets -- -D warnings
    ZIG="$PWD/scripts/zig-stub.sh" CARGO_TARGET_DIR=target/lint-windows cargo clippy --locked --target x86_64-pc-windows-msvc --all-targets -- -D warnings

[windows]
lint:
    cargo fmt --all -- --check
    cargo deny check
    cargo clippy --locked --all-targets -- -D warnings

fmt:
    cargo fmt --all

# After a dependency change: the licenses hc carries (`hc --licenses`).
[unix]
notices:
    scripts/notices

# Build and install this checkout; running terminals are kept.
[unix]
install:
    sh scripts/install-local.sh

[windows]
install:
    powershell.exe -NoProfile -ExecutionPolicy Bypass -File scripts/install-local.ps1

# Before a release: every check, then what changed since the last release, to review by hand.
[unix]
pre-release: check docs-check
    #!/usr/bin/env bash
    set -euo pipefail
    last=$(git describe --tags --abbrev=0 --match 'v*')
    echo; echo "Since $last:"; git log --oneline "$last"..HEAD
    echo
    if git diff --quiet "$last"..HEAD -- crates/herdr-core/src/SKILL.md; then
      echo "review: the agent manual (hc --skill, crates/herdr-core/src/SKILL.md) is unchanged since $last. Does it still match the changes above?"
    else
      echo "review: the agent manual (hc --skill) changed since $last. Read it once more."
    fi
    echo "review: README.md and docs/ describe the changes above."
    echo "review: skills/herdr-core/SKILL.md only says when to use hc; change it only if that changed."

# Bump the version, refresh the schema snapshots, commit. Tags nothing.
[unix]
release-prepare version:
    @printf '%s\n' "{{version}}" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+$' || { echo "error: version must look like 0.1.5, no v"; exit 1; }
    @awk 'index($0, "## [{{version}}] - ") == 1 { found = 1 } END { exit !found }' CHANGELOG.md || { echo "error: write release notes in CHANGELOG.md under ## [{{version}}] - YYYY-MM-DD first"; exit 1; }
    @[ -z "$(git status --porcelain)" ] || { echo "error: working tree must be clean"; exit 1; }
    @[ "$(git branch --show-current)" = master ] || { echo "error: release from master"; exit 1; }
    git fetch origin master --tags
    @git merge-base --is-ancestor origin/master HEAD || { echo "error: behind origin/master"; exit 1; }
    @! git rev-parse -q --verify "refs/tags/v{{version}}" >/dev/null || { echo "error: v{{version}} already exists"; exit 1; }
    sed -i.bak 's/^version = ".*"/version = "{{version}}"/' crates/herdr-core/Cargo.toml && rm -f crates/herdr-core/Cargo.toml.bak
    HC_UPDATE_SCHEMA=1 cargo test -p herdr-core --test m0
    cp schema/hc.openrpc.json schema/released.openrpc.json
    just check
    git add Cargo.lock crates/herdr-core/Cargo.toml schema
    git commit -m "chore: release {{version}}"
    @echo "v{{version}} prepared. Review it, then: just release-publish {{version}}"

# Push master, wait for its CI to pass, then push the tag (starts the release). Red CI publishes nothing.
[unix]
release-publish version:
    @[ -z "$(git status --porcelain)" ] || { echo "error: working tree must be clean"; exit 1; }
    @grep -q '^version = "{{version}}"' crates/herdr-core/Cargo.toml || { echo "error: this checkout is not {{version}}; run just release-prepare {{version}}"; exit 1; }
    @! git rev-parse -q --verify "refs/tags/v{{version}}" >/dev/null || { echo "error: v{{version}} already exists"; exit 1; }
    git push origin HEAD:master
    @sha="$(git rev-parse HEAD)"; run=""; \
    for _ in $(seq 60); do \
        run="$(gh run list --workflow ci --commit "$sha" --json databaseId -q '.[0].databaseId')"; \
        [ -n "$run" ] && break; sleep 5; \
    done; \
    [ -n "$run" ] || { echo "error: no CI run for $sha"; exit 1; }; \
    gh run watch "$run" --exit-status --interval 30 || { echo "error: CI failed; nothing published"; exit 1; }
    git tag v{{version}}
    git push origin v{{version}}
    @echo "v{{version}} tagged: the release workflow builds and publishes it"
