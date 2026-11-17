# 测试策略

Buddy（Tauri 版）采用「Rust 单测 + 前端单测 + 手工冒烟」的三层测试策略。原 Electron 版的 Playwright E2E 未移植，原因与替代路径见文末。

## 一、Rust 单元测试（512 个）

```bash
pnpm test:rust
# 等价于
cargo test --manifest-path src-tauri/Cargo.toml
```

按模块过滤：

```bash
cargo test --manifest-path src-tauri/Cargo.toml store     # 只跑 store 模块
```

当前共 512 个测试（含 1 个平台相关忽略项），主要覆盖：

- 任务状态机、Actor 启动器与原生 NDJSON/ACP 解析、失败重试/恢复、健康检查、compact 设置与重试
- 指令队列、Git 操作、原子持久化、读时 schema、事件/统计、通知和脱敏
- dsh 参数与输出、CodeBuddy 启动、ACP 凭据共享和用量完整性
- Agent 版本比较、认证失败识别、模型目录解析、SHA-256 模型缓存
- Skills 导入/eject、MCP 的 JSONC/TOML/dsh 局部写回与格式转换
- `BuddyCoreService` 的 Agent 配置与资源库真实调用回归

按模块过滤时可直接使用测试名关键字，例如 `cargo test --manifest-path src-tauri/Cargo.toml library`。

## 二、前端单元测试（304 个，vitest）

```bash
pnpm test
# 等价于
vitest run tests/unit
```

27 个测试文件：

- `tests/unit/bridge/tauri-bridge.test.ts` — 桥接层契约：mock `@tauri-apps/api`，验证 `window.buddy` / `window.api` 的 command 名、camelCase 参数、Agent/Library API 与事件订阅/退订
- `tests/unit/renderer/`（25 个文件）— 组件、Composer/新建任务模型选择、Agent 更新、Skills/MCP、会话与主题相关行为
- `tests/unit/format.test.ts` — 格式化、模型标签与恢复命令工具

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
