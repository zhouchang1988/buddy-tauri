# 运行详情展示方案

## 需求

1. 不再为显示运行详情 card 分别 resume 两个 actor 的会话
2. 复用现有数据，不新增存储
3. 会话详情展示效果与现在保持不变

## 现有方案

### 两层显示架构

| 层级 | 时机 | 数据源 | 组件 |
|------|------|--------|------|
| 实时流 | Actor 运行中 | `EventBus` → `useActorStream()` | `RunningDetailPanel` |
| 成绩单 | Actor 完成后 | `transcript.jsonl` | `MessageBubble` |

### 进程模型

所有 actor CLI 以 `spawn()` + pipe stdio 启动，stdin 在 prompt 传入后立即 `end()`，进程以 fire-and-forget 模型运行至退出。Resume 通过 `--resume`/`--session` 标志启动**新进程**，传入已有 session ID。

### 数据资产

每轮运行产生的文件：

```
tasks/<id>/
├── transcript.jsonl                          ← 已展示
├── events.jsonl                              ← Buddy 生命周期事件
├── artifacts/
│   ├── <runId>-prompt.md                     ← 发给 actor 的 prompt
│   ├── <runId>-events.jsonl                  ← stream-json 原始事件
│   └── <runId>-output.md                     ← 提取的纯文本输出
```

### events.jsonl 写入时机

**关键**：`artifacts/<runId>-events.jsonl` 在 native launcher 模式下是进程退出后一次性写入（`collectRawEvents()` → `writeFile()`），运行期间文件不存在。

| Launcher 类型 | 写入时机 | 写入者 |
|--------------|---------|--------|
| native (Claude/Codex/OpenCode/Kimi) | 进程退出后 | buddy runner |
| contract | actor 自行写入（`BUDDY_EVENT_FILE`） | actor CLI |

## 问题

### 1. OpenCode 运行中详情面板为空

**根因**：`parseOpenCodeJsonLine()` 只处理 `type: "text"` 和 `type: "error"`，忽略了 `step_start`、`tool_use`、`step_finish` 等事件。

OpenCode `--format json` 实际输出的事件类型：

| 事件类型 | 内容 | 当前是否处理 |
|---------|------|-------------|
| `step_start` | 步骤开始 | ❌ 忽略 |
| `tool_use` | 工具调用（含 tool name、input、output） | ❌ 忽略 |
| `text` | 文本输出 | ✅ 处理 |
| `step_finish` | 步骤结束（含 token 用量） | ❌ 忽略 |

运行期间，`onStdout` 回调调用 `parseActorLine('opencode', line)`，非 text/error 事件返回 `text: undefined`，不发布 `actor.stdout` 事件。因此 `useActorStream()` 收不到任何数据，`RunningDetailPanel` 始终为空。

**修复**：扩展 `parseOpenCodeJsonLine()` 对非 text 事件生成占位文本：

- `step_start` → `"..."` 或 `"开始执行"`
- `tool_use` → `"🔧 {tool name}"` 或 `"🔧 {tool name}: {input 摘要}"`
- `step_finish` → 不生成文本（静默）

**注意**：OpenCode 无流式输出选项（只有 `--format default/json`），即使修复解析器，运行期间也只会在 `step_start` 时收到 1 条占位文本，之后直到结束才有 `text` 事件。这是上游 CLI 的设计限制。

文件：`src/main/buddy/parsers.ts`

### 2. 运行后详情只显示 In/Out token，无工具调用列表

**现象**：某些轮次展开详情只显示 "In: 79,643 / Out: 56"，没有工具调用信息。

**根因**：这不是 bug，是该轮次确实没有 tool_use 事件。例如 break 确认轮次（仅 thinking + 短文本 + result），没有调用任何工具，`getRoundEvents()` 返回 `toolUses: []`，只剩 token 用量。

有 tool_use 的轮次（如编辑代码的轮次）会正常显示工具列表。这是正确行为，但 UX 可以优化——当没有工具调用时，可以用不同的展示方式替代空白。

**优化**：当 `toolUses` 为空时，显示"本轮无工具调用"或直接隐藏详情按钮。

### 3. 运行中详情面板体验不足

- 展开按钮（14px ChevronDown）不够醒目，用户可能不知道可以展开
- 轮次切换时展开状态被重置（`ChatArea.tsx:46-48` 在 `!isRunning` 时强制折叠）
- 展开时不保证滚到最新内容

### 3. 运行后无法回看详情

`useActorStream()` 的数据是纯内存瞬态的，actor 完成后数据丢失。`events.jsonl` 已持久化但前端未消费。

### 4. Resume session 只能继续对话，不能查看历史

每次 resume 会启动新 CLI 进程（3-8s 冷启动），且看到的是完整会话历史而非单轮详情，还可能影响后续轮次。

## 已完成

### ~~P0：修复 OpenCode 实时流显示~~ ✅ 已实现

