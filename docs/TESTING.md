# 测试策略

Buddy（Tauri 版）采用「Rust 单测 + 前端单测 + 手工冒烟」的三层测试策略。原 Electron 版的 Playwright E2E 未移植，原因与替代路径见文末。

## 一、Rust 单元测试（257 个）

```bash
pnpm test:rust
# 等价于
cargo test --manifest-path src-tauri/Cargo.toml
```

按模块过滤：

```bash
cargo test --manifest-path src-tauri/Cargo.toml store     # 只跑 store 模块
```

各模块覆盖内容（257 个测试的分布）：

| 模块 | 测试数 | 覆盖内容 |
|------|-------:|----------|
| `buddy::runner` | 53 | 任务状态机、轮次调度、失败重试/恢复、倒计时跳过/暂停、打断与退出错误处理、健康检查、compact 设置与重试 |
| `buddy::parsers` | 42 | 5 种 actor CLI 流式输出解析（Claude / Codex / Cursor / OpenCode / Kimi），含转义、截断、畸形行容错 |
| `buddy::model_detect` | 33 | 从各 actor 配置文件与 CLI 参数检测实际生效模型，含 WeCode 命令识别与 `-m`/`--model` 优先规则 |
| `buddy::queue_coordinator` | 27 | 指令队列的入队/出队/清空/插队、reconcile 合并、queue.blocked 去重 |
| `buddy::launchers` | 17 | 原生/契约启动器判定、命令拆分、session 复用参数拼接、能力降级 |
| `buddy::git` | 15 | status/diff/stage/commit/push、分支列表与切换、分支名合法性校验 |
| `buddy::store` | 13 | 原子写（`.tmp` → rename）、events/transcript JSONL 读写、读时校验、`get_round_events`、`get_task_stats` |
| `buddy::service` | 11 | `BuddyCoreService` 门面：bootstrap、任务 CRUD、设置更新、事件转发 |
| `buddy::prompts` | 9 | 双 actor prompt 构建（与 Electron 版逐字节一致）、语言检测 |
| `buddy::schemas` | 8 | 读时 schema 校验与默认值补齐、前向兼容（未知字段容忍） |
| `buddy::notifications` | 8 | 任务完成/失败/暂停通知的 payload 构造 |
| `buddy::session_insight` | 7 | 从 kimi `wire.jsonl` / opencode 会话存储归集模型与 token 用量 |
| `buddy::redact` | 4 | 事件写入前的 API key 脱敏 |
| `commands::tests` | 4 | command 层参数序列化契约 |
| `menu::tests` | 3 | 菜单语言切换与事件名 |
| `buddy::paths` | 2 | workspace key = slug + sha256(path) 前 12 位，与 Electron 版一致 |
| `updater::tests` | 1 | `updater:event` payload 形状与前端 `useUpdater` 对齐 |

## 二、前端单元测试（96 个，vitest）

```bash
pnpm test
# 等价于
vitest run tests/unit
```

12 个测试文件：

- `tests/unit/bridge/tauri-bridge.test.ts` — 桥接层契约：mock `@tauri-apps/api`，验证每个 `window.buddy` / `window.api` 方法调用正确的 command 名与 camelCase 参数对象，事件订阅/退订行为
- `tests/unit/renderer/`（10 个文件）— 从 Electron 版原样移植的组件与工具测试：sidebar、chat-area、message-bubble、task-list、status-bar、title-bar、keyboard、markdown（含 edge cases）、api
- `tests/unit/format.test.ts` — 格式化工具

前端改动需同时保证 `pnpm typecheck` 干净。

## 三、手工冒烟清单

单测覆盖不到的端到端链路，发版前在 `pnpm tauri:dev` 下人工过一遍：

1. **创建任务**：选择本地 Git 仓库 → 新建任务 → 填写任务说明 → 出现在任务列表
2. **双 actor 轮次**：启动任务 → 执行方跑完自动切审查方 → 双方 `type=break` 后任务转 DONE → 完成统计表（模型/Token/耗时/轮次）正确
3. **指令队列**：Actor 运行期间发送指令 → 进入队列 → 当前轮次结束后自动执行；出队、清空、插队各操作一次
4. **Git 操作**：查看变更文件 diff → 按文件勾选暂存 → 生成 conventional commit 消息（可随时打断）→ 提交并推送；分支列表、切换、从 HEAD 新建分支
5. **主题/语言切换**：切换几套主题（含暗色/亮色）→ 切换简中/繁中/英文 → 原生菜单语言同步更新
6. **崩溃恢复**：任务运行中 `kill` 掉应用 → 重启 → 原 `RUNNING_*` 任务自动重置为 `PAUSED` → 可继续运行
7. **旧数据目录兼容**：将 Electron 版的 `~/Library/Application Support/buddy/` 数据原样指向本版 → 任务列表/详情/续跑正常，JSON 无字段丢失

## 四、E2E 现状与未来路径

原 Electron 版的 Playwright E2E（`tests/e2e/`）**未移植**：Playwright 的 Electron driver 无法驱动 Tauri 应用。

如需端到端自动化，Tauri 官方路径是 **tauri-driver + WebDriver**（macOS 上配合 `WKWebView` 的 safaridriver 方案尚不成熟，官方 E2E 支持以 Linux/Windows 为主），这也是暂未移植的原因之一。当前策略是：

- Rust 单测覆盖后端全部核心逻辑（状态机、解析、队列、Git、持久化）
- 前端单测覆盖桥接契约与组件行为
- 发版前执行上面的手工冒烟清单

后续若引入 tauri-driver，建议从冒烟清单的第 1、2、6 项（创建任务、双 actor 轮次、崩溃恢复）开始自动化，actor CLI 用 mock command 替代以保持稳定。
