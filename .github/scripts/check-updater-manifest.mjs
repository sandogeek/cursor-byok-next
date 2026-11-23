import { createHash } from "node:crypto";
import { readFile, stat } from "node:fs/promises";
import { dirname, resolve } from "node:path";
import { pathToFileURL } from "node:url";

function readOptions(args) {
  const options = new Map();
  for (let index = 0; index < args.length; index += 2) {
    const name = args[index];
    const value = args[index + 1];
    if (!name?.startsWith("--") || value === undefined) {
      throw new Error(`invalid argument near ${name ?? "end of command"}`);
    }
    options.set(name.slice(2), value);
  }
  return options;
}

function required(options, name) {
  const value = options.get(name)?.trim();
  if (!value) throw new Error(`--${name} is required`);
  return value;
}

const ARCHIVE_EXTENSIONS = { macos: "tar.gz", linux: "tar.gz", windows: "zip" };

/**
 * The archive `nsh package` writes for one platform, named as
 * `nsh update-manifest` requires: `<name>-<version>-<os>-<arch>.<ext>`.
 *
 * The platform key is the one `nsh_updater::platform()` reports at run
 * time (`macos-aarch64`, `linux-x86_64`, …), so it is also the key the
 * manifest must carry for that machine to find itself.
 */
export function archiveName({ name, version, platform }) {
  const os = platform.split("-")[0];
  const extension = ARCHIVE_EXTENSIONS[os];
  if (!extension) throw new Error(`unsupported platform: ${platform}`);
  return `${name}-${version}-${platform}.${extension}`;
}

export function archiveNames({ name, version, platforms }) {
  const names = new Map();
  for (const platform of platforms) {
    names.set(platform, archiveName({ name, version, platform }));
  }
  return names;
}

/**
 * Throws unless `latest.json` offers every archive under its own name, with
 * the size and SHA-256 the uploaded file has, and offers no other platform.
 * `measure(file)` returns what the file on disk is; it throws when the file
 * is absent.
 */
export function checkManifest(manifest, names, { id, version, measure }) {
  if (manifest.id !== id) {
    throw new Error(`updater manifest is for ${manifest.id ?? "no app"}; expected ${id}`);
  }
  if (manifest.version !== version) {
    throw new Error(`updater manifest version ${manifest.version ?? "is missing"}; expected ${version}`);
  }
  const platforms = manifest.platforms ?? {};
  for (const platform of Object.keys(platforms)) {
    if (!names.has(platform)) {
      throw new Error(`updater manifest has ${platform}, which no packaged archive covers`);
    }
  }
  for (const [platform, file] of names) {
    const entry = platforms[platform];
    if (!entry) throw new Error(`updater manifest is missing ${platform} (${file})`);
    if (entry.url !== file) {
      throw new Error(`updater platform ${platform} points at ${entry.url}; expected ${file}`);
    }
    const actual = measure(file);
    if (entry.size !== actual.size) {
      throw new Error(`updater platform ${platform}: manifest says ${entry.size} bytes, ${file} has ${actual.size}`);
    }
    if (entry.sha256 !== actual.sha256) {
      throw new Error(`updater platform ${platform}: ${file} changed after the manifest was written`);
    }
  }
  return manifest;
}

/** Reads `latest.json` and checks it against the archives next to it. */
export async function verifyUpdaterManifest({
  manifestPath,
  app,
  platforms,
}) {
  const names = archiveNames({ name: app.name, version: app.version, platforms });
  const measured = new Map();
  for (const file of new Set(names.values())) {
    const path = resolve(dirname(manifestPath), file);
    measured.set(file, {
      size: (await stat(path)).size,
      sha256: createHash("sha256").update(await readFile(path)).digest("hex"),
    });
  }
  const manifest = JSON.parse(await readFile(manifestPath, "utf8"));
  return checkManifest(manifest, names, {
    id: app.id,
    version: app.version,
    measure: (file) => {
      const digest = measured.get(file);
      if (!digest) throw new Error(`${file} is not one of the release's archives`);
      return digest;
    },
  });
}

async function main() {
  const options = readOptions(process.argv.slice(2));
  const app = JSON.parse(await readFile(resolve(required(options, "app")), "utf8"));
  const platforms = required(options, "platforms")
    .split(",")
    .map((platform) => platform.trim())
    .filter(Boolean);
  const manifest = await verifyUpdaterManifest({
    manifestPath: resolve(required(options, "manifest")),
    app,
    platforms,
  });
  console.log(`Updater manifest verified: ${manifest.id} v${manifest.version}, ${platforms.length} platforms`);
}

if (process.argv[1] && import.meta.url === pathToFileURL(resolve(process.argv[1])).href) {
  main().catch((error) => {
    console.error(error instanceof Error ? error.message : String(error));
    process.exitCode = 1;
  });
}