import type { Api, BuddyApi } from './lib/tauri-bridge'

declare global {
  interface Window {
    api: Api
    buddy: BuddyApi
  }
}

export {}
