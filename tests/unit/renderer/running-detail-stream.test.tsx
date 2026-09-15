import { renderToStaticMarkup } from 'react-dom/server'
import { describe, expect, it, vi } from 'vitest'
import { RunningDetailPanel } from '../../../src/components/RunningStatusMessage'
import { appendActorStreamLine, type ActorStreamLine } from '../../../src/lib/actor-stream'

// Port of upstream tests/unit/renderer/running-detail-stream.test.tsx. The
// upstream test feeds events through the TS main-process parser
// (src/main/buddy/parsers.ts); in this repo the parser lives in Rust
// (src-tauri/src/buddy/parsers.rs, with its own unit tests), so this helper
// mirrors its output contract for the event shapes used below.
vi.mock('../../../src/hooks/useI18n', () => ({
  useT: () => (key: string) => key,
  useLanguage: () => 'zh-CN'
}))

interface ParsedLike {
  text?: string
  streamMode?: 'delta' | 'line'
}

function parseCursorStreamLine(json: string): ParsedLike {
  const event = JSON.parse(json)
  if (event.type === 'assistant') {
    // Live deltas carry timestamp_ms and no model_call_id; the final flush
    // copy (no timestamp_ms) is dropped.
    if (event.timestamp_ms == null || event.model_call_id != null) return {}
    const content = event.message?.content
    const text = Array.isArray(content)
      ? content.map((part: { type?: string; text?: string }) => (part?.type === 'text' ? part.text ?? '' : '')).filter(Boolean).join('')
      : ''
    return { text: text || undefined, streamMode: 'delta' }
  }
  if (event.type === 'tool_call') {
    const toolCall = event.tool_call ?? {}
    for (const [key, value] of Object.entries(toolCall)) {
      const name = key.replace(/ToolCall$/, '') || key
      const args = (value as { args?: Record<string, unknown> })?.args ?? (value as Record<string, unknown>)
      const path = (args?.path ?? args?.file_path ?? args?.command) as string | undefined
      const label = path ? `${name} ${path}` : name
      return { text: `🔧 ${event.subtype ? `${label} (${event.subtype})` : label}`, streamMode: 'line' }
    }
    return { text: '🔧 tool', streamMode: 'line' }
  }
  if (event.type === 'connection' || event.type === 'retry') {
    return { text: `⏳ ${event.subtype ?? event.type}`, streamMode: 'line' }
  }
  return {}
}

function linesFromCursorEvents(events: unknown[]): ActorStreamLine[] {
  let lines: ActorStreamLine[] = []
  for (const event of events) {
    const parsed = parseCursorStreamLine(JSON.stringify(event))
    if (!parsed.text) continue
    lines = appendActorStreamLine(lines, {
      text: parsed.text,
      ts: String(lines.length + 1),
      mode: parsed.streamMode === 'delta' ? 'delta' : 'line'
    })
  }
  return lines
}

describe('RunningDetailPanel cursor stream display', () => {
  it('renders repeated Chinese deltas, whitespace, and tool-separated paragraphs as expected', () => {
    const streamLines = linesFromCursorEvents([
      { type: 'assistant', timestamp_ms: 1, message: { content: [{ type: 'text', text: '哈' }] } },
      { type: 'assistant', timestamp_ms: 2, message: { content: [{ type: 'text', text: '哈' }] } },
      { type: 'assistant', timestamp_ms: 3, message: { content: [{ type: 'text', text: '\n下一行 有空格' }] } },
      {
        type: 'tool_call',
        subtype: 'started',
        tool_call: { readToolCall: { args: { path: 'src/a.ts' } } }
      },
      { type: 'assistant', timestamp_ms: 4, message: { content: [{ type: 'text', text: '后文继续' }] } },
      { type: 'assistant', message: { content: [{ type: 'text', text: '哈哈\n下一行 有空格后文继续' }] } },
      { type: 'connection', subtype: 'reconnecting' }
    ])

    expect(streamLines.map((line) => line.text)).toEqual([
      '哈哈\n下一行 有空格',
      '🔧 read src/a.ts (started)',
      '后文继续',
      '⏳ reconnecting'
    ])

    const html = renderToStaticMarkup(
      <RunningDetailPanel actor="cursor" streamLines={streamLines} />
    )

    expect(html).toContain('running-detail-line')
    expect(html).toContain('哈哈\n下一行 有空格')
    expect(html).toContain('🔧 read src/a.ts (started)')
    expect(html).toContain('后文继续')
    expect(html).toContain('⏳ reconnecting')
    // Final flush duplicate must not appear as its own row.
    expect(html.match(/哈哈/g)?.length).toBe(1)
  })
})
