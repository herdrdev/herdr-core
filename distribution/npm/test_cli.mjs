import assert from "node:assert/strict";
import crypto from "node:crypto";
import fs from "node:fs";
import http from "node:http";
import os from "node:os";
import path from "node:path";
import { execFileSync, spawnSync } from "node:child_process";
import test from "node:test";
import { fileURLToPath, pathToFileURL } from "node:url";

import {
  ensureBinary,
  parseManifestTarget,
  resolveTarget,
  verifyDarwinSignature,
} from "./bin/herdr-core.js";

test("resolveTarget maps supported OS/arch pairs and rejects unsupported ones", () => {
  assert.equal(resolveTarget("darwin", "arm64"), "macos-aarch64");
  assert.equal(resolveTarget("darwin", "x64"), "macos-x86_64");
  assert.equal(resolveTarget("linux", "arm64"), "linux-aarch64");
  assert.equal(resolveTarget("linux", "x64"), "linux-x86_64");
  assert.throws(() => resolveTarget("win32", "x64"), /Unsupported platform/);
});

test("package.json bin manifest resolves unambiguously via npm exec's getBinFromManifest", async () => {
  const pkg = JSON.parse(fs.readFileSync(new URL("./package.json", import.meta.url), "utf8"));
  assert.equal(pkg.bin["herdr-core"], "bin/herdr-core.js");
  assert.equal(pkg.bin.hc, "bin/herdr-core.js");

  // Locate npm's own libnpmexec/get-bin-from-manifest.js if installed alongside node.
  const npmBin = execFileSync("which", ["npm"], { encoding: "utf8" }).trim();
  const npmReal = fs.realpathSync(npmBin);
  const npmRoot = path.resolve(path.dirname(npmReal), "..");
  const candidates = [
    path.join(npmRoot, "node_modules", "libnpmexec", "lib", "get-bin-from-manifest.js"),
    path.join(npmRoot, "lib", "node_modules", "npm", "node_modules", "libnpmexec", "lib", "get-bin-from-manifest.js"),
  ];
  const helperPath = candidates.find((candidate) => fs.existsSync(candidate));
  assert.ok(helperPath, "expected npm's libnpmexec get-bin-from-manifest.js");
  const mod = await import(pathToFileURL(helperPath).href);
  const getBinFromManifest = mod.default || mod;
  assert.ok(["herdr-core", "hc"].includes(getBinFromManifest(pkg)));
});

