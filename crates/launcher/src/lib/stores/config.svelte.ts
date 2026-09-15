import { invoke } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';

export type HotkeyAction = 'toggle_overlay' | 'translate' | 'quick_ask';
export interface CompanionStatus {
  path: string;
  config: {
    instructions: { system_prompt: string; quick_ask_prompt: string };
    hotkeys: Record<HotkeyAction, string>;
    speech: { auto_resume: boolean };
    sessions: {
      max_image_turns: number;
      max_turns: number;
      handoff_messages: number;
      handoff_chars: number;
    };
    notebook: {
      project: string;
      brief_chars: number;
      reference_chars: number;
      checkpoint_chars: number;
      revisions: number;
    };
  };
  error: string | null;
}

let status = $state<CompanionStatus | null>(null);

export function getCompanionConfig(): CompanionStatus | null {
  return status;
}

export function shortcutLabel(action: HotkeyAction): string {
  if (!status) return 'Loading…';
  return status.config.hotkeys[action].trim() || 'Disabled';
}

export async function loadCompanionConfig(): Promise<void> {
  status = await invoke<CompanionStatus>('get_companion_config');
}

export async function reloadCompanionConfig(): Promise<void> {
  try {
    status = await invoke<CompanionStatus>('reload_companion_config');
  } catch (error) {
    await loadCompanionConfig();
    throw error;
  }
}

/** Each webview subscribes once; labels always reflect the applied config. */
export function watchCompanionConfig(): () => void {
  const listener = listen<CompanionStatus>('companion-config-changed', (event) => {
    status = event.payload;
  });
  void listener.then(() => loadCompanionConfig()).catch(console.error);
  return () => {
    void listener.then((unlisten) => unlisten()).catch(console.error);
  };
}
