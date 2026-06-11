# Romaji Agent

Romaji Agent is a Tauri 2 desktop app for fast Japanese input from romaji, typo-heavy, or unconverted text. It converts and refines locally, then copies or pastes the result back into the active app.

## Phase 1 MVP

- Global shortcut:
  - macOS: `Cmd+Shift+J`
  - Windows/Linux: `Ctrl+Shift+J`
- Quick input floating window with raw, converted, and refined output modes.
- Clipboard-backed selection transform preview.
- SQLite transform history and daily JSONL operation logs.
- Local sidecar inference contract over stdin/stdout JSON.
- Fallback deterministic terminology conversion from `memory.md` when no sidecar is configured.

## Local Files

The app creates:

```text
~/.romaji-agent/
  config.toml
  memory.md
  db.sqlite
  ops/
  models/
    lfm/
```

## Backends

`~/.romaji-agent/config.toml` selects the conversion backend:

```toml
backend = "sidecar" # "sidecar", "codex_app_server", or "fallback"
```

`fallback` uses only deterministic terminology replacement from `memory.md`.

### Codex App Server Backend

To use Codex App Server as the backend, install and authenticate the Codex CLI,
then configure:

```toml
backend = "codex_app_server"
codex_command = "codex"
codex_args = ["app-server"]
codex_model = "gpt-5.5" # optional; omit or set null to use the Codex default
codex_timeout_ms = 90000
```

Romaji Agent starts `codex app-server` over stdio for each transform, creates an
ephemeral thread with `approvalPolicy = "never"` and a read-only sandbox, and
asks for a JSON response matching the app's transform schema. If the selected
backend fails or times out, the app falls back to deterministic conversion.

See the official Codex App Server documentation:
<https://developers.openai.com/codex/app-server>

### Sidecar Contract

Configure the sidecar in `~/.romaji-agent/config.toml`:

```toml
backend = "sidecar"
sidecar_command = "/path/to/romaji-agent-lfm-sidecar"
sidecar_args = ["--max-tokens", "256", "--temperature", "0.1"]
model_path = "/path/to/LFM2.5-1.2B-JP-202606-Q4_K_M.gguf"
```

`model_path` is passed to the sidecar as `--model` unless `sidecar_args`
already contains `--model` or `--model=...`.

### Apple Foundation Models Sidecar

On macOS 26+ with Apple Intelligence enabled, Romaji Agent can use Apple's
on-device Foundation Models through the `ringo-fm` crates.io package:

- <https://crates.io/crates/ringo-fm/0.1.0>
- <https://crates.io/crates/ringo-fm-sys/0.1.0>

Build the Apple Foundation Models sidecar:

```bash
cd src-tauri
cargo build --bin romaji-agent-apple-fm-sidecar --release
```

Configure Romaji Agent to use it:

```toml
backend = "sidecar"
sidecar_command = "/path/to/romaji-agent-apple-fm-sidecar"
sidecar_args = ["--max-tokens", "256", "--temperature", "0.1"]
```

Run it directly for a contract smoke test:

```bash
printf '%s\n' '{"raw":"kyou mtg de hanasita todo","memory":"mtg -> ミーティング\ntodo -> TODO","context":{"timestamp":"2026-06-08T00:00:00Z","os":"macos","app_name":null,"process_id":null,"window_title":null}}' \
  | ./target/release/romaji-agent-apple-fm-sidecar
```

The app sends one JSON request line:

```json
{
  "raw": "kyou mtg de hanasita todo",
  "memory": "# Terminology\nmtg -> ミーティング\n",
  "context": {
    "timestamp": "2026-06-08T00:00:00Z",
    "os": "macos",
    "app_name": null,
    "process_id": null,
    "window_title": null
  }
}
```

The sidecar must print one JSON response line:

```json
{
  "converted": "今日 mtg で話した todo",
  "refined": "今日のミーティングで話した内容をTODO化する。",
  "confidence": 0.94
}
```

Use 1Password Developer Environments or another runtime injection mechanism for any model registry tokens. Do not put secrets in `config.toml`.

Build the Rust GGUF sidecar:

```bash
cd src-tauri
cargo build --bin romaji-agent-lfm-sidecar --release
```

Run it directly for a contract smoke test:

```bash
printf '%s\n' '{"raw":"kyou mtg de hanasita todo","memory":"mtg -> ミーティング","context":{"timestamp":"2026-06-08T00:00:00Z","os":"macos","app_name":null,"process_id":null,"window_title":null}}' \
  | ./target/release/romaji-agent-lfm-sidecar --model /path/to/model.gguf
```

## Development

```bash
pnpm install
pnpm tauri dev
```

```bash
pnpm build
pnpm lint
pnpm format
cd src-tauri && cargo test
```

## CLI

Build the CLI:

```bash
cd src-tauri
cargo build --bin romaji-agent-cli
```

Inspect the deterministic romaji-to-kana candidate:

```bash
./target/debug/romaji-agent-cli kana "konoguraino kakikata anara dou??"
```

Run the same transform path as the app, using `~/.romaji-agent/config.toml`:

```bash
./target/debug/romaji-agent-cli transform "konoguraino kakikata anara dou??"
```

For evaluation loops, read one input per line and emit JSONL:

```bash
printf '%s\n' "konoguraino kakikata anara dou??" "kyou mtg de hanasita todo" \
  | ./target/debug/romaji-agent-cli transform --stdin
```

CLI transforms do not write preview rows to SQLite unless `--save` is passed.
Use `--text` for quick human-readable output.

## Ubuntu 24.04 Debian Package

The Ubuntu package target is tested for Ubuntu 24.04 x86_64 on an X11 session.
Runtime paste support uses `xdotool`, which is declared as a Debian package
dependency.

Install build dependencies:

```bash
just ubuntu-deps
```

Build the `.deb` package:

```bash
just deb
```

The package is written to:

```text
src-tauri/target/release/bundle/deb/
```

Install the generated package with apt so package dependencies are resolved:

```bash
just install-deb
```

To install Ubuntu dependencies and build the package in one step:

```bash
just ubuntu-deb
```

## Windows Build And Distribution

Windows is supported as a Tauri desktop target with `Ctrl+Shift+J` as the default global shortcut.

### Prerequisites

- Node.js and `pnpm`
- Rust toolchain
- Microsoft C++ Build Tools
- WebView2 runtime

### Build

From PowerShell:

```powershell
pnpm install
pnpm build
pnpm tauri build
```

The main Windows installer artifact is the generated MSI under:

```text
src-tauri\target\release\bundle\msi\
```

### Paste Behavior On Windows

- Accept always writes the selected result to the clipboard first.
- Automatic paste is best-effort and uses Windows `SendKeys`.
- If auto-paste does not reach the previously active app, the text is still copied and can be pasted manually with `Ctrl+V`.
