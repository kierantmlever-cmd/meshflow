# MeshFlow

A self-hosted multi-agent AI workstation, written in Rust with [Freya](https://freyaui.dev).

No accounts, no subscriptions, no telemetry. Your API keys go in the OS keychain, your
conversations into a local SQLite file, and nothing leaves the machine except the requests you
make to the model endpoints you configure.

## Status

Phases 0–2 are done and running; Phase 3 (index & context) is next.

| | |
|---|---|
| Chat with a provider | OpenAI-compatible and Anthropic, streaming |
| Tools | `read_file` `write_file` `list_dir` `search_files` `run_command`, each behind a permission gate |
| Approval | Destructive calls stop and show you exactly what will happen — writes render as a diff |
| Workspace | A switchable root that bounds everything an agent can touch |
| Editor | File tree, tabs, syntax highlighting, undo, save |
| Search | Workspace-wide find and a confirmed replace |
| Terminal | A real PTY |

## Design

Freya's reactivity is single-threaded and `!Send`. That is not an obstacle here, it is the
architecture: the app is two worlds joined by two channels, and the compiler physically prevents a
UI signal from reaching a background task.

```
mf-ui      single-threaded, owns every widget, knows freya exists
   │  EngineCommand  ↓            ↑  EngineEvent
mf-engine  Send + Sync, on Tokio, never mentions freya
```

- `crates/mf-engine` — providers, the agent loop, tools, the path policy, storage, search
- `crates/mf-ui` — every screen
- `crates/tree-sitter-skript` — a Skript grammar generated from the
  [Sk-VSC](https://github.com/AyhamAl-Ali/Sk-VSC) TextMate grammar, because none existed

## Security

The parts that are not negotiable, and are tested rather than asserted:

- **API keys live in the OS keychain.** Never in `config.toml`, never in the database, never in a
  log line. A test asserts the written config contains nothing resembling a key.
- **Every path an agent supplies is canonicalised before it is authorised**, so `..` and symlinks
  cannot walk out of the workspace. Credentials, SSH keys, `.env` files and VCS metadata are denied
  in every mode, including the unrestricted one.
- **Destructive tool calls need explicit consent**, shown verbatim — the exact command line, or the
  exact diff. No timeout proceeds on its own.
- The editor reads and writes through the same boundary as the agents, so a file that must never
  reach a model can never become an open tab either.

## Build

```sh
cargo run --release
```

Then open Settings, add a provider and paste a key. Requires a Secret Service provider
(gnome-keyring, kwallet) on Linux for key storage.

## Licence

MIT
