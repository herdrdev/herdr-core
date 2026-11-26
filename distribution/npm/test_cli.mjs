import assert from "node:assert/strict";
import crypto from "node:crypto";
import fs from "node:fs";
import http from "node:http";
import os from "node:os";
import path from "node:path";
import { execFileSync, spawnSync } from "node:child_process";
import test from "node:test";
import { fileURLToPath } from "node:url";

import { ensureBinary, parseManifestTarget, resolveTarget } from "./bin/herdr-core.js";

test("resolveTarget maps supported OS/arch pairs and rejects unsupported ones", () => {
  assert.equal(resolveTarget("darwin", "arm64"), "macos-aarch64");
  assert.equal(resolveTarget("darwin", "x64"), "macos-x86_64");
  assert.equal(resolveTarget("linux", "arm64"), "linux-aarch64");
  assert.equal(resolveTarget("linux", "x64"), "linux-x86_64");
  assert.throws(() => resolveTarget("win32", "x64"), /Unsupported platform/);
});

test("ensureBinary verifies manifest SHA-256, extracts herdr-core + notices, caches, and rejects tampered archives", async () => {
  const tmp = fs.mkdtempSync(path.join(os.tmpdir(), "hc-npm-test-"));
  const target = resolveTarget();
  const version = "0.99.0";
  const archiveName = `herdr-core-${target}.tar.gz`;

  const buildDir = path.join(tmp, "build");
  fs.mkdirSync(buildDir, { recursive: true });
  fs.writeFileSync(path.join(buildDir, "herdr-core"), '#!/bin/sh\nprintf "hc-mock %s\\n" "$*"\n', { mode: 0o755 });
  fs.writeFileSync(path.join(buildDir, "LICENSE"), "Apache-2.0\n");
  fs.writeFileSync(path.join(buildDir, "THIRD-PARTY-NOTICES"), "Notices\n");
  const tarPath = path.join(tmp, archiveName);
  execFileSync("tar", ["-czf", tarPath, "-C", buildDir, "herdr-core", "LICENSE", "THIRD-PARTY-NOTICES"]);
  const tarBytes = fs.readFileSync(tarPath);
  const sha = crypto.createHash("sha256").update(tarBytes).digest("hex");

  let downloads = 0;
  let corrupt = false;
  let port = 0;
  const server = http.createServer((req, res) => {
    if (req.url === `/core/${version}/manifest.json`) {
      res.writeHead(200, { "Content-Type": "application/json" });
      res.end(
        JSON.stringify({
          version,
          generation: 1,
          notes: `herdr-core ${version}`,
          targets: {
            [target]: {
              url: `http://127.0.0.1:${port}/core/${version}/${archiveName}`,
              sha256: sha,
            },
          },
        })
      );
      return;
    }
    if (req.url === `/core/${version}/${archiveName}`) {
      downloads += 1;
      res.writeHead(200, { "Content-Type": "application/gzip" });
      res.end(corrupt ? Buffer.from("tampered") : tarBytes);
      return;
    }
    res.writeHead(404).end();
  });

  await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
  port = server.address().port;
  const releasesBaseURL = `http://127.0.0.1:${port}`;

  try {
    const cacheRoot = path.join(tmp, "cache");
    const binPath = await ensureBinary({ version, cacheRoot, releasesBaseURL });
    assert.equal(downloads, 1);
    const out = execFileSync(binPath, ["--version"], { encoding: "utf8" });
    assert.equal(out.trim(), "hc-mock --version");
    assert.equal(fs.existsSync(path.join(cacheRoot, version, target, "THIRD-PARTY-NOTICES")), true);

    // Cached run performs zero additional downloads.
    const cachedPath = await ensureBinary({ version, cacheRoot, releasesBaseURL });
    assert.equal(cachedPath, binPath);
    assert.equal(downloads, 1);

    // Tampered archive fails closed without installing a binary.
    corrupt = true;
    const badCache = path.join(tmp, "bad-cache");
    await assert.rejects(
      ensureBinary({ version, cacheRoot: badCache, releasesBaseURL }),
      /SHA-256 mismatch/
    );
    assert.equal(fs.existsSync(path.join(badCache, version, target, "herdr-core")), false);

    assert.throws(
      () => parseManifestTarget({ version: "0.1.0", targets: {} }, version, target),
      /Manifest version mismatch/
    );

    // Verify both bin/herdr-core.js and bin/hc.js forward arguments and fatal signals.
    const pkgVersion = JSON.parse(fs.readFileSync(new URL("./package.json", import.meta.url), "utf8")).version;
    const cliBinDir = path.join(cacheRoot, pkgVersion, target);
    fs.mkdirSync(cliBinDir, { recursive: true });
    fs.writeFileSync(
      path.join(cliBinDir, "herdr-core"),
      '#!/bin/sh\nif [ "$1" = "--sigterm" ]; then kill -TERM $$; fi\nprintf "ok:%s\\n" "$*"\n',
      { mode: 0o755 }
    );

    for (const rel of ["./bin/herdr-core.js", "./bin/hc.js"]) {
      const entry = fileURLToPath(new URL(rel, import.meta.url));
      const runRes = spawnSync(process.execPath, [entry, "list", "--json"], {
        env: { ...process.env, HC_NPM_CACHE_DIR: cacheRoot },
        encoding: "utf8",
      });
      assert.equal(runRes.status, 0);
      assert.equal(runRes.stdout.trim(), "ok:list --json");

      const sigRes = spawnSync(process.execPath, [entry, "--sigterm"], {
        env: { ...process.env, HC_NPM_CACHE_DIR: cacheRoot },
        encoding: "utf8",
      });
      assert.equal(sigRes.signal, "SIGTERM");
    }
  } finally {
    server.close();
    fs.rmSync(tmp, { recursive: true, force: true });
  }
});

