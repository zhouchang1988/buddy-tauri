// Tauri bridge: constructs window.buddy and window.api with the exact same
// shapes as the Electron preload (src/preload/index.ts + buddy-api.ts in the
// original project), backed by Tauri invoke()/listen() instead of ipcRenderer.
//
// Command naming convention (implemented by the Rust backend):
//   buddy:xxx IPC channel  ->  snake_case command buddy_xxx
//   args are passed as a single object with camelCase keys matching the
//   original positional argument names.

import { invoke } from '@tauri-apps/api/core'
import { listen } from '@tauri-apps/api/event'
import { getCurrentWindow } from '@tauri-apps/api/window'
import { open as openDialog } from '@tauri-apps/plugin-dialog'
import { openUrl, revealItemInDir } from '@tauri-apps/plugin-opener'
import { currentLanguage, translate } from './i18n'
import type {
  AttachmentMeta,
  BootstrapResponse,
  CountdownInput,
  CreateTaskInput,
  CreateTaskResult,
  Event,
  GitCommitPushResult,
  GitPushAvailability,
  GitPushResult,
  GlobalSettings,
  InstructionQueueItem,
  RoundEventSummary,
  SendMessageInput,
  StartTaskInput,
  Task,
  TaskDetail,
  TaskEventEnvelope,
  TaskSettings,
  TaskStats,
  TestLauncherResult
} from '../shared/types'

const buddy = {
  checkHealth: (): Promise<boolean> =>
    invoke<boolean>('buddy_check_health'),
  bootstrap: (): Promise<BootstrapResponse> =>
    invoke<BootstrapResponse>('buddy_bootstrap'),
  getTasks: (): Promise<Task[]> =>
    invoke<Task[]>('buddy_get_tasks'),
  getTaskDetail: (taskId: string, workspaceKey?: string): Promise<TaskDetail> =>
    invoke<TaskDetail>('buddy_get_task_detail', { taskId, workspaceKey }),
  createTask: (input: CreateTaskInput): Promise<CreateTaskResult> =>
    invoke<CreateTaskResult>('buddy_create_task', { input }),
  deleteTask: (taskId: string, workspaceKey?: string): Promise<void> =>
    invoke<void>('buddy_delete_task', { taskId, workspaceKey }),
  startTask: (taskId: string, input: StartTaskInput): Promise<void> =>
    invoke<void>('buddy_start_task', { taskId, input }),
  sendMessage: (taskId: string, input: SendMessageInput): Promise<void> =>
    invoke<void>('buddy_send_message', { taskId, input }),
  skipCountdown: (taskId: string, input: CountdownInput): Promise<void> =>
    invoke<void>('buddy_skip_countdown', { taskId, input }),
  pauseCountdown: (taskId: string, input: CountdownInput): Promise<void> =>
    invoke<void>('buddy_pause_countdown', { taskId, input }),
  interrupt: (taskId: string, workspaceKey?: string): Promise<void> =>
    invoke<void>('buddy_interrupt', { taskId, workspaceKey }),
  enqueueInstruction: (taskId: string, workspaceKey: string, content: string, attachments?: AttachmentMeta[]): Promise<InstructionQueueItem> =>
    invoke<InstructionQueueItem>('buddy_enqueue_instruction', { taskId, workspaceKey, content, attachments }),
  dequeueInstruction: (taskId: string, workspaceKey: string, itemId: string): Promise<void> =>
    invoke<void>('buddy_dequeue_instruction', { taskId, workspaceKey, itemId }),
  clearInstructionQueue: (taskId: string, workspaceKey: string): Promise<void> =>
    invoke<void>('buddy_clear_instruction_queue', { taskId, workspaceKey }),
  interruptAndInsert: (taskId: string, workspaceKey: string, queueItemId: string): Promise<void> =>
    invoke<void>('buddy_interrupt_and_insert', { taskId, workspaceKey, queueItemId }),
  getEvents: (taskId: string, since: number, workspaceKey?: string): Promise<{ events: Event[] }> =>
    invoke<{ events: Event[] }>('buddy_get_events', { taskId, since, workspaceKey }),
  getRoundEvents: (taskId: string, runId: string, workspaceKey?: string, actor?: string): Promise<RoundEventSummary | null> =>
    invoke<RoundEventSummary | null>('buddy_get_round_events', { taskId, runId, workspaceKey, actor }),
  getTaskStats: (taskId: string, workspaceKey?: string): Promise<TaskStats | null> =>
    invoke<TaskStats | null>('buddy_get_task_stats', { taskId, workspaceKey }),
  updateGlobalSettings: (settings: GlobalSettings): Promise<GlobalSettings> =>
    invoke<GlobalSettings>('buddy_update_global_settings', { settings }),
  gitStatus: (repoRoot: string): Promise<unknown> =>
    invoke<unknown>('buddy_git_status', { repoRoot }),
  gitStageAll: (repoRoot: string): Promise<void> =>
    invoke<void>('buddy_git_stage_all', { repoRoot }),
  gitStageFiles: (repoRoot: string, paths: string[]): Promise<void> =>
    invoke<void>('buddy_git_stage_files', { repoRoot, paths }),
  gitCommitAndPush: (repoRoot: string, message: string, remote: string, push?: boolean): Promise<GitCommitPushResult> =>
    invoke<GitCommitPushResult>('buddy_git_commit_and_push', { repoRoot, message, remote, push }),
  gitDiffForCommitMessage: (repoRoot: string, paths?: string[]): Promise<string> =>
    invoke<string>('buddy_git_diff_for_commit_message', { repoRoot, paths }),
  gitFileDiff: (repoRoot: string, filePath: string): Promise<string> =>
    invoke<string>('buddy_git_file_diff', { repoRoot, filePath }),
  gitBranches: (repoRoot: string): Promise<string[]> =>
    invoke<string[]>('buddy_git_branches', { repoRoot }),
  gitCheckout: (repoRoot: string, branch: string): Promise<void> =>
    invoke<void>('buddy_git_checkout', { repoRoot, branch }),
  gitCreateBranch: (repoRoot: string, branch: string): Promise<void> =>
    invoke<void>('buddy_git_create_branch', { repoRoot, branch }),
  gitPushAvailability: (repoRoot: string, remote: string): Promise<GitPushAvailability> =>
    invoke<GitPushAvailability>('buddy_git_push_availability', { repoRoot, remote }),
  gitPush: (repoRoot: string, remote: string): Promise<GitPushResult> =>
    invoke<GitPushResult>('buddy_git_push', { repoRoot, remote }),
  generateCommitMessage: (input: { repoRoot: string; actor: string; lang?: string; paths: string[]; taskSettings?: TaskSettings | null }): Promise<{ message: string }> =>
    invoke<{ message: string }>('buddy_generate_commit_message', { input }),
  cancelGenerateCommitMessage: (): Promise<void> =>
    invoke<void>('buddy_cancel_generate_commit_message'),
  testLauncher: (actor: string, command: string, env?: Record<string, string>): Promise<TestLauncherResult> =>
    invoke<TestLauncherResult>('buddy_test_launcher', { actor, command, env }),
  detectActorModels: (): Promise<Record<string, string | undefined>> =>
    invoke<Record<string, string | undefined>>('buddy_detect_actor_models'),
  updateTaskText: (taskId: string, workspaceKey: string, taskText: string): Promise<void> =>
    invoke<void>('buddy_update_task_text', { taskId, workspaceKey, taskText }),
  onTaskEvent: (callback: (payload: TaskEventEnvelope) => void): (() => void) => {
    // Electron callbacks received (event, payload); here the callback gets just the payload.
    const unlisten = listen<TaskEventEnvelope>('buddy:event', (event) => callback(event.payload))
    return () => { unlisten.then((fn) => fn()) }
  }
}

