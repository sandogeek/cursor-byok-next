# Cursor-BYOK-Next

Bring your own model APIs to Cursor, with a local takeover written in Rust and an
NSH (Native Shell) desktop UI.

> Beta. Configuration formats, data directories and the local CA are not stable
> between releases; see [Upgrading](#upgrading).

<p align="center">
  <img src="apps/desktop/assets/icon.svg" width="128" alt="Cursor-BYOK-Next">
</p>

English · [中文](./README-CN.md)

## What it does

Cursor-BYOK-Next makes Cursor talk to a model API you configure instead of the
bundled one:

```text
Cursor agent-host
      │  *.cursor.sh over the local proxy
      ▼
harness (Rust)            local CA, MITM for *.cursor.sh only, managed settings.json
      │
      ▼
backend (Rust)            InferenceService/RunInferenceSSE + BidiService/BidiAppend
      │
      ▼
Magpie plugin (QuickJS)   picks a model from your gateway, streams the response
      │
      ▼
Your OpenAI- / Anthropic-compatible API
```

| Piece | Where | What it owns |
|---|---|---|
| Takeover | `apps/desktop/src/harness/` | Proxy lifecycle, local CA, `settings.json`, local account cache |
| Local loop | `apps/desktop/src/loop_service/`, `backend/` | Cursor's inference stream, model catalog, server config |
| Plugin host | `apps/desktop/src/plugins/` | Isolated QuickJS VMs, Magpie catalog, Chat Completions / Responses / Anthropic adapters |
| Call log | `apps/desktop/src/calls/` | SQLite records of every Magpie `InvokeModel` |
| UI | `apps/desktop/App.tsx`, `src/screens/`, `src/ui/` | TSX screens mirroring Rust state |
| Debug tools | `crates/cursor-capture/`, `crates/cursor-protocol-extractor/` | Traffic capture and protobuf schema extraction |

The Rust side is the only writer of proxy, CA, settings and database state; TSX
holds a mirror and calls methods.

## Requirements

- macOS 14+, Windows 10 1809+ or Linux with GTK 4.14+ and glibc 2.39 (Ubuntu 24.04 and later)
- To build: Rust 1.97+ and the `nsh` CLI 0.6.0

The built-in Magpie plugin runs in an in-process QuickJS VM compiled into the
application, so no external JavaScript runtime is needed.

## Install

Releases are published from this repository. macOS packages are ad hoc signed and
not notarized, so Gatekeeper asks for confirmation on the first launch.

## Build

```sh
curl -fsSL https://nsh.dev/install.sh | sh    # NSH CLI; Windows: irm https://nsh.dev/install.ps1 | iex

cargo check --workspace --all-targets
cargo test --workspace
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo fmt --all

cd apps/desktop
nsh check        # crate + TSX type check
nsh test         # crate tests + *.test.tsx
nsh run          # the real window
nsh package --target macos
```

## Using it

1. Open the **Cursor** screen and initialize the local CA.
2. Copy the install command from the dialog, run it in a terminal, and trust the CA.
   The app only copies the command; it never runs it or asks for a password itself.
3. Turn on takeover. This writes a managed proxy entry into Cursor's `settings.json`.
   Restart Cursor if it was running.
4. Fill in your Magpie gateway address and API key on the **Plugins** screen, then
   refresh the model catalog.
5. Pick one of those models in Cursor's model picker.

Stopping takeover clears `settings.json` first and then stops the proxy, so Cursor is
never left pointing at a dead port. Cleanup only removes an entry this app wrote.

## Where data lives

```text
~/.cursor-byok-next/
├── harness.json            takeover intent, preferred port, last bound port
├── ca/                     the local CA certificate and its private key
├── calls.db                call records and the detailed-logging switch
└── plugins/                Magpie gateways and their catalog cache
```

The CA private key never leaves the machine. API keys stay in the plugin data
directory; the call log stores no authorization headers.

## Upgrading

This is a beta, so breaking changes are made in place rather than behind
compatibility layers. Renaming the app to Cursor-BYOK-Next moved the data
directory, which means an upgrade starts fresh: initialize and trust the CA again
and turn takeover back on. The old `Cursor BYOK Local CA` can be removed by hand.

## Documentation

- `docs/protocol/` — what the Cursor protocol fields mean, with evidence
- `apps/desktop/docs/` — port records for each step: directory mapping, every
  intentional difference from upstream and why, and the invariants it must keep

## Acknowledgements

Cursor-BYOK-Next ports the local takeover of
[cursor-byok](https://github.com/hainuoli/cursor-byok), which is based on
[leookun/cursor-byok](https://github.com/leookun/cursor-byok). The Magpie plugin is
by [@leookun](https://github.com/leookun).

Released under the [MIT License](./LICENSE). Upstream notices are preserved in
[Third-Party Licenses](./THIRD-PARTY-LICENSES.md).

## Security

Please report vulnerabilities privately; see [SECURITY.md](./SECURITY.md).