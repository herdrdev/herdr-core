#!/usr/bin/env node
// Downloads and SHA-256-verifies the matching herdr-core release archive on
// first run against manifest.json, verifies Apple Developer ID code signing on
// official macOS releases, caches it per version under
// ~/.cache/herdr-core/npm/<version>/<target>/herdr-core, and forwards CLI args.
import crypto from "node:crypto";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { execFileSync, spawn } from "node:child_process";
import { fileURLToPath } from "node:url";

const OFFICIAL_RELEASES_URL = "https://releases.herdr.dev";
export const DARWIN_CODESIGN_REQUIREMENT =
  '=identifier "dev.herdr.core" and anchor apple generic and certificate leaf[subject.OU] = "2W9652Q7QR"';

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
  let parsedURL;
  try {
    parsedURL = new URL(entry.url);
  } catch {
    throw new Error(`Invalid archive URL for ${target} in manifest.json: ${entry.url}`);
  }
  if (parsedURL.protocol !== "https:" && parsedURL.protocol !== "http:") {
    throw new Error(`Unsupported archive URL protocol for ${target}: ${parsedURL.protocol}`);
  }
  if (typeof entry.sha256 !== "string" || !/^[0-9a-fA-F]{64}$/.test(entry.sha256)) {
    throw new Error(`Invalid SHA-256 digest for ${target} in manifest.json`);
  }
  return { url: entry.url, sha256: entry.sha256.toLowerCase() };
}

// verifyDarwinSignature checks that binPath carries Herdr's Developer ID
// signature (dev.herdr.core, team 2W9652Q7QR) before it enters the cache.
export function verifyDarwinSignature(binPath, { codesignBin = "/usr/bin/codesign" } = {}) {
  try {
    execFileSync(codesignBin, ["--verify", "--strict", "-R", DARWIN_CODESIGN_REQUIREMENT, binPath], {
      stdio: "pipe",
    });
  } catch {
    throw new Error("Not a signed Herdr release (expected dev.herdr.core, team 2W9652Q7QR)");
  }
}

// fetchBuffer downloads url using fetchImpl and returns a Buffer.
async function fetchBuffer(url, fetchImpl = fetch) {
  const res = await fetchImpl(url, { redirect: "follow" });
  if (!res.ok) {
    throw new Error(`GET ${url} failed with HTTP ${res.status}`);
  }
  return Buffer.from(await res.arrayBuffer());
}

// ensureBinary returns the path to the verified herdr-core binary for version,
// downloading, SHA-256-verifying, and (on official macOS releases) signature-
// checking it atomically when not yet cached.
export async function ensureBinary({
  version = pkg.version,
  cacheRoot = process.env.HC_NPM_CACHE_DIR || path.join(os.homedir(), ".cache", "herdr-core", "npm"),
  releasesBaseURL = (process.env.HERDR_RELEASES_URL || OFFICIAL_RELEASES_URL).replace(/\/+$/, ""),
  platform = process.platform,
  arch = process.arch,
  fetchImpl = fetch,
  codesignBin = "/usr/bin/codesign",
} = {}) {
  const target = resolveTarget(platform, arch);
  const versionDir = path.join(cacheRoot, version, target);
  const binPath = path.join(versionDir, "herdr-core");
  if (fs.existsSync(binPath)) {
    return binPath;
  }

  const manifestURL = `${releasesBaseURL}/core/${version}/manifest.json`;
  const manifestBuf = await fetchBuffer(manifestURL, fetchImpl);
  const { url, sha256: expected } = parseManifestTarget(manifestBuf.toString("utf8"), version, target);

  const archiveBuf = await fetchBuffer(url, fetchImpl);
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
    if (platform === "darwin" && releasesBaseURL === OFFICIAL_RELEASES_URL) {
      verifyDarwinSignature(stagedBin, { codesignBin });
    }
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

function invokedCommandName(argv1 = process.argv[1]) {
  const base = path.basename(argv1 || "herdr-core").replace(/\.m?js$/, "");
  return base === "hc" ? "hc" : "herdr-core";
}

// runCLI ensures the binary is cached and runs it, forwarding signals and exit status.
export async function runCLI(commandName = invokedCommandName()) {
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
  const name = invokedCommandName();
  runCLI(name).catch((err) => {
    process.stderr.write(`${name}: ${err.message}\n`);
    process.exit(1);
  });
}
