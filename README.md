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

## Sidecar Contract

Configure the sidecar in `~/.romaji-agent/config.toml`:

```toml
sidecar_command = "/path/to/romaji-agent-lfm-sidecar"
sidecar_args = []
model_path = "/path/to/LFM2.5-1.2B-JP-202606-Q4_K_M.gguf"
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
