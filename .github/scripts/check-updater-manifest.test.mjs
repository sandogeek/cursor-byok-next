import assert from "node:assert/strict";
import test from "node:test";
import {
  archiveName,
  archiveNames,
  checkManifest,
  verifyUpdaterManifest,
} from "./check-updater-manifest.mjs";

const app = { name: "Cursor-BYOK-Next", id: "dev.cursorbyoknext.app", version: "1.2.3" };

function manifest(overrides = {}) {
  return {
    id: app.id,
    version: app.version,
    platforms: {
      "macos-aarch64": {
        url: "Cursor-BYOK-Next-1.2.3-macos-aarch64.tar.gz",
        sha256: "a".repeat(64),
        size: 10,
      },
      "windows-x86_64": {
        url: "Cursor-BYOK-Next-1.2.3-windows-x86_64.zip",
        sha256: "b".repeat(64),
        size: 20,
      },
    },
    ...overrides,
  };
}

const measured = new Map([
  ["Cursor-BYOK-Next-1.2.3-macos-aarch64.tar.gz", { sha256: "a".repeat(64), size: 10 }],
  ["Cursor-BYOK-Next-1.2.3-windows-x86_64.zip", { sha256: "b".repeat(64), size: 20 }],
]);

const names = archiveNames({
  name: app.name,
  version: app.version,
  platforms: ["macos-aarch64", "windows-x86_64"],
});

const measure = (file) => {
  const found = measured.get(file);
  if (!found) throw new Error(`${file} is not one of the release's archives`);
  return found;
};

test("nsh's archive names: <name>-<version>-<platform>.<ext>", () => {
  assert.equal(
    archiveName({ name: app.name, version: app.version, platform: "macos-aarch64" }),
    "Cursor-BYOK-Next-1.2.3-macos-aarch64.tar.gz",
  );
  assert.equal(
    archiveName({ name: app.name, version: app.version, platform: "linux-aarch64" }),
    "Cursor-BYOK-Next-1.2.3-linux-aarch64.tar.gz",
  );
  assert.equal(
    archiveName({ name: app.name, version: app.version, platform: "windows-x86_64" }),
    "Cursor-BYOK-Next-1.2.3-windows-x86_64.zip",
  );
  assert.throws(
    () => archiveName({ name: app.name, version: app.version, platform: "ios" }),
    /unsupported platform/,
  );
});

test("accepts a manifest that matches the archives and their hashes", () => {
  assert.deepEqual(checkManifest(manifest(), names, { id: app.id, version: app.version, measure }), manifest());
});

test("refuses a manifest for another app or version", () => {
  assert.throws(
    () => checkManifest(manifest({ id: "dev.other.app" }), names, { id: app.id, version: app.version, measure }),
    /is for dev\.other\.app/,
  );
  assert.throws(
    () => checkManifest(manifest({ version: "1.2.2" }), names, { id: app.id, version: app.version, measure }),
    /expected 1\.2\.3/,
  );
});

test("refuses a manifest missing a platform this release built", () => {
  const missing = manifest();
  delete missing.platforms["windows-x86_64"];
  assert.throws(
    () => checkManifest(missing, names, { id: app.id, version: app.version, measure }),
    /missing windows-x86_64/,
  );
});

test("refuses a platform no archive of this release covers", () => {
  const extra = manifest();
  extra.platforms["linux-x86_64"] = {
    url: "Cursor-BYOK-Next-1.2.3-linux-x86_64.tar.gz",
    sha256: "c".repeat(64),
    size: 30,
  };
  assert.throws(
    () => checkManifest(extra, names, { id: app.id, version: app.version, measure }),
    /no packaged archive covers/,
  );
});

test("refuses an archive replaced after the manifest was written", () => {
  const tampered = manifest();
  tampered.platforms["macos-aarch64"].sha256 = "d".repeat(64);
  assert.throws(
    () => checkManifest(tampered, names, { id: app.id, version: app.version, measure }),
    /changed after the manifest was written/,
  );
  const resized = manifest();
  resized.platforms["windows-x86_64"].size = 21;
  assert.throws(
    () => checkManifest(resized, names, { id: app.id, version: app.version, measure }),
    /manifest says 21 bytes/,
  );
});

test("refuses a manifest pointing at another file", () => {
  const renamed = manifest();
  renamed.platforms["windows-x86_64"].url = "Cursor-BYOK-Next-1.2.3-windows-amd64.zip";
  assert.throws(
    () => checkManifest(renamed, names, { id: app.id, version: app.version, measure }),
    /points at Cursor-BYOK-Next-1\.2\.3-windows-amd64\.zip/,
  );
});

test("verifyUpdaterManifest reads latest.json beside the archives", async () => {
  const { mkdtemp, writeFile } = await import("node:fs/promises");
  const { tmpdir } = await import("node:os");
  const { join } = await import("node:path");
  const { createHash } = await import("node:crypto");

  const directory = await mkdtemp(join(tmpdir(), "nsh-manifest-"));
  const files = {
    "Cursor-BYOK-Next-1.2.3-macos-aarch64.tar.gz": "mac",
    "Cursor-BYOK-Next-1.2.3-windows-x86_64.zip": "windows",
  };
  const platforms = {};
  for (const [file, body] of Object.entries(files)) {
    await writeFile(join(directory, file), body);
    platforms[file.includes("macos") ? "macos-aarch64" : "windows-x86_64"] = {
      url: file,
      sha256: createHash("sha256").update(body).digest("hex"),
      size: Buffer.byteLength(body),
    };
  }
  await writeFile(
    join(directory, "latest.json"),
    `${JSON.stringify({ id: app.id, version: app.version, platforms }, null, 2)}\n`,
  );
  const verified = await verifyUpdaterManifest({
    manifestPath: join(directory, "latest.json"),
    app,
    platforms: ["macos-aarch64", "windows-x86_64"],
  });
  assert.equal(verified.id, app.id);

  // One archive short of the platform the release built: refused.
  await assert.rejects(
    verifyUpdaterManifest({
      manifestPath: join(directory, "latest.json"),
      app,
      platforms: ["macos-aarch64", "linux-x86_64"],
    }),
    /ENOENT/,
  );
});