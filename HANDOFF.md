# HANDOFF — Buddy Tauri 版重构交接文档

> 目标：根据 `../buddy/`（Electron 版）在当前目录重构一个 **Tauri 版本**，功能与原版完全相同，文档、代码、测试齐全。
> 本文件记录最终状态（2026-08-06）。**所有移植工作已完成**，仅剩 DMG 重打包与运行时冒烟验证。

---

## 1. 总体架构（已实现）

| 层 | 原版 (Electron) | Tauri 版（本项目） |
|---|---|---|
| 后端 | Node.js 主进程 (`src/main/`, ~4.4k 行 TS) | **Rust** (`src-tauri/src/`，19 个 buddy 模块 + commands/menu/updater） |
| 桥接 | preload + contextBridge (`window.buddy` / `window.api`) | `src/lib/tauri-bridge.ts`：import 副作用挂同名同形对象，内部走 `@tauri-apps/api` |
| 前端 | React 18 + TanStack Query 5 + Tailwind | 原样复用（仅修导入路径） |
| IPC | `buddy:xxx` channel | Tauri command `buddy_xxx`（40 个，与 bridge 逐一核对一致）；事件名不变（`buddy:event` / `menu:action` / `updater:event` / `window:fullScreenChange`） |
| 数据目录 | `~/Library/Application Support/buddy/` | 完全兼容，JSON 逐字节一致 |
| PTY (opencode) | node-pty | `portable-pty` |
| 更新器 | electron-updater | tauri-plugin-updater（密钥对已生成于 `~/.tauri/buddy-tauri.key`，pubkey 已写入 tauri.conf.json；endpoint 指向 zhouchang1988/buddy-tauri） |

## 2. 当前状态：全部验证项

- `pnpm install` ✅（`pnpm-workspace.yaml` 的 `allowBuilds: esbuild: true`）
- `pnpm test`（vitest）✅ **96/96**
- `pnpm typecheck` ✅
- `pnpm vite build` ✅
- `cargo test` ✅ **257/257**（连续 3 次全量确定绿色；git.rs 的测试竞态已修）
- `cargo check --all-targets` ✅ 零警告
- `cargo build`（lib + binary）✅
- `pnpm tauri build` ✅ 产出 Buddy.app + Buddy_1.2.9_aarch64.dmg（首次 DMG bundling 偶发失败，重跑成功；`devtools` feature 已加）
- 运行时冒烟 ✅（启动 12s 无 panic，干净退出）

## 3. 模块清单（src-tauri/src/buddy/，全部移植完成）

types / defaults / paths / locks / events / redact / shell_path / schemas / store / parsers / prompts / session_insight / model_detect / launchers / queue_coordinator / runner / git / notifications / service — 每个模块都有对应 Rust 单测，覆盖原版 `tests/unit/main/` 的全部用例。
另有 `commands.rs`（40 个 `#[tauri::command]`）、`menu.rs`（三语菜单 + `menu:action` 转发）、`updater.rs`（`updater:event` 载荷与前端 useUpdater 匹配）、`lib.rs`（完整接线）。

## 4. 集成期主 agent 自查修复（超出各 wave 范围的遗漏）

1. `window:fullScreenChange` 事件原本无人发射 → lib.rs 中用 `on_window_event(Resized)` + `is_fullscreen()` 轮询 + AtomicBool 去重补上等效逻辑
2. 窗口 `visible:false` 但无人 show → lib.rs 加 `.on_page_load(|w, _| w.show())`（对齐原版 ready-to-show）
3. `open_devtools` release 编译失败 → Cargo.toml 加 `devtools` feature
4. bridge `selectDirectory` 用了不存在的 `createDirectory` 选项 → 改为 `canCreateDirectories`（plugin-dialog v2 正确名称）
5. updater 占位 pubkey → 已生成真实密钥对并写入配置（私钥 `~/.tauri/buddy-tauri.key` 不入库）
6. TCC 反复弹「想访问文稿文件夹」→ 根因是 ad-hoc 签名每次构建哈希都变；已创建自签名证书 `Buddy Local Dev`（存于独立钥匙串 `~/Library/Keychains/buddy-dev.keychain-db`），`tauri.conf.json` 配置 `signingIdentity` 自动签名，手动兜底 `sh scripts/sign-local.sh`。完整方案与重建步骤见 docs/LOCAL_SIGNING.md
7. **opencode 轮次结束后卡死（AppleScript 提示）**→ 根因：用户全局 opencode 配置挂了 `oh-my-openagent` 插件，其 session-notification 钩子在每次运行结束时调用 `osascript -e 'display notification ...'` 并 await；GUI 父进程没有自动化授权时该调用阻塞在授权弹窗上 → opencode 永不退出 → runner 永远等不到 actor.completed。修复（`runner.rs`）：为 PTY 子进程在 PATH 前挂 `<data_root>/shims/osascript`  shim——吞掉 `display notification`（Buddy 自有通知系统，无功能损失），其余参数透传真 osascript。`ensure_osascript_shim_dir` + `prepend_path`，应用于 `run_actor_command` 与 summarize 两个 PTY 调用点。该问题在 Electron 原版同样存在（2026-07 数据可查），此修复使 Tauri 版行为优于原版

## 5. 收尾工作（全部完成 ✅）

1. ✅ `pnpm tauri build` 成功，产出 `src-tauri/target/release/bundle/macos/Buddy.app` 与 `bundle/dmg/Buddy_1.2.9_aarch64.dmg`
2. ✅ 运行时验证：debug 包 + 临时探针确认窗口创建（1 个）、React 挂载（root=1）、bridge 就绪（window.buddy/window.api 为 object）、`buddy_bootstrap` IPC 成功调用、无 JS 错误；探针已全部移除并重跑 257+96 测试全绿
3. 详细手测清单见 docs/TESTING.md（双 actor 轮次、指令队列、git 操作、主题/语言切换等需在真实使用中逐项过一遍）
4. （可选）tauri-driver E2E —— 原版 Playwright E2E 未移植，理由与路径已写入 docs/TESTING.md

### 5.1 重大教训：窗口空白/不显示（2026-08-06 修复）

首次交付后用户报告「打开完全没界面」。根因两个叠加：

1. **`index.html` 里的 `<meta http-equiv="Content-Security-Policy">` 封杀了 Tauri 注入的初始化脚本**（meta CSP 不会自动带上 Tauri 内部脚本的 hash，只有 `tauri.conf.json` 的 `app.security.csp` 会被 Tauri 自动合并 hash）。`script-src 'self'` 拦截 → `window.__TAURI_INTERNALS__` 未初始化 → tauri-bridge 在 import 时抛错 → React 整树未挂载 → 白屏。**规则：CSP 只写 `tauri.conf.json`，绝不写 meta 标签。**
2. **`"visible": false` + 依赖 `on_page_load` 回调 show**：页面加载链路断了之后窗口永远隐藏，进程活着但一个窗口都没有（System Events 查 `count of windows = 0`）。已改为 `visible: true`（创建即显示，失败时至少能看到窗框），`on_page_load` show 保留为冗余。

验证方法（无头环境可查窗口与前端启动）：`osascript -e 'tell application "System Events" to tell process "Buddy" to get {count of windows, title of every window}'`；前端启动探针 = 临时 `#[tauri::command] diag_log` + `window.eval` 里用 `__TAURI_INTERNALS__.invoke`（**不是** `__TAURI__`，后者需要 `withGlobalTauri`）回报挂载状态 + 在 `buddy_bootstrap` 里临时 `eprintln!`。注意：`document.title` 改动**不会**同步到原生窗口标题，不能当探针信道。

## 6. 已知偏差（全部记录在案，均不影响功能）

- PTY 路径（portable-pty）无法上报终止信号，被信号杀掉的 opencode 报原始 exit code
- `interrupt` 不杀孤立子进程（与原版行为一致——原版也不杀）
- runner 用循环替代 TS 的递归自动轮次推进（避免栈溢出，语义相同）
- locale 检测经 `defaults read AppleLocale`（`en_US` vs `en-US`，仅外观差异）
- `GlobalSettings.max_rounds` 为 `Option<u32>`，TS 的 `-1` 哨兵不可达（行为等价）
- 截断按 Unicode scalar 而非 UTF-16 code unit（仅影响 astral 字符边界）
