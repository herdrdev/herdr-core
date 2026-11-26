#!/usr/bin/env node
// Downloads and SHA-256-verifies the matching herdr-core release archive on
// first run against manifest.json, caches it per version under
// ~/.cache/herdr-core/npm/<version>/<target>/herdr-core, and forwards CLI args.
import crypto from "node:crypto";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { execFileSync, spawn } from "node:child_process";
import { fileURLToPath } from "node:url";

const pkgPath = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..", "package.json");
const pkg = JSON.parse(fs.readFileSync(pkgPath, "utf8"));

// resolveTarget maps Node's process.platform and process.arch to herdr-core's
// release target naming convention (<os>-<arch>).
export function resolveTarget(platform = process.platform, arch = process.arch) {
  const osMap = { darwin: "macos", linux: "linux" };
  const archMap = { arm64: "aarch64", x64: "x86_64" };
  const targetOS = osMap[platform];
  const targetArch = archMap[arch];
  if (!targetOS || !targetArch) {
    throw new Error(`Unsupported platform ${platform}/${arch}; herdr-core provides macos and linux builds for arm64 and x64.`);
  }
  return `${targetOS}-${targetArch}`;
}

// parseManifestTarget validates manifestJSON for expectedVersion and extracts
// the archive URL and 64-character hex SHA-256 digest for target.
export function parseManifestTarget(manifestJSON, expectedVersion, target) {
  const doc = typeof manifestJSON === "string" ? JSON.parse(manifestJSON) : manifestJSON;
  if (!doc || typeof doc !== "object") {
    throw new Error("Invalid herdr-core manifest.json");
  }
  if (doc.version !== expectedVersion) {
    throw new Error(`Manifest version mismatch: expected ${expectedVersion}, got ${doc.version}`);
  }
  const entry = doc.targets?.[target];
  if (!entry || typeof entry.url !== "string" || entry.url.length === 0) {
    throw new Error(`No ${target} build listed in manifest.json`);
  }
  if (typeof entry.sha256 !== "string" || !/^[0-9a-fA-F]{64}$/.test(entry.sha256)) {
    throw new Error(`Invalid SHA-256 digest for ${target} in manifest.json`);
  }
  return { url: entry.url, sha256: entry.sha256.toLowerCase() };
}

// fetchBuffer downloads url using Node's built-in fetch and returns a Buffer.
async function fetchBuffer(url) {
  const res = await fetch(url, { redirect: "follow" });
  if (!res.ok) {
    throw new Error(`GET ${url} failed with HTTP ${res.status}`);
  }
  return Buffer.from(await res.arrayBuffer());
}

// ensureBinary returns the path to the verified herdr-core binary for version,
// downloading and SHA-256-verifying it atomically when not yet cached.
export async function ensureBinary({
  version = pkg.version,
  cacheRoot = process.env.HC_NPM_CACHE_DIR || path.join(os.homedir(), ".cache", "herdr-core", "npm"),
  releasesBaseURL = (process.env.HERDR_RELEASES_URL || "https://releases.herdr.dev").replace(/\/+$/, ""),
  platform = process.platform,
  arch = process.arch,
} = {}) {
  const target = resolveTarget(platform, arch);
  const versionDir = path.join(cacheRoot, version, target);
  const binPath = path.join(versionDir, "herdr-core");
  if (fs.existsSync(binPath)) {
    return binPath;
  }

  const manifestURL = `${releasesBaseURL}/core/${version}/manifest.json`;
  const manifestBuf = await fetchBuffer(manifestURL);
  const { url, sha256: expected } = parseManifestTarget(manifestBuf.toString("utf8"), version, target);

  const archiveBuf = await fetchBuffer(url);
  const actual = crypto.createHash("sha256").update(archiveBuf).digest("hex");
  if (actual !== expected) {
    throw new Error(`SHA-256 mismatch for ${url}: expected ${expected}, got ${actual}`);
  }

  fs.mkdirSync(versionDir, { recursive: true });
  const stageDir = fs.mkdtempSync(path.join(versionDir, `.stage-${process.pid}-`));
  try {
    const archivePath = path.join(stageDir, "herdr-core.tar.gz");
    fs.writeFileSync(archivePath, archiveBuf);
    execFileSync("tar", ["-xzf", archivePath, "-C", stageDir], { stdio: "pipe" });
    const stagedBin = path.join(stageDir, "herdr-core");
    if (!fs.existsSync(stagedBin)) {
      throw new Error(`Archive ${url} did not contain herdr-core`);
    }
    fs.chmodSync(stagedBin, 0o755);
    for (const notice of ["LICENSE", "THIRD-PARTY-NOTICES"]) {
      const stagedNotice = path.join(stageDir, notice);
      if (fs.existsSync(stagedNotice)) {
        fs.copyFileSync(stagedNotice, path.join(versionDir, notice));
      }
    }
    fs.renameSync(stagedBin, binPath);
  } finally {
    fs.rmSync(stageDir, { recursive: true, force: true });
  }
  return binPath;
}

// runCLI ensures the binary is cached and runs it, forwarding signals and exit status.
export async function runCLI(commandName = "herdr-core") {
  const binPath = await ensureBinary();
  const child = spawn(binPath, process.argv.slice(2), { stdio: "inherit" });
  for (const sig of ["SIGINT", "SIGTERM", "SIGHUP"]) {
    process.on(sig, () => {
      try {
        child.kill(sig);
      } catch {}
    });
  }
  child.on("exit", (code, signal) => {
    if (signal) {
      process.removeAllListeners(signal);
      process.kill(process.pid, signal);
      return;
    }
    process.exit(code ?? 0);
  });
  child.on("error", (err) => {
    process.stderr.write(`${commandName}: ${err.message}\n`);
    process.exit(1);
  });
}

if (process.argv[1] && fs.realpathSync(process.argv[1]) === fs.realpathSync(fileURLToPath(import.meta.url))) {
  runCLI("herdr-core").catch((err) => {
    process.stderr.write(`herdr-core: ${err.message}\n`);
    process.exit(1);
  });
}