test("homebrew-formula.sh renders Formula/herdr-core.rb from manifest.json and fails closed on missing targets", () => {
  const tmp = fs.mkdtempSync(path.join(os.tmpdir(), "hc-brew-test-"));
  const scriptPath = fileURLToPath(new URL("../homebrew-formula.sh", import.meta.url));
  const version = "0.1.22";
  const sha = (ch) => ch.repeat(64);
  const manifestPath = path.join(tmp, "manifest.json");
  const fullManifest = {
    version,
    generation: 1,
    notes: `herdr-core ${version}`,
    targets: {
      "macos-aarch64": { url: `https://releases.herdr.dev/core/${version}/herdr-core-macos-aarch64.tar.gz`, sha256: sha("a") },
      "macos-x86_64": { url: `https://releases.herdr.dev/core/${version}/herdr-core-macos-x86_64.tar.gz`, sha256: sha("b") },
      "linux-aarch64": { url: `https://releases.herdr.dev/core/${version}/herdr-core-linux-aarch64.tar.gz`, sha256: sha("c") },
      "linux-x86_64": { url: `https://releases.herdr.dev/core/${version}/herdr-core-linux-x86_64.tar.gz`, sha256: sha("d") },
    },
  };
  try {
    fs.writeFileSync(manifestPath, JSON.stringify(fullManifest, null, 2));
    const formula = execFileSync("sh", [scriptPath, version, manifestPath], { encoding: "utf8" });
    assert.match(formula, /class HerdrCore < Formula/);
    assert.match(formula, new RegExp(`sha256 "${sha("a")}"`));
    assert.match(formula, new RegExp(`sha256 "${sha("d")}"`));
    assert.match(formula, /bin\.install_symlink "herdr-core" => "hc"/);

    // Version mismatch fails closed with empty stdout.
    const mismatch = spawnSync("sh", [scriptPath, "0.2.0", manifestPath], { encoding: "utf8" });
    assert.notEqual(mismatch.status, 0);
    assert.equal(mismatch.stdout, "");

    // Missing target fails closed with empty stdout.
    delete fullManifest.targets["linux-arm64"];
    delete fullManifest.targets["linux-aarch64"];
    fs.writeFileSync(manifestPath, JSON.stringify(fullManifest, null, 2));
    const incomplete = spawnSync("sh", [scriptPath, version, manifestPath], { encoding: "utf8" });
    assert.notEqual(incomplete.status, 0);
    assert.equal(incomplete.stdout, "");
  } finally {
    fs.rmSync(tmp, { recursive: true, force: true });
  }
});
