# Shipped notices

`just notices` combines these texts with every registry package in `Cargo.lock`, including target-specific and build dependencies. It reads full LICENSE/COPYING/NOTICE files from Cargo's downloaded sources and fails if none exist. It needs Cargo, jq, and Unix tools. The result is embedded in `hc --licenses` and shipped in release archives.

Non-Cargo inputs are checked in so regeneration doesn't depend on a license server:

- `ghostty.txt`: Ghostty's MIT license at submodule commit `3d07ef9e28c4af05065b9e2e546b8ac8f9b62df0`.
- `simdutf.txt`, `highway.txt`, `wuffs.txt`: the libraries built into libghostty-vt, at the versions pinned by Ghostty.
- `zig.txt`: Zig 0.17.1 compiler_rt.
- `uucode*.txt`: [uucode at ea62149739404a73c202b48a33bf6dd2af4bd9b0](https://github.com/jacobsandlund/uucode/tree/ea62149739404a73c202b48a33bf6dd2af4bd9b0), its `LICENSE.md` and both files in `licenses/`. Include the Hoehrmann decoder notice conservatively alongside the Unicode tables; both are part of the imported module.
- `musl.txt`: [musl v1.2.5 COPYRIGHT](https://github.com/ifduyue/musl/blob/v1.2.5/COPYRIGHT). Rust 1.95.0's `src/ci/docker/scripts/musl.sh` and `musl-toolchain.sh` select 1.2.5 with security patches for the release Linux targets. The patches do not replace its license.
- `rust.txt`: Rust 1.95.0's installed `share/doc/rust/COPYRIGHT-library.html`, converted to plain text, followed by `library/compiler-builtins/LICENSE.txt` from the same toolchain's rust-src. These cover the standard library and compiler runtime, which Cargo.lock does not list.

Windows archives also carry Microsoft's ConPTY LICENSE and NOTICE from `distribution/windows/`, checked against the pinned package by `scripts/fetch-conpty.ps1`.

Core ships no GUI bundle, icons, fonts, artwork, or sounds. Ghostty's GUI assets and other GUI dependencies (including fonts) are not part of libghostty-vt's build. QuickJS, taffy, Lucide, Feather, and LobeHub are not used by core.

When changing Ghostty, Rust, Zig, or release targets, review this list and refresh the affected texts before running `just notices`. Cargo dependency additions without notices fail the license test, as do changed or missing checked-in non-Cargo texts. This is a coverage check, not automatic discovery of newly linked native libraries.
