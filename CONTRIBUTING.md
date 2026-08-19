# 贡献指南

感谢你对 Buddy 的关注！本文档介绍如何参与项目开发。

> ⚠️ **请注意：本仓库是研究性实验项目**——为研究「用 AI 编程切换开发方案」（Electron → Tauri 2 移植）而产生，不是一个面向生产环境的稳定项目。如果你希望更好地体验和使用 Buddy，请使用原项目 [davidhoo/buddy](https://github.com/davidhoo/buddy)（官网：https://davidhoo.github.io/buddy/）。

## 开发环境

### 前置要求

- macOS 12+ (Monterey)
- Node.js >= 18
- pnpm 11 (`corepack enable && corepack prepare pnpm@11 --activate`)
- Rust toolchain（`rustup`，稳定版）
- 至少一个已安装的 AI CLI 工具（用于手动冒烟测试）：Claude Code / Codex / Cursor CLI / OpenCode / Kimi Code

### 快速开始

```bash
pnpm install          # 安装依赖
pnpm tauri:dev        # 完整开发模式（Rust 后端 + 前端 HMR）
```

### 常用命令

| 命令 | 说明 |
|------|------|
| `pnpm dev` | 前端开发模式（仅 vite HMR，无后端） |
| `pnpm tauri:dev` | 完整开发模式（Rust 后端 + 前端 HMR） |
| `pnpm test` | 前端单元测试（vitest run tests/unit） |
| `pnpm test:rust` | Rust 单元测试（cargo test --manifest-path src-tauri/Cargo.toml） |
| `pnpm typecheck` | TypeScript 类型检查 |
| `pnpm tauri build` | 构建应用包 |
| `pnpm dist` | 构建 + DMG（tauri build 的别名） |

运行单个测试文件：

```bash
pnpm vitest run tests/unit/renderer/sidebar.test.tsx          # 前端
cargo test --manifest-path src-tauri/Cargo.toml store          # Rust（按模块名过滤）
```

## 项目结构

```
src/                   # 前端（渲染层，复用自 Electron 版）
├── components/        # React 组件
├── hooks/             # 自定义 hooks
├── lib/
│   └── tauri-bridge.ts # 桥接层：window.buddy / window.api → invoke/listen
├── shared/            # 前后端共享类型与默认值
└── main.tsx           # 入口（首行 import tauri-bridge）

src-tauri/
├── src/
│   ├── buddy/         # 核心逻辑：service, store, runner, queue_coordinator,
│   │                  #   launchers, parsers, prompts, git, schemas, ...
│   ├── commands.rs    # 40 个 #[tauri::command] 处理器
│   ├── menu.rs        # 原生菜单
│   ├── updater.rs     # 自动更新（tauri-plugin-updater）
│   └── lib.rs         # 接线：插件 → 服务 → 事件转发 → 恢复 → command 注册
├── capabilities/      # Tauri 权限声明
└── tauri.conf.json    # 窗口 / CSP / 打包 / updater 配置

tests/
└── unit/              # 前端单元测试（vitest）
    ├── bridge/
    └── renderer/
```

## 代码规范

### Commit 消息

使用 Conventional Commits 格式，支持中文或英文：

```
type(scope): 简短描述
```

**类型**：`feat`、`fix`、`refactor`、`style`、`chore`、`docs`、`test`、`ci`

**范围**（可选）：`core`、`ui`、`git`、`runner`、`updater`、`ci`、`dialog`

示例：

```
feat(ui): 添加任务未读状态指示
fix(core): 修复 macOS PATH 问题
refactor(runner): 移除倒计时机制
ci: 添加 GitHub Actions 流水线配置
```

### TypeScript

- `strict: true`，启用所有严格检查
- 前端使用 `@/` 别名映射到 `src/`
- Rust 端共享类型在 `src-tauri/src/buddy/types.rs`，与 `src/shared/types.ts` 逐字段对应，改动必须两边同步

### 关键约定

- **图标**：仅使用 lucide-react，不引入其他图标库或自定义 SVG
- **JSON 写入**：必须走 `.tmp` → `rename` 原子写入，不做直接写入
- **Schema 校验**：定义在 `src-tauri/src/buddy/schemas.rs`，读取时校验（前向兼容）
- **敏感数据**：API key 在事件写入前由 `redact.rs` 自动脱敏
- **国际化**：UI 文本通过 `useI18n` hook 处理，不硬编码字符串
- **Command 命名**：`buddy:xxx` ↔ snake_case `buddy_xxx`；新增 command 需同时改 `commands.rs`、`lib.rs` 注册表与 `src/lib/tauri-bridge.ts`
- **注释**：仅注释「为什么」，不注释「做什么」

## 提交 PR

1. Fork 仓库并从 `main` 创建功能分支
2. 确保通过类型检查和测试：`pnpm typecheck && pnpm test && pnpm test:rust`
3. 按照规范编写 commit 消息
4. 提交 Pull Request，描述变更内容和动机

### PR 检查清单

- [ ] `pnpm typecheck` 通过
- [ ] `pnpm test` 通过
- [ ] `pnpm test:rust` 通过
- [ ] 新功能有对应测试（Rust 端 + 前端）
- [ ] UI 变更已在 `pnpm tauri:dev` 下验证
- [ ] 国际化文本已提取（不硬编码中英文）
- [ ] 图标使用 lucide-react

## 架构要点

### Tauri 双端模型

- **Rust 后端**（`src-tauri/`）：文件系统访问、子进程管理（portable-pty）、原生菜单、自动更新
- **桥接层**（`src/lib/tauri-bridge.ts`）：替代 Electron preload，在 `window` 上暴露 `window.api`（系统操作）和 `window.buddy`（核心业务），内部走 `invoke`/`listen`
- **前端**（`src/`）：系统 Webview 环境，React UI，与 Electron 版渲染代码一致

### IPC 通信

- 请求-响应：`invoke('buddy_xxx')` ↔ `#[tauri::command] buddy_xxx`，参数以单个 camelCase 对象传递
- 推送事件：Rust 端 `app.emit('buddy:event', ...)` → 前端 `listen('buddy:event')`，由 `BuddyEventBus`（tokio broadcast）转发

### 数据持久化

纯文件系统（`~/Library/Application Support/buddy/`），无数据库，与 Electron 版逐字节兼容。所有 JSON 写入为原子操作，schema 在读取时校验而非写入时，保证前向兼容。

## 问题反馈

在仓库 Issues 中提交，请包含：

- macOS 版本
- Buddy 版本
- 复现步骤
- 预期行为与实际行为
