# MeshFlow

A self-hosted multi-agent AI workstation, written in Rust with [Freya](https://freyaui.dev).

No accounts, no subscriptions, no telemetry. Your API keys go in the OS keychain, your
conversations into a local SQLite file, and nothing leaves the machine except the requests you
make to the model endpoints you configure.

## Status

Phases 0–4 are done and running.

Two things those phases scoped are deliberately not built, and the reasons are the same shape:

- **The workspace index.** The tree walk is capped and reads each file once. An index adds a build
  pass, invalidation on every write, and a class of staleness bug, in exchange for nothing
  measurable until Find feels slow. That is the point to build it.
- **User-defined agents.** Delegation runs on three fixed roles, which is what makes the
  permission arithmetic checkable. Stored agent definitions are worth adding when someone wants a
  role these three cannot express — not as a settings screen nobody asked for.

| | |
|---|---|
| Chat with a provider | OpenAI-compatible and Anthropic, streaming |
| Tools | `read_file` `write_file` `list_dir` `search_files` `run_command` `delegate`, each behind a permission gate |
| Delegation | An agent hands a task to a sub-agent with a narrower role, never a wider one |
| Approval | Destructive calls stop and show you exactly what will happen — writes render as a diff |
| Workspace | A switchable root that bounds everything an agent can touch |
| Editor | File tree, tabs, syntax highlighting, undo, save |
| Search | Workspace-wide find, literal or regex, and a confirmed replace |
| Context | History is trimmed to the model's window at turn boundaries; `@path` attaches a file, with completion |
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
  exact diff. No timeout proceeds on its own. A sub-agent's calls stop at the same modal, so
  delegation is not a way to act without being asked.
- **A sub-agent is granted the intersection of its role and its parent's permissions**, never the
  role alone — otherwise `delegate` is how a read-only agent obtains a writer.
- The editor reads and writes through the same boundary as the agents, so a file that must never
  reach a model can never become an open tab either — nor an `@` attachment, which is read through
  that same policy rather than by naming a path.

## Build

```sh
cargo run --release
```

Then open Settings, add a provider and paste a key. Requires a Secret Service provider
(gnome-keyring, kwallet) on Linux for key storage; Windows uses Credential Manager and macOS the
login keychain, both without setup.

### A portable Windows build

```sh
./dev/build-windows.sh     # needs docker; produces dist/meshflow-windows-x64.zip
```

Cross-compiles to `x86_64-pc-windows-msvc` in a container — that triple specifically, because
Skia is only published prebuilt for MSVC and building it from source costs an hour and ~10 GB.
The CRT is linked statically, so the executable runs on a machine that has never seen a Visual C++
redistributable.

**Portable means the state travels with it.** Config, database and logs go to
`meshflow-data/` beside the executable when that directory exists — the zip ships with an empty
one — so the folder can live on a USB stick and leaves nothing behind on the machine that runs it.
Delete `meshflow-data/` and the same binary reverts to `%APPDATA%`. `MESHFLOW_HOME` overrides both.

API keys are the exception and stay in the OS credential store, which is per-user and per-machine:
a portable copy asks for the key again on a new machine. A credential that travelled around in a
folder on a stick would not be one worth having.

## Licence

MIT
