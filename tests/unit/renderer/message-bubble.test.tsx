import { renderToStaticMarkup } from 'react-dom/server'
import { describe, expect, it } from 'vitest'
import { MessageBubble } from '../../../src/components/MessageBubble'
import type { TranscriptEntry } from '../../../src/shared/types'

function transcriptEntry(role: TranscriptEntry['role'], content = 'Short message'): TranscriptEntry {
  return {
    role,
    content,
    ts: '2026-05-26T07:30:00.000Z'
  }
}

describe('MessageBubble layout', () => {
  it('gives human message cards a minimum width of two thirds', () => {
    const html = renderToStaticMarkup(
      <MessageBubble entry={transcriptEntry('human')} />
    )

    expect(html).toContain('justify-end')
    expect(html).toContain('min-w-[66.666667%] max-w-[82%]')
  })

  it('keeps agent message cards full width', () => {
    const html = renderToStaticMarkup(
      <MessageBubble entry={transcriptEntry('codex')} />
    )

    expect(html).toContain('justify-start')
    expect(html).toContain('w-full')
    expect(html).not.toContain('min-w-[66.666667%]')
  })

  it('renders system messages as cards with the msg-system class', () => {
    const html = renderToStaticMarkup(
      <MessageBubble entry={transcriptEntry('system', 'codex run failed')} />
    )

    expect(html).toContain('msg-system')
    expect(html).toContain('w-full')
    expect(html).not.toContain('justify-center')
    expect(html).not.toContain('rounded-full')
  })
})