test("ensureBinary verifies manifest SHA-256, Darwin codesign, caching, tampered archives, and CLI aliases", async () => {
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
    assert.match(binPath, /\/mirrors\/[0-9a-f]{16}\//);
    assert.equal(fs.existsSync(path.join(path.dirname(binPath), "THIRD-PARTY-NOTICES")), true);

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
    assert.equal(fs.existsSync(binPath.replace(cacheRoot, badCache)), false);

    // Official macOS release checks Apple Developer ID signature before publishing to cache.
    const unsignedDarwinArchiveName = "herdr-core-macos-aarch64.tar.gz";
    const fakeOfficialFetch = async (url) => {
      if (url === `https://releases.herdr.dev/core/${version}/manifest.json`) {
        const body = Buffer.from(
          JSON.stringify({
            version,
            generation: 1,
            notes: `herdr-core ${version}`,
            targets: {
              "macos-aarch64": {
                url: `https://releases.herdr.dev/core/${version}/${unsignedDarwinArchiveName}`,
                sha256: sha,
              },
            },
          })
        );
        return { ok: true, status: 200, arrayBuffer: async () => body };
      }
      if (url === `https://releases.herdr.dev/core/${version}/${unsignedDarwinArchiveName}`) {
        return { ok: true, status: 200, arrayBuffer: async () => tarBytes };
      }
      return { ok: false, status: 404, arrayBuffer: async () => Buffer.alloc(0) };
    };
    const failingCodesign = path.join(tmp, "fake-codesign");
    fs.writeFileSync(failingCodesign, "#!/bin/sh\nexit 1\n", { mode: 0o755 });
    const codesignBin = process.platform === "darwin" && fs.existsSync("/usr/bin/codesign")
      ? "/usr/bin/codesign"
      : failingCodesign;

    const unsignedCache = path.join(tmp, "unsigned-cache");
    // Mirror-to-official transition in the SAME cacheRoot must not reuse the unsigned mirror entry.
    corrupt = false;
    const mirrorDarwinFetch = async (url) => {
      if (url === `http://127.0.0.1:${port}/core/${version}/manifest.json`) {
        const body = Buffer.from(
          JSON.stringify({
            version,
            generation: 1,
            notes: `herdr-core ${version}`,
            targets: {
              "macos-aarch64": {
                url: `http://127.0.0.1:${port}/core/${version}/${unsignedDarwinArchiveName}`,
                sha256: sha,
              },
            },
          })
        );
        return { ok: true, status: 200, arrayBuffer: async () => body };
      }
      if (url === `http://127.0.0.1:${port}/core/${version}/${unsignedDarwinArchiveName}`) {
        return { ok: true, status: 200, arrayBuffer: async () => tarBytes };
      }
      return { ok: false, status: 404, arrayBuffer: async () => Buffer.alloc(0) };
    };
    await ensureBinary({
      version,
      cacheRoot: unsignedCache,
      releasesBaseURL,
      platform: "darwin",
      arch: "arm64",
      fetchImpl: mirrorDarwinFetch,
      codesignBin,
    });
    await assert.rejects(
      ensureBinary({
        version,
        cacheRoot: unsignedCache,
        releasesBaseURL: "https://releases.herdr.dev",
        platform: "darwin",
        arch: "arm64",
        fetchImpl: fakeOfficialFetch,
        codesignBin,
      }),
      /Not a signed Herdr release/
    );
    assert.equal(fs.existsSync(path.join(unsignedCache, version, "macos-aarch64", "herdr-core")), false);
    assert.throws(
      () => verifyDarwinSignature(path.join(buildDir, "herdr-core"), { codesignBin }),
      /Not a signed Herdr release/
    );

    assert.throws(
      () => parseManifestTarget({ version: "0.1.0", targets: {} }, version, target),
      /Manifest version mismatch/
    );

    // Verify both invoked bin names (herdr-core.js and symlinked hc) forward arguments and fatal signals.
    const pkgVersion = JSON.parse(fs.readFileSync(new URL("./package.json", import.meta.url), "utf8")).version;
    const mirrorKey = crypto.createHash("sha256").update(releasesBaseURL).digest("hex").slice(0, 16);
    const cliBinDir = path.join(cacheRoot, "mirrors", mirrorKey, pkgVersion, target);
    fs.mkdirSync(cliBinDir, { recursive: true });
    fs.writeFileSync(
      path.join(cliBinDir, "herdr-core"),
      '#!/bin/sh\nif [ "$1" = "--sigterm" ]; then kill -TERM $$; fi\nprintf "ok:%s\\n" "$*"\n',
      { mode: 0o755 }
    );

    const launcher = fileURLToPath(new URL("./bin/herdr-core.js", import.meta.url));
    const hcSymlinkDir = path.join(tmp, "bin-links");
    fs.mkdirSync(hcSymlinkDir, { recursive: true });
    const hcLink = path.join(hcSymlinkDir, "hc");
    fs.symlinkSync(launcher, hcLink);

    for (const entry of [launcher, hcLink]) {
      const runRes = spawnSync(process.execPath, [entry, "list", "--json"], {
        env: { ...process.env, HC_NPM_CACHE_DIR: cacheRoot, HERDR_RELEASES_URL: releasesBaseURL },
        encoding: "utf8",
      });
      assert.equal(runRes.status, 0);
      assert.equal(runRes.stdout.trim(), "ok:list --json");

      const sigRes = spawnSync(process.execPath, [entry, "--sigterm"], {
        env: { ...process.env, HC_NPM_CACHE_DIR: cacheRoot, HERDR_RELEASES_URL: releasesBaseURL },
        encoding: "utf8",
      });
      assert.equal(sigRes.signal, "SIGTERM");
    }
  } finally {
    server.close();
    fs.rmSync(tmp, { recursive: true, force: true });
  }
});

