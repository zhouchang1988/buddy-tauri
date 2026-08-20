// Port of the original tests/unit/preload/buddy-api.test.ts, adapted to the
// Tauri bridge: verifies that window.buddy / window.api map to snake_case
// Tauri commands and that event subscriptions forward payloads.

import { beforeEach, describe, expect, it, vi } from 'vitest'

const invokeMock = vi.fn()
const listenMock = vi.fn()
const openDialogMock = vi.fn()
const openUrlMock = vi.fn()

vi.mock('@tauri-apps/api/core', () => ({
  invoke: (...args: unknown[]) => invokeMock(...args)
}))

vi.mock('@tauri-apps/api/event', () => ({
  listen: (...args: unknown[]) => listenMock(...args)
}))

vi.mock('@tauri-apps/api/window', () => ({
  getCurrentWindow: () => ({
    isFullscreen: vi.fn().mockResolvedValue(false)
  })
}))

vi.mock('@tauri-apps/plugin-dialog', () => ({
  open: (...args: unknown[]) => openDialogMock(...args)
}))

vi.mock('@tauri-apps/plugin-opener', () => ({
  openUrl: (...args: unknown[]) => openUrlMock(...args),
  revealItemInDir: vi.fn()
}))

describe('tauri-bridge', () => {
  beforeEach(async () => {
    vi.resetModules()
    invokeMock.mockReset()
    listenMock.mockReset()
    openDialogMock.mockReset()
    openUrlMock.mockReset()
    window.localStorage.clear()
    await import('../../../src/lib/tauri-bridge')
  })

  it('exposes window.buddy and window.api', () => {
    expect(window.buddy).toBeDefined()
    expect(window.api).toBeDefined()
  })

  it('maps buddy methods to snake_case Tauri commands', async () => {
    invokeMock.mockResolvedValue({ version: 'native' })
    await expect(window.buddy.bootstrap()).resolves.toEqual({ version: 'native' })
    expect(invokeMock).toHaveBeenCalledWith('buddy_bootstrap')

    invokeMock.mockResolvedValue([])
    await window.buddy.getTasks()
    expect(invokeMock).toHaveBeenCalledWith('buddy_get_tasks')
  })

  it('passes positional args as a camelCase args object', async () => {
    invokeMock.mockResolvedValue({})
    await window.buddy.getTaskDetail('task-1', 'ws-1')
    expect(invokeMock).toHaveBeenCalledWith('buddy_get_task_detail', { taskId: 'task-1', workspaceKey: 'ws-1' })

    invokeMock.mockResolvedValue(undefined)
    await window.buddy.enqueueInstruction('task-1', 'ws-1', 'do it', [])
    expect(invokeMock).toHaveBeenCalledWith('buddy_enqueue_instruction', {
      taskId: 'task-1',
      workspaceKey: 'ws-1',
      content: 'do it',
      attachments: []
    })
  })

  it('maps system ops to their Tauri commands', async () => {
    invokeMock.mockResolvedValue([])
    await window.api.readClipboardFilePaths()
    expect(invokeMock).toHaveBeenCalledWith('read_clipboard_file_paths')

    invokeMock.mockResolvedValue('/tmp/file.png')
    await window.api.saveAttachmentBuffer('task-1', 'ws-1', 'file.png', 'aGVsbG8=')
    expect(invokeMock).toHaveBeenCalledWith('save_attachment_buffer', {
      taskId: 'task-1',
      workspaceKey: 'ws-1',
      name: 'file.png',
      bufferBase64: 'aGVsbG8='
    })

    invokeMock.mockResolvedValue('data:image/png;base64,aGVsbG8=')
    await window.api.readFileAsDataURL('/tmp/file.png', 'image/png')
    expect(invokeMock).toHaveBeenCalledWith('read_file_as_data_url', { filePath: '/tmp/file.png', mimeType: 'image/png' })

    invokeMock.mockResolvedValue(undefined)
    window.api.updateMenuLanguage('zh')
    expect(invokeMock).toHaveBeenCalledWith('update_menu_language', { lang: 'zh' })

    window.api.checkForUpdates()
    expect(invokeMock).toHaveBeenCalledWith('updater_check')
    window.api.downloadUpdate()
    expect(invokeMock).toHaveBeenCalledWith('updater_download')
    window.api.installUpdate()
    expect(invokeMock).toHaveBeenCalledWith('updater_install')
    window.api.dismissUpdateError()
    expect(invokeMock).toHaveBeenCalledWith('updater_dismiss_error')
  })

  it('localizes the directory picker title from the stored language', async () => {
    openDialogMock.mockResolvedValue('/tmp/repo')
    window.localStorage.setItem('buddy.language', 'zh-CN')

    await expect(window.api.selectDirectory('/tmp')).resolves.toBe('/tmp/repo')
    expect(openDialogMock).toHaveBeenCalledWith({
      directory: true,
      canCreateDirectories: true,
      defaultPath: '/tmp',
      title: '选择工作目录'
    })
  })

  it('falls back to the English picker title by default', async () => {
    openDialogMock.mockResolvedValue(null)

    await expect(window.api.selectDirectory()).resolves.toBeNull()
    expect(openDialogMock).toHaveBeenCalledWith(
      expect.objectContaining({ title: 'Select working directory' })
    )
  })

  it('subscribes to buddy:event and forwards only the payload', async () => {
    const unlisten = vi.fn()
    listenMock.mockResolvedValue(unlisten)
    const callback = vi.fn()

    const unsubscribe = window.buddy.onTaskEvent(callback)
    expect(listenMock).toHaveBeenCalledWith('buddy:event', expect.any(Function))

    const handler = listenMock.mock.calls.find((c) => c[0] === 'buddy:event')![1] as (e: { payload: unknown }) => void
    const payload = { task_id: 'task-1', event: { type: 'done' } }
    handler({ payload })
    expect(callback).toHaveBeenCalledWith(payload)

    unsubscribe()
    await vi.waitFor(() => expect(unlisten).toHaveBeenCalled())
  })

  it('subscribes to menu:action, updater:event and window:fullScreenChange', () => {
    listenMock.mockResolvedValue(vi.fn())

    const menuCb = vi.fn()
    window.api.onMenuAction(menuCb)
    expect(listenMock).toHaveBeenCalledWith('menu:action', expect.any(Function))
    const menuHandler = listenMock.mock.calls.find((c) => c[0] === 'menu:action')![1] as (e: { payload: unknown }) => void
    menuHandler({ payload: 'new-task' })
    expect(menuCb).toHaveBeenCalledWith('new-task')

    const updaterCb = vi.fn()
    window.api.onUpdaterEvent(updaterCb)
    expect(listenMock).toHaveBeenCalledWith('updater:event', expect.any(Function))

    const fsCb = vi.fn()
    window.api.onFullScreenChange(fsCb)
    expect(listenMock).toHaveBeenCalledWith('window:fullScreenChange', expect.any(Function))
    const fsHandler = listenMock.mock.calls.find((c) => c[0] === 'window:fullScreenChange')![1] as (e: { payload: unknown }) => void
    fsHandler({ payload: true })
    expect(fsCb).toHaveBeenCalledWith(true)
  })

  describe('external link interception', () => {
    function clickElement(el: Element): MouseEvent {
      const event = new MouseEvent('click', { bubbles: true, cancelable: true, button: 0 })
      el.dispatchEvent(event)
      return event
    }

    it('opens external http links in the system browser and prevents webview navigation', () => {
      const anchor = document.createElement('a')
      anchor.href = 'https://example.com/docs'
      anchor.innerHTML = '<span>docs</span>'
      document.body.appendChild(anchor)

      // Click a nested element: delegation must still resolve the anchor,
      // matching links inside markdown HTML.
      const event = clickElement(anchor.querySelector('span')!)
      expect(event.defaultPrevented).toBe(true)
      expect(openUrlMock).toHaveBeenCalledWith('https://example.com/docs')
      anchor.remove()
    })

    it('opens mailto links externally', () => {
      const anchor = document.createElement('a')
      anchor.href = 'mailto:user@example.com'
      document.body.appendChild(anchor)

      const event = clickElement(anchor)
      expect(event.defaultPrevented).toBe(true)
      expect(openUrlMock).toHaveBeenCalledWith('mailto:user@example.com')
      anchor.remove()
    })

    it('leaves same-origin and in-page links alone', () => {
      const hashLink = document.createElement('a')
      hashLink.href = '#section'
      const relativeLink = document.createElement('a')
      relativeLink.href = '/internal/page'
      document.body.append(hashLink, relativeLink)

      expect(clickElement(hashLink).defaultPrevented).toBe(false)
      expect(clickElement(relativeLink).defaultPrevented).toBe(false)
      expect(openUrlMock).not.toHaveBeenCalled()
      hashLink.remove()
      relativeLink.remove()
    })

    it('ignores clicks outside anchors', () => {
      const div = document.createElement('div')
      document.body.appendChild(div)

      expect(clickElement(div).defaultPrevented).toBe(false)
      expect(openUrlMock).not.toHaveBeenCalled()
      div.remove()
    })
  })
})
