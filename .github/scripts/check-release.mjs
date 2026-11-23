import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";

const root = new URL("../../", import.meta.url);
const read = (path) => readFileSync(new URL(path, root), "utf8");
const json = (path) => {
  const text = read(path);
  return JSON.parse(
    text
      .split(/\r?\n/)
      .filter((line) => !line.trimStart().startsWith("//"))
      .join("\n")
      .replace(/\/\*[\s\S]*?\*\//g, ""),
  );
};

// One version, in one place: `nsh.json` is what `nsh package` and
// `nsh update-manifest` read.
const app = json("apps/desktop/nsh.json");
const version = app.version;

assert.match(version, /^\d+\.\d+\.\d+$/, "nsh.json version must be stable SemVer");
assert.ok(Number.isInteger(app.build) && app.build > 0, "nsh.json build must be a positive integer");

// The release notes name the version, so raising one without the other fails here.
const changelog = read(`.github/release-notes/${version}.md`);
assert.equal(
  changelog.split(/\r?\n/, 1)[0],
  `# Cursor-BYOK-Next v${version}`,
  `release notes must exist at .github/release-notes/${version}.md and start with its title`,
);

assert.equal(app.name, "Cursor-BYOK-Next", "nsh.json name is the release name");
assert.equal(app.id, "dev.cursorbyoknext.app", "nsh.json id is the updater's app id");

// The updater needs a repository and a key, both from nsh.json: `nsh package`
// writes them into the app.
const updater = app.updater ?? {};
assert.match(String(updater.github), /^[\w.-]+\/[\w.-]+$/, "nsh.json updater.github must be owner/repo");
assert.match(
  String(updater.publicKey),
  /^[\w+/=]{43,44}$/,
  "nsh.json updater.publicKey must be the base64 of an Ed25519 public key",
);

// The desktop app is the crate that ships with the TSX; Cargo.lock must name
// the same version, or a `--locked` build would rewrite it.
const crateVersion = read("apps/desktop/Cargo.toml").match(/^version = "([^"]+)"/m)?.[1];
assert.match(String(crateVersion), /^\d+\.\d+\.\d+$/, "apps/desktop/Cargo.toml must give a version");
const locked = read("Cargo.lock").match(/name = "nsh-app-cursor-byok-next"\nversion = "([^"]+)"/)?.[1];
assert.equal(locked, crateVersion, "Cargo.lock must name the desktop crate at its Cargo.toml version");

// One pin for CI and the app: `nsh` rewrites `.nsh/rust`'s crates to its own
// version, so a workflow on one `nsh` and Cargo on another would retarget the
// app on one runner only. All three places must agree.
const nshVersion = read(".github/actions/install-nsh/action.yml").match(/default: (\d+\.\d+\.\d+)/)?.[1];
assert.match(String(nshVersion), /^\d+\.\d+\.\d+$/, "the install-nsh action must default to a version");
const cargoNsh = read("apps/desktop/Cargo.toml").match(/^nsh-core = "=?([^"]+)"/m)?.[1];
assert.equal(cargoNsh, nshVersion, "apps/desktop Cargo.toml must depend on the nsh the workflows install");
for (const workflow of ["ci.yml", "release.yml"]) {
  const pinned = read(`.github/workflows/${workflow}`).match(/NSH_VERSION: "([^"]+)"/)?.[1];
  assert.equal(pinned, nshVersion, `.github/workflows/${workflow} must pin NSH_VERSION to ${nshVersion}`);
}

for (const name of ["agent_v1.proto", "aiserver_v1.proto"]) {
  assert.ok(read(`protocols/cursor/${name}`).length > 0, `${name} is required`);
}

const args = process.argv.slice(2);
if (args.length) {
  assert.equal(args.length, 2, "usage: check-release.mjs [--tag vMAJOR.MINOR.PATCH]");
  assert.equal(args[0], "--tag");
  assert.equal(args[1], `v${version}`, "release tag must match the nsh.json version");
}
console.log(
  `Release configuration verified: ${updater.github} v${version} build ${app.build} (${fileURLToPath(root)})`,
);