`parseOpenCodeJsonLine()` 已扩展支持 `step_start` → `"..."` 和 `tool_use` → `"🔧 {toolName}"`（`parsers.ts:59-64`）。

### ~~P1：运行中详情面板体验优化~~ ✅ 已实现

- 整个 `running-status-body` 区域可点击展开（`RunningStatusMessage.tsx:130`）
- `RunningDetailPanel` 挂载后立即滚到底（`RunningStatusMessage.tsx:156-159`）
- 轮次切换时仅终态折叠，COUNTDOWN/READY 间隙保持展开（`ChatArea.tsx:127`）
- 空 stream 时显示上轮最后消息（`ChatArea.tsx:115-123`，`RunningDetailPanel` 的 `lastMessage` prop）

### ~~P2：运行后完整详情展示~~ ✅ 大部分已实现

**已存在的完整管道**（6 层全部就绪）：

| 层级 | 文件 | 状态 |
|------|------|------|
| 类型定义 | `shared/types.ts:237-258` `RoundEventEntry` + `RoundEventSummary` | ✅ 含 thinking/text/tool_use/tool_result/result |
| Store 解析 | `store.ts:308-415` `getRoundEvents()` | ✅ 解析 Claude/Codex/Kimi/OpenCode 四种格式 |
| IPC handler | `buddy-handlers.ts` `buddy:getRoundEvents` | ✅ |
| Preload 桥接 | `preload/buddy-api.ts` `getRoundEvents()` | ✅ |
| Renderer API | `renderer/lib/api.ts` `getRoundEvents()` | ✅ |
| React Hook | `hooks/useBuddy.ts` `useRoundEvents()` | ✅ |
| UI 组件 | `MessageBubble.tsx:265-367` `RoundEvents` + `RoundEventItem` | ✅ 完整时间线渲染 |

**但管道不工作**——因为 `getRoundEvents()` 的 JSONL 解析器无法处理含换行符的 JSON，导致 OpenCode/Codex/Kimi 的 events.jsonl 大量行被静默跳过，返回空数据。

## 当前唯一阻塞项：JSONL 多行 JSON 解析

### 问题

`getRoundEvents()`（`store.ts:321`）使用 `raw.split(/\r?\n/)` + 逐行 `JSON.parse(line)` 解析 events.jsonl。

OpenCode/Codex 的 `--format json` 输出中，tool_use 事件的 `input` 字段可能包含未转义的换行符（如文件内容），导致一条 JSON 记录被 `\n` 分割成多行。`JSON.parse()` 在碎片行上失败，`catch { /* skip */ }` 静默丢弃。

同样的问题也存在于 `parseJsonEvents()`（`parsers.ts:324-334`），影响 `extractActorOutput()` 等函数。

### 修复：缓冲式 JSON 解析器

替换 `raw.split(/\r?\n/)` + 逐行 `JSON.parse()` 为缓冲式解析器：累加行直到形成合法 JSON 对象。

```typescript
function parseJsonlBuffer(raw: string): Record<string, unknown>[] {
  const results: Record<string, unknown>[] = []
  let buffer = ''

  for (const line of raw.split(/\r?\n/)) {
    if (!line.trim()) continue
    buffer = buffer ? buffer + '\n' + line : line
    try {
      const obj = JSON.parse(buffer)
      if (obj && typeof obj === 'object' && !Array.isArray(obj)) {
        results.push(obj)
      }
      buffer = ''
    } catch {
      // 不清空 buffer，继续累加下一行
    }
  }

  // 残余 buffer 尝试一次最终解析
  if (buffer.trim()) {
    try {
      const obj = JSON.parse(buffer)
      if (obj && typeof obj === 'object' && !Array.isArray(obj)) {
        results.push(obj)
      }
    } catch { /* 丢弃 */ }
  }

  return results
}
```

### 改动文件

| 文件 | 改动 | 说明 |
|------|------|------|
| `src/main/buddy/parsers.ts` | 新增 `parseJsonlBuffer()`，替换 `parseJsonEvents()` 的逐行解析 | 统一修复 |
| `src/main/buddy/store.ts` | `getRoundEvents()` 使用 `parseJsonlBuffer(raw)` 替代 `raw.split()` + 逐行 `JSON.parse()` | 修复 OpenCode/Codex/Kimi 详情为空 |

约 30 行改动，零架构变更。

### 改动总览

| 优先级 | 状态 | 说明 |
|--------|------|------|
| P0 实时流 | ✅ 已实现 | `parseOpenCodeJsonLine()` 支持 step_start/tool_use |
| P1 面板体验 | ✅ 已实现 | 可点击展开、保持展开、空 stream 回退 |
| P2 完整详情 | ✅ 已实现 | `parseJsonlBuffer()` 缓冲式解析器修复多行 JSON 解析，6 层管道完整 |
| P3 空 stream 回退 | ✅ 已实现 | `lastMessage` prop |

**全部实现完成。零架构变更，零存储变更，不需要 resume session ID。**
