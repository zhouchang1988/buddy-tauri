# AGENTS.md

This file provides guidance to AI agents (Codex, Claude Code, Kimi Code, etc.) when working with code in this repository.

## Project

Buddy is a macOS desktop app that orchestrates **dual-AI-agent collaborative coding**. Two AI actors (implementer + reviewer) take turns on a task, and the loop ends when both actors confirm "break" (dual-break). Supported actors: Claude Code, Codex, Cursor CLI, OpenCode, Kimi Code.

This repository is the **Tauri 2 port** of the Electron edition ([davidhoo/buddy](https://github.com/davidhoo/buddy)). The backend is Rust (`src-tauri/src/`), ported module by module from the Electron main process; the renderer is reused verbatim. Data directories (`~/Library/Application Support/buddy/`) are byte-for-byte compatible with the Electron edition.

## Commands

```bash
pnpm dev                    # Frontend dev only (vite HMR, no backend)
pnpm tauri:dev              # Full dev mode (Rust backend + frontend HMR)
pnpm test                   # Frontend unit tests (vitest run tests/unit)
pnpm vitest run tests/unit/renderer/sidebar.test.tsx  # Single test file
pnpm test:rust              # Rust unit tests (cargo test --manifest-path src-tauri/Cargo.toml)
pnpm typecheck              # tsc --noEmit
pnpm tauri build            # Build app bundle
pnpm dist                   # Build + DMG (alias of tauri build)
pnpm dist:arm64             # aarch64 (Apple Silicon) DMG
pnpm dist:intel             # x86_64 (Intel) DMG
pnpm dist:universal         # Universal DMG (both arches; needs rustup targets aarch64/x86_64-apple-darwin)
```

Both test suites must stay green: `cargo test` (338 tests) **and** `pnpm test` (231 tests).

## Architecture

**Tauri 2**: a Rust backend (replacing the Electron main process) + a system webview running the original React renderer.

### Bridge: `src/lib/tauri-bridge.ts` replaces preload

The renderer was ported with zero changes, so the bridge reconstructs the exact `window.buddy` / `window.api` shapes the Electron preload exposed, backed by `@tauri-apps/api` `invoke()`/`listen()`. It is imported for side effects at the top of `src/main.tsx`.

**Naming contract**: Electron IPC channel `buddy:xxx` ↔ Tauri command `buddy_xxx` (snake_case). Arguments are passed as a single object with camelCase keys matching the original positional parameter names. Event names are unchanged: `buddy:event`, `menu:action`, `updater:event`, `window:fullScreenChange`.

When adding a command: implement `#[tauri::command]` in `src-tauri/src/commands.rs`, register it in `src-tauri/src/lib.rs`'s `generate_handler!`, and add the matching method in `src/lib/tauri-bridge.ts` — all three must agree.

### Core: BuddyCoreService → BuddyStore + BuddyRunner + BuddyEventBus + QueueCoordinator

`src-tauri/src/buddy/service.rs` composes four modules:

- **Store** (`store.rs`): Filesystem persistence. All JSON writes are atomic (write `.tmp`, then `rename`). Schemas validate on read, not write, for forward compatibility.
- **Runner** (`runner.rs`): Spawns actor CLIs, drives the task state machine, parses streaming output.
- **EventBus** (`events.rs`): tokio broadcast pub/sub for task lifecycle events; `lib.rs` forwards every envelope to the frontend as a `buddy:event` emit.
- **QueueCoordinator** (`queue_coordinator.rs`): Instruction queue scheduling — instructions sent while an actor runs are queued and auto-executed after the round.

### Rust backend modules (`src-tauri/src/buddy/`)

| Module | Role |
|--------|------|
| `types.rs` | All shared serde types, field-by-field mirror of `src/shared/types.ts` (camelCase via rename) — guarantees byte-compatible JSON |
| `defaults.rs` | Default settings/values |
| `paths.rs` | Data root / workspace / task path resolution (workspace key = slug + sha256(path)[:12]) |
| `locks.rs` | File-based write/run locks |
| `task_id.rs` | `validate_task_id` — shared task-ID policy (mirrors `src/shared/task-id.ts`) |
| `schemas.rs` | Read-time validation with default filling (Zod-on-read equivalent) |
| `store.rs` | `BuddyStore`: atomic writes, `events.jsonl`/`transcript.jsonl`, instruction queue, `get_round_events`, `get_task_stats` |
| `events.rs` | `BuddyEventBus` (tokio broadcast) |
| `coalesce.rs` | `StdoutCoalescer` — merges high-frequency `actor.stdout` chunks before `lib.rs` emits them to the webview (keeps typing responsive during runs) |
| `redact.rs` | API-key/secret redaction before events are written |
| `shell_path.rs` | `fix_shell_path()` — repairs PATH for GUI-launched apps |
| `parsers.rs` | Streaming-output parsers for the 5 actor CLIs |
| `prompts.rs` | Byte-faithful prompt construction |
| `launchers.rs` | Native vs contract launcher detection and spawning (node-pty → `portable-pty`) |
| `queue_coordinator.rs` | Instruction queue coordination across rounds |
| `runner.rs` | `BuddyRunner`: state machine, child-process lifecycle, retry/recovery, health checks |
| `service.rs` | `BuddyCoreService`: façade composing store/runner/queue/bus |
| `git.rs` | Git status/diff/stage/commit/push, branches |
| `commit_message.rs` | One-shot AI commit-message generation (diff collection, prompt, output parsing, cancellable actor run) |
| `session_insight.rs` | Reads model/token usage from actor CLI session stores (kimi `wire.jsonl`, opencode SQLite/JSON) |
| `model_detect.rs` | Detects the effective model per actor from configs and CLI flags (incl. WeCode) |
| `notifications.rs` | Desktop notifications via tauri-plugin-notification |

Plus at the crate root: `commands.rs` (43 `#[tauri::command]` handlers), `menu.rs` (native menu), `updater.rs` (tauri-plugin-updater flow), `lib.rs` (wiring).

### `lib.rs` wiring order

`fix_shell_path()` → register plugins (dialog/notification/shell/opener/clipboard-manager/updater) → in `setup`: build `BuddyEventBus` + `BuddyCoreService` (with notifier factory) → spawn task forwarding the bus to `app.emit("buddy:event", ...)` → `block_on(service.recover_interrupted_runs())` (must run before the window is created, matching the Electron edition's `app.whenReady()` ordering) → `app.manage(service)` → `menu::setup_menu` → `updater::init_updater` → `generate_handler!` registers all 43 commands.

### Task state machine

```
READY → RUNNING_{ACTOR} → (READY | PAUSED | DONE)
                                 ↓
                              FAILED (recoverable)
                                 ↓
                              PAUSED
```

- **Dual-break**: Both actors must signal `type=break` for the task to reach DONE. Tracked via `pending_break` in state.
- **Recovery**: On app restart, tasks stuck in `RUNNING_*` are reset to `PAUSED` by `recover_interrupted_runs()`.

### Data model (filesystem, no database)

- Shared with the Electron edition: `~/Library/Application Support/buddy/`, byte-for-byte compatible JSON (enforced by `types.rs`)
- Workspaces keyed by repo path hash; per-task directory: `state.json`, `settings.json`, `task.md`, `context.md`, `events.jsonl`, `transcript.jsonl`, `artifacts/`
- Global settings: `dataRoot/global/settings.json`

### Renderer (`src/`)

- React 18 + TanStack React Query 5, copied verbatim from the Electron edition's `src/renderer/` (plus `shared/types.ts` / `shared/defaults.ts`).
- 23 preset themes (CSS custom properties), i18n (zh-CN/zh-TW/en with CJK auto-detect).
- `@` alias maps to `src/`.

## Conventions

- **Icons**: Use lucide-react. Do not introduce other icon libraries or custom SVGs.
- **Atomic writes**: Always write JSON via `.tmp` → `rename`, never direct write.
- **Schemas**: Defined in `src-tauri/src/buddy/schemas.rs`. Validate on read, not write.
- **Sensitive data**: API keys are automatically redacted from event logs by `redact.rs`.
- **i18n**: UI text goes through the `useI18n` hook. The prompt builder detects human language and instructs actors to reply in the same language.
- **Command naming**: `buddy:xxx` ↔ `buddy_xxx`; keep `commands.rs`, `lib.rs`, and `tauri-bridge.ts` in sync.
- **macOS traffic lights**: `trafficLightPosition.y` in `src-tauri/tauri.conf.json` is `28`, deliberately not the Electron edition's `19`. wry's inset code (`inset_traffic_lights`) lands the buttons 9pt above the configured y, so 28 reproduces the Electron layout (button frame top at 19, vertically centered with the 50px sidebar/titlebar row and its `mt-[4px]` toggle buttons).
- **Tests**: A change is not done until both `pnpm test:rust` and `pnpm test` (plus `pnpm typecheck`) are green. See `docs/TESTING.md`.
