import { invoke } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';

export interface NotebookStatus {
  ready: boolean;
  project: string;
  path: string;
  identity: string;
  updated_at: string;
  revision: number;
  objective: string;
  error: string | null;
}

export interface NotebookChoice {
  id: string;
  source_identity: string;
  source_project: string;
  naming: boolean;
  request: { kind: 'switch' | 'create' | 'write_here'; project: string; topic: string };
}

export interface NotebookChoiceResult {
  pending: NotebookChoice | null;
  selected_project: string | null;
  notice: string;
}

let status = $state<NotebookStatus | null>(null);

export function getNotebook(): NotebookStatus | null {
  return status;
}

export async function refreshNotebook(): Promise<NotebookStatus> {
  status = await invoke<NotebookStatus>('get_notebook_status');
  return status;
}

export async function resetNotebook(restorePrevious = false): Promise<NotebookStatus> {
  status = await invoke<NotebookStatus>('reset_notebook', { restorePrevious });
  return status;
}

export function watchNotebook(): () => void {
  const listener = listen<NotebookStatus>('notebook-status', (event) => {
    status = event.payload;
  });
  void listener.then(() => refreshNotebook()).catch(console.error);
  return () => {
    void listener.then((unlisten) => unlisten()).catch(console.error);
  };
}
