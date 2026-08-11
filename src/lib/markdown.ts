import { marked, Tokenizer } from 'marked'
import DOMPurify from 'dompurify'

marked.setOptions({ breaks: true, gfm: true })

// marked's GFM autolink only backpedals trailing ASCII punctuation, so CJK
// punctuation (，。（） etc.) right after a bare URL gets swallowed into the
// link. Such characters can never appear unencoded in a URL, so trim the
// match at the first non-ASCII-printable char, then re-run the default
// tokenizer on the trimmed text to keep escaping / www. handling intact.
const defaultUrl = Tokenizer.prototype.url
const NON_URL_CHAR = /[^\x21-\x7e]/

marked.use({
  tokenizer: {
    url(src: string) {
      const token = defaultUrl.call(this, src)
      if (!token) return undefined
      if (!token.href.startsWith('mailto:')) {
        const cut = token.raw.search(NON_URL_CHAR)
        if (cut > 0) return defaultUrl.call(this, token.raw.slice(0, cut))
      }
      return token
    },
  },
})

// Transcript messages are re-rendered on every keystroke / poll / stream
// chunk, and marked.parse + DOMPurify.sanitize is by far the most expensive
// part of that path. Message contents are immutable once written, so cache
// rendered HTML per input text (LRU, bounded).
const RENDER_CACHE_LIMIT = 500
const renderCache = new Map<string, string>()

export function renderMarkdown(text: string): string {
  const cached = renderCache.get(text)
  if (cached !== undefined) {
    // Refresh recency (Map iteration order = insertion order).
    renderCache.delete(text)
    renderCache.set(text, cached)
    return cached
  }
  let html: string
  try {
    const raw = marked.parse(text, { async: false }) as string
    html = DOMPurify.sanitize(raw, { ALLOWED_URI_REGEXP: /^(?:(?:https?|mailto):)/i })
  } catch {
    html = escapeHtml(text)
  }
  if (renderCache.size >= RENDER_CACHE_LIMIT) {
    const oldest = renderCache.keys().next().value
    if (oldest !== undefined) renderCache.delete(oldest)
  }
  renderCache.set(text, html)
  return html
}

function escapeHtml(s: string): string {
  return s
    .replace(/&/g, '&amp;')
    .replace(/</g, '&lt;')
    .replace(/>/g, '&gt;')
    .replace(/"/g, '&quot;')
    .replace(/'/g, '&#39;')
}