const api = {
  selectDirectory: async (defaultPath?: string): Promise<string | null> => {
    const title = translate(currentLanguage(), 'dialog.selectWorkspace.title')
    const selected = await openDialog({ directory: true, canCreateDirectories: true, defaultPath, title })
    return typeof selected === 'string' ? selected : null
  },
  openInFinder: (path: string): Promise<void> =>
    revealItemInDir(path),
  openExternal: (url: string): Promise<void> =>
    openUrl(url),
  onFullScreenChange: (callback: (isFullScreen: boolean) => void): (() => void) => {
    const unlisten = listen<boolean>('window:fullScreenChange', (event) => callback(event.payload))
    return () => { unlisten.then((fn) => fn()) }
  },
  onMenuAction: (callback: (action: string) => void): (() => void) => {
    const unlisten = listen<string>('menu:action', (event) => callback(event.payload))
    return () => { unlisten.then((fn) => fn()) }
  },
  isFullScreen: (): Promise<boolean> =>
    getCurrentWindow().isFullscreen(),
  updateMenuLanguage: (lang: string): void => {
    void invoke('update_menu_language', { lang })
  },
  readClipboardFilePaths: (): Promise<Array<{ path: string; size: number }>> =>
    invoke<Array<{ path: string; size: number }>>('read_clipboard_file_paths'),
  saveAttachmentBuffer: (taskId: string, workspaceKey: string, name: string, bufferBase64: string): Promise<string> =>
    invoke<string>('save_attachment_buffer', { taskId, workspaceKey, name, bufferBase64 }),
  readFileAsDataURL: (filePath: string, mimeType: string): Promise<string> =>
    invoke<string>('read_file_as_data_url', { filePath, mimeType }),
  checkForUpdates: (): Promise<{ error?: string } | void> =>
    invoke('updater_check').then(() => undefined).catch((e) => ({ error: String(e) })),
  downloadUpdate: (): Promise<{ error?: string } | void> =>
    invoke('updater_download').then(() => undefined).catch((e) => ({ error: String(e) })),
  installUpdate: (): Promise<{ error?: string } | void> =>
    invoke('updater_install').then(() => undefined).catch((e) => ({ error: String(e) })),
  dismissUpdateError: (): void => {
    void invoke('updater_dismiss_error')
  },
  onUpdaterEvent: (callback: (event: unknown) => void): (() => void) => {
    const unlisten = listen<unknown>('updater:event', (event) => callback(event.payload))
    return () => { unlisten.then((fn) => fn()) }
  }
}

window.buddy = buddy
window.api = api

export type Api = typeof api
export type BuddyApi = typeof buddy