test("homebrew-formula.sh renders Formula/herdr-core.rb from manifest.json and fails closed on invalid URLs or missing targets", () => {
  const tmp = fs.mkdtempSync(path.join(os.tmpdir(), "hc-brew-test-"));
  const scriptPath = fileURLToPath(new URL("../homebrew-formula.sh", import.meta.url));
  const version = "0.1.22";
  const sha = (ch) => ch.repeat(64);
  const manifestPath = path.join(tmp, "manifest.json");
  const makeManifest = () => ({
    version,
    generation: 1,
    notes: `herdr-core ${version}`,
    targets: {
      "macos-aarch64": { url: `https://releases.herdr.dev/core/${version}/herdr-core-macos-aarch64.tar.gz`, sha256: sha("a") },
      "macos-x86_64": { url: `https://releases.herdr.dev/core/${version}/herdr-core-macos-x86_64.tar.gz`, sha256: sha("b") },
      "linux-aarch64": { url: `https://releases.herdr.dev/core/${version}/herdr-core-linux-aarch64.tar.gz`, sha256: sha("c") },
      "linux-x86_64": { url: `https://releases.herdr.dev/core/${version}/herdr-core-linux-x86_64.tar.gz`, sha256: sha("d") },
    },
  });
  try {
    fs.writeFileSync(manifestPath, JSON.stringify(makeManifest(), null, 2));
    const formula = execFileSync(scriptPath, [version, manifestPath], { encoding: "utf8" });
    assert.match(formula, /class HerdrCore < Formula/);
    assert.match(formula, new RegExp(`sha256 "${sha("a")}"`));
    assert.match(formula, new RegExp(`sha256 "${sha("d")}"`));
    assert.match(formula, /bin\.install_symlink "herdr-core" => "hc"/);

    // Version mismatch fails closed with empty stdout.
    const mismatch = spawnSync(scriptPath, ["0.2.0", manifestPath], { encoding: "utf8" });
    assert.notEqual(mismatch.status, 0);
    assert.equal(mismatch.stdout, "");

    // Invalid or non-HTTPS URL fails closed with empty stdout.
    for (const badURL of [
      "not-a-url",
      "http://releases.herdr.dev/core/0.1.22/herdr-core.tar.gz",
      "https://:443/actual-name.tar.gz",
      "https://releases.herdr.dev:bad/actual-name.tar.gz",
      "https://releases.herdr.dev/core/0.1.22/a.tar.gz#frag",
    ]) {
      const bad = makeManifest();
      bad.targets["macos-aarch64"].url = badURL;
      fs.writeFileSync(manifestPath, JSON.stringify(bad, null, 2));
      const badRes = spawnSync(scriptPath, [version, manifestPath], { encoding: "utf8" });
      assert.notEqual(badRes.status, 0);
      assert.equal(badRes.stdout, "");
    }

    // Missing target fails closed with empty stdout.
    const incompleteManifest = makeManifest();
    delete incompleteManifest.targets["linux-aarch64"];
    fs.writeFileSync(manifestPath, JSON.stringify(incompleteManifest, null, 2));
    const incomplete = spawnSync(scriptPath, [version, manifestPath], { encoding: "utf8" });
    assert.notEqual(incomplete.status, 0);
    assert.equal(incomplete.stdout, "");
  } finally {
    fs.rmSync(tmp, { recursive: true, force: true });
  }
});
