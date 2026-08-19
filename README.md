# Buddy (Tauri)

> ⚠️ **项目说明：研究性实验，非稳定项目**
>
> 本仓库是为了**研究「用 AI 编程切换开发方案」**而产生的实验项目——即探索如何让 AI Agent 将一个完整的 Electron 应用移植到 Tauri 2（Rust 后端）。它不是一个面向生产环境的稳定项目，功能与质量不做保证。
>
> **如果你希望更好地体验和使用 Buddy，请使用原项目：**
>
> - 仓库：[davidhoo/buddy](https://github.com/davidhoo/buddy)
> - 官网：https://davidhoo.github.io/buddy/

Buddy 是一个双 AI Agent 协作编码的 macOS 桌面应用（两个 AI Actor 轮流工作，双确认结束）。本仓库是它的 **Tauri 2 移植实验版**：后端由 Node.js 主进程重写为 Rust，前端渲染代码原样复用，数据目录与原版兼容。

## 特性

- **双 Actor 协作**：执行方实现代码，审查方检查修正，循环推进直到双方确认完成
- **5 种 AI Actor**：Claude Code、Codex、Cursor CLI、OpenCode、Kimi Code
- **双确认结束**：双方均发出 `type=break` 才结束任务，单方 break 不终止
- **指令队列**：在 Actor 运行期间排队发送指令，轮次结束后自动执行
- **Git 集成**：本地化 conventional commit 消息生成、变更查看、提交与推送
- **23 套预设主题**：CSS 自定义属性驱动的主题引擎，支持自定义颜色
- **国际化**：中文简体 / 中文繁体 / 英文，CJK 自动检测
- **可恢复**：应用崩溃或重启后，任务状态从磁盘文件完整恢复

## 系统要求

- macOS 12+ (Monterey)
- 至少一个已安装的 AI CLI 工具：[Claude Code](https://docs.anthropic.com/en/docs/claude-code)、[Codex CLI](https://github.com/openai/codex)、[Cursor CLI](https://docs.cursor.com/en/cli/overview)、[OpenCode](https://github.com/opencode-ai/opencode)、[Kimi Code](https://github.com/MoonshotAI/kimi-cli)
- 仅开发需要：Rust toolchain（`rustup`）、Node.js >= 18、pnpm 11

## 安装

从源码构建：

```bash
pnpm install
pnpm dist
```

DMG 输出在 `src-tauri/target/release/bundle/dmg/` 目录。

## 开发

```bash
pnpm install          # 安装依赖
pnpm dev              # 前端开发模式（仅 vite HMR，无后端）
pnpm tauri:dev        # 完整开发模式（Rust 后端 + 前端 HMR）
pnpm test             # 前端单元测试 (vitest)
pnpm test:rust        # Rust 单元测试 (cargo test --manifest-path src-tauri/Cargo.toml)
pnpm typecheck        # 类型检查
pnpm tauri build      # 构建应用包
pnpm dist             # 构建 + DMG（tauri build 的别名）
```

## 架构

Tauri 2 架构：**Rust 后端**承担原 Electron 主进程的全部职责，前端渲染代码与原版一致。桥接层 `src/lib/tauri-bridge.ts` 在 `window` 上挂出与原版 preload 同名同形的 `window.buddy` / `window.api`，内部走 `@tauri-apps/api` 的 `invoke`/`listen`。

```
┌──────────────────────────────────────────────┐
│            Rust Core (src-tauri)              │
│  BuddyCoreService                             │
│  ├── BuddyStore      (原子写入持久层)          │
│  ├── BuddyRunner     (状态机 + 子进程调度)     │
│  ├── BuddyEventBus   (事件发布/订阅)          │
│  └── QueueCoordinator (指令队列协调)          │
│                                               │
│  Launchers → Claude / Codex / Cursor / OpenCode / Kimi │
│  Git Integration → diff / status / commit     │
└──────────────────┬───────────────────────────┘
                   │ Tauri IPC (buddy_* commands)
                   │ 事件推送 buddy:event
┌──────────────────┴───────────────────────────┐
│              Webview (前端)                   │
│  React 18 + TanStack React Query 5            │
│  tauri-bridge.ts → window.buddy / window.api  │
│  ┌────────┬──────────────┬──────────────┐    │
│  │Sidebar │ Chat         │ Right Panel   │    │
│  │ 260px  │ (flex)       │  400px       │    │
│  └────────┴──────────────┴──────────────┘    │
└──────────────────────────────────────────────┘
```

### 任务状态机

```
READY → RUNNING_{ACTOR} → (READY | PAUSED | DONE)
                                 ↓
                              FAILED (recoverable)
                                 ↓
                              PAUSED
```

- Actor 完成后直接启动下一轮（已移除倒计时机制）
- 双方均 `type=break` → DONE
- 失败可恢复，连续失败达上限 → PAUSED
- 应用重启时，`RUNNING_*` 状态的任务自动重置为 `PAUSED`

### 数据模型

纯文件系统，无数据库。**与 Electron 版共享同一数据目录** `~/Library/Application Support/buddy/`，JSON 字段名逐字节兼容（由 `src-tauri/src/buddy/types.rs` 保证），任务可在两版之间无缝切换。

```
buddy/
├── global/
│   └── settings.json           # 全局设置
└── workspaces/
    └── {project-hash}/
        ├── workspace.json
        └── tasks/
            └── {task_id}/
                ├── state.json       # 任务状态
                ├── settings.json    # 任务设置
                ├── task.md          # 任务目标
                ├── context.md       # 上下文
                ├── events.jsonl     # 事件流
                ├── transcript.jsonl # 对话记录
                └── artifacts/       # 产物文件
```

所有 JSON 写入均为原子操作（`.tmp` → `rename`），schema 在读取时校验（前向兼容）。

### IPC 通信

- **请求-响应**：前端 `invoke('buddy_xxx')` ↔ Rust `#[tauri::command] buddy_xxx`。原 Electron 的 `buddy:xxx` channel 一一对应 snake_case command `buddy_xxx`，参数以单个 camelCase 对象传递
- **推送事件**：Rust 端 `app.emit('buddy:event', ...)` → 前端 `listen('buddy:event')`，由 `BuddyEventBus`（tokio broadcast）转发

## Actor 启动适配

| Actor | 调用方式 | Session 复用 |
|-------|---------|-------------|
| Claude | `{cmd} -p --output-format stream-json --input-format text [--resume SID]` | `--resume` |
| Codex | `{cmd} exec --json -C REPO -o OUTPUT [resume SID]` | `exec resume` |
| Cursor | `{cmd} --print --force --output-format stream-json --stream-partial-output [--resume SID] PROMPT` | `--resume` |
| OpenCode | `{cmd} run --format json [--session SID]` | `--session` |
| Kimi | `{cmd} --print --output-format stream-json --input-format text [--session SID]` | `--session` |

非原生命令使用契约模式，传递 `BUDDY_ACTOR`、`BUDDY_MODE` 等环境变量。

## 技术栈

| 层面 | 选型 |
|------|------|
| 运行时 | Tauri 2（Rust 后端 + 系统 Webview） |
| 后端语言 | Rust（`src-tauri/src/buddy/`） |
| 前端语言 | TypeScript 5 |
| UI | React 18 + Tailwind CSS 3 |
| 构建 | Vite 7 + tauri-cli |
| 包管理 | pnpm |
| 打包 | tauri bundler (DMG) |
| PTY | portable-pty crate |
| Schema | serde + 读时校验（`schemas.rs`） |
| 自动更新 | tauri-plugin-updater |
| 图标 | lucide-react |
| i18n | 自定义 hook，CJK 自动检测 |

## 约定

- 图标使用 lucide-react，不引入其他图标库
- JSON 写入走 `.tmp` → `rename`，不做直接写入
- Schema 定义在 `src-tauri/src/buddy/schemas.rs`，读取时校验
- API key 在事件写入前自动脱敏（`redact.rs`）
- UI 文本通过 `useI18n` hook 国际化
- command 命名契约：`buddy:xxx` ↔ `buddy_xxx`，与 `tauri-bridge.ts` 逐一对应

## License

MIT
