//! Builds libghostty-vt from the pinned Ghostty submodule with Zig.

use std::path::PathBuf;
use std::process::Command;

fn main() {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let src = manifest
        .join("../../vendor/ghostty")
        .canonicalize()
        .expect("vendor/ghostty missing: run `git submodule update --init --depth 1`");
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    for path in ["src", "build.zig", "build.zig.zon", "include"] {
        println!("cargo:rerun-if-changed={}", src.join(path).display());
    }
    println!("cargo:rerun-if-env-changed=ZIG");

    let (target, host) = (std::env::var("TARGET").unwrap(), std::env::var("HOST").unwrap());
    let windows = target.contains("windows");
    let mut zig = PathBuf::from(std::env::var("ZIG").unwrap_or_else(|_| "zig".into()));
    // Zig on Windows fails starting its helpers when PATH holds certain links
    // (the folder junctions some installers make): it only
    // needs Windows itself and its own folder.
    let mut clean_path = None;
    if cfg!(windows) {
        zig = which(&zig).unwrap_or_else(|| panic!("{zig:?} not found on PATH (Zig 0.16 required; or set ZIG)"));
        let root = PathBuf::from(std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into()));
        clean_path =
            Some(std::env::join_paths([zig.parent().unwrap().to_path_buf(), root.join("System32"), root]).unwrap());
    }
    let mut build = Command::new(&zig);
    // Baseline CPU, never this machine's: hc is copied to other machines
    // (releases, `hc machine add`), whose CPUs may lack this one's features.
    build.current_dir(&src).args(["build", "-Demit-lib-vt", "-Doptimize=ReleaseFast", "-Dcpu=baseline"]);
    if let Some(path) = clean_path {
        build.env("PATH", path);
    }
    // Only the static library is linked. Ghostty builds the xcframework whenever
    // it finds xcodebuild, and with only the Command Line Tools installed
    // /usr/bin/xcodebuild exists but fails.
    if target.contains("apple") {
        build.arg("-Demit-xcframework=false");
    }
    // Building for another target than this machine (the static musl Linux
    // release), or for Windows (its libc must be named): Zig must build for it too.
    if target != host || windows {
        build.arg(format!("-Dtarget={}", zig_target(&target)));
    }
    let status = build
        .arg("--prefix")
        .arg(out.join("zig-out"))
        .arg("--cache-dir")
        .arg(out.join("zig-cache"))
        .status()
        .unwrap_or_else(|e| panic!("cannot run {zig:?} (Zig 0.16 required): {e}"));
    assert!(status.success(), "zig build of libghostty-vt failed: {status}");

    // Keep only the static archive in the search path so the linker can't pick
    // the shared one. On Windows (MSVC) the static one has its own name.
    let lib = out.join("lib");
    std::fs::create_dir_all(&lib).unwrap();
    let name = if windows { "ghostty-vt-static.lib" } else { "libghostty-vt.a" };
    std::fs::copy(out.join("zig-out/lib").join(name), lib.join(name)).unwrap();
    println!("cargo:rustc-link-search=native={}", lib.display());
    println!("cargo:rustc-link-lib=static={}", if windows { "ghostty-vt-static" } else { "ghostty-vt" });
}

/// Cargo's target triple as Zig names it: `x86_64-unknown-linux-musl` ->
/// `x86_64-linux-musl`, `aarch64-apple-darwin` -> `aarch64-macos`,
/// `x86_64-pc-windows-msvc` -> `x86_64-windows-msvc`.
fn zig_target(triple: &str) -> String {
    let arch = triple.split('-').next().unwrap_or("native");
    let os = if triple.contains("apple") {
        "macos"
    } else if triple.contains("windows") {
        "windows"
    } else {
        "linux"
    };
    let abi = ["musl", "gnu", "msvc"].into_iter().find(|abi| triple.ends_with(abi));
    abi.map_or(format!("{arch}-{os}"), |abi| format!("{arch}-{os}-{abi}"))
}

/// A program's full path, as the shell would find it.
fn which(program: &PathBuf) -> Option<PathBuf> {
    if program.components().count() > 1 {
        return Some(program.clone());
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .flat_map(|dir| [dir.join(program), dir.join(program).with_extension("exe")])
        .find(|p| p.is_file())
}
