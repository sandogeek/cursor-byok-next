# Security

Security fixes target the latest release. Update to that release before reporting a vulnerability.

Please use [GitHub private vulnerability reporting](https://github.com/sandogeek/cursor-byok-nsh/security/advisories/new). Include the affected version, impact, and reproduction steps. Remove API keys, access tokens, and private conversation content. Please report vulnerabilities privately rather than through public issues.

安全修复面向最新版本。请通过上述链接私下报告漏洞，附上受影响版本、影响范围和复现步骤，并移除密钥、Token 和私人对话内容。

## What this app does that matters

- It installs a local CA and asks the user to trust it, then decrypts `*.cursor.sh` traffic on loopback. The CA's private key stays in the app's data directory (`~/.cursor-byok-next/ca/`) and never leaves the machine.
- It writes a managed proxy entry into Cursor's `settings.json`. Cleanup only removes an entry this app wrote (managed signature + loopback) and never touches a proxy the user or their employer configured.
- API keys live only in the user's own data directory. They are not logged; the call log keeps no authorization headers.

## Repositories

- Updater manifests are signed with the key whose public half is `nsh.json`'s `updater.publicKey`; an app refuses any other manifest.
- macOS packages are ad hoc signed and not notarized, so Gatekeeper may ask the user to confirm the first launch.