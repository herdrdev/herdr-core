# `@herdrdev/core`

Terminal runtime for coding agents (`herdr-core` / `hc`).

On first invocation, `@herdrdev/core` downloads the official `herdr-core` release archive for your OS and CPU (`macos-aarch64`, `macos-x86_64`, `linux-aarch64`, or `linux-x86_64`), verifies its SHA-256 digest against `manifest.json`, caches it under `~/.cache/herdr-core/npm/<version>/<target>/herdr-core`, and runs it with all arguments forwarded.

## Usage

```sh
npx @herdrdev/core --help
npx -p @herdrdev/core hc --skill
```

## Environment Variables

- `HERDR_RELEASES_URL`: Override the release server base URL (defaults to `https://releases.herdr.dev`).
- `HC_NPM_CACHE_DIR`: Override the local binary cache root (defaults to `~/.cache/herdr-core/npm`).
