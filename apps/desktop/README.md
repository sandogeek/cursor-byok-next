# Cursor-BYOK-Next desktop app

An NSH mixed app: `App.tsx` is the entry (TSX), and this directory is also the Rust
crate (`nsh.json` `"rust": "."`) behind `cursor-byok-next/native`. The crate owns
the proxy, CA, local loop, plugins and call log; the TSX screens mirror that state.

```sh
nsh run                      # this machine's window
nsh check                    # cargo check of the crate, then the TSX types
nsh test                     # crate tests, then *.test.tsx, headless
nsh package --target macos   # or linux, windows
```

See the repository [README](../../README.md) for what the app does, and `docs/` for
the port records: each step's directory mapping, deliberate differences from
upstream, and the invariants it preserves.

`nsh check` regenerates `.nsh/types/cursor-byok-next/native.d.ts` from `src/lib.rs`;
that file is generated and not committed. NSH's declarations and guides come from
the `nsh` CLI (0.6.0); keep the `nsh-*` dependencies in `Cargo.toml` at that version.