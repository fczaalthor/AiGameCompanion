<script lang="ts">
  import { onMount, tick } from 'svelte';
  import { invoke, Channel } from '@tauri-apps/api/core';
  import { listen } from '@tauri-apps/api/event';
  import { hashHue } from '../utils/accent';
  import { PROVIDERS, type Provider } from '../stores/companion.svelte';
  import { shortcutLabel } from '../stores/config.svelte';
  import {
    getNotebook,
    refreshNotebook,
    resetNotebook,
    type NotebookChoice,
    type NotebookChoiceResult,
  } from '../stores/notebook.svelte';

  type GameInfo = {
    hwnd: number;
    pid: number;
    exe: string;
    title: string;
    accent?: string;
  } | null;
  type Availability = { gemini: boolean; claude: boolean; openai: boolean };
  type SageEvent = {
    kind: 'chunk' | 'done' | 'error';
    requestId: number;
    conversationId: number;
    text?: string;
    message?: string;
  };
  type Msg = {
    role: 'user' | 'assistant';
    content: string;
    model?: string;
    screenshot?: boolean;
    streaming?: boolean;
  };

  const PROVIDER_ORDER: Provider[] = ['gemini', 'claude', 'openai'];
  const SUGGESTIONS = ['Where do I go next?', "What's this enemy weak to?", 'Explain this screen'];

  let game = $state<GameInfo>(null);
  let availability = $state<Availability>({ gemini: false, claude: false, openai: false });
  let provider = $state<Provider>('gemini');
  let savedProvider: Provider | null = null;
  let dropdownOpen = $state(false);
  let tab = $state<'chat' | 'translate'>('chat');
  let attach = $state(false);
  let prompt = $state('');
  let asking = $state(false);
  let messages = $state<Msg[]>([]);

  let translateText = $state('');
  let translateBusy = $state(false);
  let translateError = $state('');
  let speechError = $state('');
  let notebookError = $state('');
  let preparing = $state(false);
  let notebookBusy = $state(false);
  let notebookChoice = $state<NotebookChoice | null>(null);
  let proposedName = $state('');
  let chatNotebookIdentity: string | null = null;
  const notebook = $derived(getNotebook());

  // Plain counters (not reactive): real request ids start at 1, so 0 = "none".
  let nextRequestId = 0;
  let conversationId = 1;
  let activeRequestId = 0;
  let streamIndex = -1;
  let savedProviderLoaded = false;
  let speechHandoffBusy = false;

  const available = $derived(PROVIDER_ORDER.filter((p) => availability[p]));
  const meta = $derived(PROVIDERS[provider]);
  const accent = $derived(
    game ? (game.accent ?? hashHue(game.exe || game.title || 'sage')) : '#e0a23c',
  );
  const canAttach = $derived(!!game);
  const canSend = $derived(!!game && available.length > 0);
  const captureHint = $derived.by(() => {
    if (attach && canAttach) return 'screenshot attached · WGC';
    return 'screenshot attaches via WGC';
  });

  // Re-query availability (CLI detection can lag startup); restore the saved
  // provider once it's known-available, else fall back to the first available.
  async function refreshProviders() {
    try {
      availability = await invoke<Availability>('available_providers');
    } catch {
      return;
    }
    if (savedProvider && availability[savedProvider]) provider = savedProvider;
    else if (!availability[provider] && available.length > 0) provider = available[0];
  }

  async function selectProvider(p: Provider) {
    provider = p;
    savedProvider = p;
    dropdownOpen = false;
    try {
      await invoke('set_active_provider', { provider: p });
    } catch {
      /* selection still applies for this session */
    }
  }

  async function newChat() {
    const inflight = asking ? activeRequestId : 0;
    // Reset synchronously first so a Send fired during the cancel IPC gap cannot
    // be clobbered by a post-await state reset.
    activeRequestId = 0;
    asking = false;
    conversationId += 1;
    messages = [];
    prompt = '';
    if (inflight) {
      try {
        await invoke('cancel_sage', { requestId: inflight });
      } catch {
        /* best effort */
      }
    }
  }

  function onWindowPointerDown(event: PointerEvent) {
    if (!dropdownOpen) return;
    const target = event.target as HTMLElement;
    if (!target.closest('.provider-pill') && !target.closest('.dropdown')) {
      dropdownOpen = false;
    }
  }

  async function send(text?: string, speakWhenDone = false) {
    const question = (text ?? prompt).trim();
    if (!question || asking || preparing || notebookBusy || notebookChoice || !canSend) return;
    // Capture the one-shot flag before the notebook read yields to another UI event.
    const withShot = attach && canAttach;
    preparing = true;
    notebookError = '';
    try {
      const current = await refreshNotebook();
      if (!current.ready) throw new Error(current.error ?? 'Notebook could not be loaded.');
      if (chatNotebookIdentity !== null && chatNotebookIdentity !== current.identity)
        await newChat();
      chatNotebookIdentity = current.identity;
    } catch (error) {
      notebookError = String(error);
      if (speakWhenDone) void invoke('show_overlay_for_speech');
      return;
    } finally {
      preparing = false;
    }

    const id = (nextRequestId += 1);
    const convo = conversationId;
    activeRequestId = id;

    // History for the backend: prior turns + this question.
    const outgoing = messages.map((m) => ({ role: m.role, content: m.content }));
    outgoing.push({ role: 'user', content: question });

    messages = [
      ...messages,
      { role: 'user', content: question, screenshot: withShot },
      { role: 'assistant', content: '', model: meta.model, streaming: true },
    ];
    const idx = messages.length - 1;
    streamIndex = idx;
    prompt = '';
    asking = true;

    const channel = new Channel<SageEvent>();
    channel.onmessage = (event) => {
      // Ignore output from a superseded request or cleared conversation.
      if (event.requestId !== activeRequestId || event.conversationId !== convo) return;
      if (event.kind === 'chunk') {
        messages[idx].content += event.text ?? '';
      } else if (event.kind === 'done') {
        messages[idx].streaming = false;
        asking = false;
        void finishNotebookTurn(idx, speakWhenDone);
      } else if (event.kind === 'error') {
        const msg = event.message ?? 'Unknown error';
        messages[idx].content = messages[idx].content
          ? `${messages[idx].content}\n\n[error] ${msg}`
          : `[error] ${msg}`;
        messages[idx].streaming = false;
        asking = false;
        if (speakWhenDone) void invoke('show_overlay_for_speech');
      }
    };

    try {
      await invoke('ask_sage', {
        requestId: id,
        conversationId: convo,
        provider,
        messages: outgoing,
        attachScreenshot: withShot,
        notebookIdentity: chatNotebookIdentity,
        channel,
      });
    } catch (err) {
      messages[idx].content = `[error] ${String(err)}`;
      messages[idx].streaming = false;
      asking = false;
      if (speakWhenDone) void invoke('show_overlay_for_speech');
    }
  }

  async function refreshNotebookChoice() {
    notebookChoice = await invoke<NotebookChoice | null>('get_notebook_choice');
    if (notebookChoice) proposedName = notebookChoice.request.project;
  }

  async function finishNotebookTurn(index: number, speakWhenDone: boolean) {
    try {
      await refreshNotebookChoice();
      if (notebookChoice) {
        // A hard stop must remain visible; do not send Escape to the game.
        await invoke('show_overlay_for_speech');
        if (speakWhenDone) await readReply(index);
      } else if (speakWhenDone) {
        await readReply(index, true);
      }
    } catch (error) {
      notebookError = String(error);
      if (speakWhenDone) void invoke('show_overlay_for_speech');
    }
  }

  async function answerNotebookChoice(action: string) {
    const choice = notebookChoice;
    if (!choice || notebookBusy || asking || speechHandoffBusy) return;
    notebookBusy = true;
    notebookError = '';
    try {
      const result = await invoke<NotebookChoiceResult>('answer_notebook_choice', {
        id: choice.id,
        action,
        name: proposedName.trim(),
      });
      notebookChoice = result.pending;
      if (result.pending) {
        proposedName = result.pending.request.project;
      } else {
        if (result.selected_project) {
          await newChat();
          const current = await refreshNotebook();
          chatNotebookIdentity = current.identity;
        } else if (action !== 'cancel') {
          // Preserve the human choice as the user's message, not as a model claim.
          messages = [
            ...messages,
            {
              role: 'user',
              content: `Keep this investigation and its notes in ${choice.source_project}.`,
            },
          ];
        } else {
          messages = [
            ...messages,
            {
              role: 'user',
              content:
                'Cancel that notebook proposal. I have not approved a switch or new notebook.',
            },
          ];
        }
      }
      messages = [...messages, { role: 'assistant', content: result.notice }];
    } catch (error) {
      notebookError = String(error);
      await refreshNotebookChoice();
    } finally {
      notebookBusy = false;
    }
  }

  async function openNotebook() {
    try {
      await invoke('open_notebook');
    } catch (error) {
      notebookError = String(error);
    }
  }

  async function reloadNotebook() {
    if (asking || preparing || notebookBusy || speechHandoffBusy) return;
    notebookBusy = true;
    notebookError = '';
    try {
      const current = await refreshNotebook();
      if (!current.ready) throw new Error(current.error ?? 'Notebook could not be loaded.');
      if (chatNotebookIdentity !== null && chatNotebookIdentity !== current.identity) {
        const draft = prompt;
        await newChat();
        prompt = draft;
      }
      chatNotebookIdentity = current.identity;
    } catch (error) {
      notebookError = String(error);
    } finally {
      notebookBusy = false;
    }
  }

  async function changeRun(restorePrevious: boolean) {
    if (asking || preparing || notebookBusy || notebookChoice || speechHandoffBusy) return;
    notebookBusy = true;
    notebookError = '';
    try {
      const current = await resetNotebook(restorePrevious);
      await newChat();
      chatNotebookIdentity = current.identity;
    } catch (error) {
      notebookError = String(error);
    } finally {
      notebookBusy = false;
    }
  }

  async function readReply(index: number, returnToGame = false) {
    if (speechHandoffBusy) return;
    const currentConversation = conversationId;
    const reply = messages[index];
    if (!reply || reply.role !== 'assistant' || reply.streaming || !reply.content.trim()) return;
    speechError = '';
    speechHandoffBusy = true;
    let overlayShown = false;
    try {
      await tick();
      if (conversationId !== currentConversation || messages[index] !== reply) return;
      if (returnToGame) {
        await invoke('show_overlay_for_speech');
        overlayShown = true;
        await tick();
      }

      // Speechify reads the Windows selection. Select only this answer bubble:
      // Ctrl+A would also read the provider, WGC label, and window header.
      const bubble = document.querySelector<HTMLElement>(`[data-speech-reply="${index}"]`);
      if (!bubble || bubble.textContent?.trim() !== reply.content.trim()) {
        throw new Error('Could not select the completed reply.');
      }
      bubble.scrollIntoView({ block: 'nearest' });
      const selection = window.getSelection();
      if (!selection) throw new Error('Text selection is unavailable.');
      const range = document.createRange();
      range.selectNodeContents(bubble);
      selection.removeAllRanges();
      selection.addRange(range);

      await invoke('speak_selected_reply');
      if (returnToGame) {
        // Give Speechify time to copy the selection before hiding its source.
        await new Promise((resolve) => setTimeout(resolve, 700));
      }
    } catch (error) {
      speechError = `Speechify: ${String(error)}`;
    } finally {
      if (returnToGame && overlayShown && conversationId === currentConversation) {
        try {
          await invoke('hide_overlay_and_resume_game');
        } catch (error) {
          speechError = `Game resume: ${String(error)}`;
        }
      }
      speechHandoffBusy = false;
    }
  }

  async function stop() {
    if (!asking) return;
    const id = activeRequestId;
    activeRequestId = 0;
    asking = false;
    if (streamIndex >= 0 && streamIndex < messages.length) messages[streamIndex].streaming = false;
    try {
      await invoke('cancel_sage', { requestId: id });
    } catch {
      /* best effort */
    }
  }

  function onKeydown(event: KeyboardEvent) {
    if (event.key === 'Enter' && !event.shiftKey) {
      event.preventDefault();
      void send();
    }
  }

  async function hideOverlay() {
    try {
      await invoke('hide_overlay_to_game');
    } catch {
      /* window may not exist in preview */
    }
  }

  async function runTranslate() {
    if (translateBusy) return;
    if (!availability.gemini) {
      translateText = '';
      translateError = 'Translation requires a Gemini API key.';
      return;
    }
    translateBusy = true;
    translateError = '';
    try {
      const res = await invoke<{ text: string }>('translate_screen');
      translateText = res.text;
    } catch (err) {
      translateError = String(err);
      translateText = '';
    } finally {
      translateBusy = false;
    }
  }

  async function runQuickAsk(target: GameInfo, question: string) {
    game = target;
    tab = 'chat';
    if (notebookChoice) {
      void invoke('show_overlay_for_speech');
      return;
    }
    if (asking || preparing || notebookBusy || speechHandoffBusy) return;
    if (!canSend) {
      void invoke('show_overlay_for_speech');
      return;
    }
    // Attach a frame for this one-shot without leaving the toggle on.
    const prev = attach;
    attach = canAttach;
    const pending = send(question, true);
    attach = prev;
    await pending;
  }

  async function copyTranslation() {
    if (!translateText) return;
    try {
      await navigator.clipboard.writeText(translateText);
    } catch {
      /* clipboard may be unavailable */
    }
  }

  onMount(() => {
    // Only the overlay window mounts this; keep its surface transparent.
    document.documentElement.style.background = 'transparent';
    document.body.style.background = 'transparent';

    void (async () => {
      await refreshNotebookChoice().catch((error) => {
        notebookError = String(error);
      });
      try {
        const settings = await invoke<{ active_provider?: string }>('get_settings');
        savedProvider = (settings.active_provider as Provider | undefined) ?? null;
      } catch {
        /* defaults apply */
      }
      savedProviderLoaded = true;
      await refreshProviders();
    })();

    const listeners = [
      listen<GameInfo>('overlay-status', (event) => {
        game = event.payload;
        // The overlay just became visible: CLI detection has had time to finish.
        if (savedProviderLoaded) void refreshProviders();
      }),
      listen('translate-request', () => {
        tab = 'translate';
        void runTranslate();
      }),
      listen<{ game: GameInfo; prompt: string }>('quick-ask', (event) => {
        void runQuickAsk(event.payload.game, event.payload.prompt);
      }),
    ];
    // Native CLIs are detected on a background thread. Recheck once after the
    // initial mount so the panel does not remain stuck on "No providers" while
    // that scan finishes.
    const providerRefreshTimer = setTimeout(() => {
      if (savedProviderLoaded && available.length === 0) void refreshProviders();
    }, 3_000);
    return () => {
      clearTimeout(providerRefreshTimer);
      for (const listener of listeners) listener.then((unlisten) => unlisten());
    };
  });
</script>

<svelte:window onpointerdown={onWindowPointerDown} />

<div class="overlay-root" style="--accent: {accent};">
  <div class="panel">
    <!-- titlebar -->
    <div class="titlebar" data-tauri-drag-region>
      <span class="logo"></span>
      <span class="wordmark">SAGE</span>
      <span class="drag-chip">drag</span>
      <div class="title-actions">
        <button
          class="icon-btn"
          onclick={newChat}
          disabled={preparing || notebookBusy || !!notebookChoice}
          title="New chat (keep notebook)"
          aria-label="New chat"
        >
          <svg
            width="15"
            height="15"
            viewBox="0 0 24 24"
            fill="none"
            stroke="currentColor"
            stroke-width="1.7"
            stroke-linecap="round"
            stroke-linejoin="round"><path d="M3 12a9 9 0 1 0 3-6.7L3 8" /><path d="M3 3v5h5" /></svg
          >
        </button>
        <button
          class="icon-btn"
          onclick={hideOverlay}
          title={`Hide (${shortcutLabel('toggle_overlay')})`}
          aria-label="Hide"
        >
          <svg
            width="15"
            height="15"
            viewBox="0 0 24 24"
            fill="none"
            stroke="currentColor"
            stroke-width="1.9"
            stroke-linecap="round"
            stroke-linejoin="round"><path d="M6 9l6 6 6-6" /></svg
          >
        </button>
      </div>
    </div>

    <!-- detected game -->
    <div class="gamebar">
      <span class="game-tile" class:muted={!game}></span>
      <div class="game-meta">
        {#if game}
          <span class="game-title">{game.title || game.exe}</span>
          <span class="game-exe">{game.exe}</span>
        {:else}
          <span class="game-title dim">No game detected</span>
          <span class="game-exe">bring a game to the foreground</span>
        {/if}
      </div>
      {#if game}
        <span class="linked-pill"><span class="d"></span>linked</span>
      {/if}
    </div>

    {#if notebook?.project}
      <div class="notebook-bar">
        <button onclick={openNotebook} title={notebook.path}>Notebook · {notebook.project}</button>
        <span title={notebook.updated_at}>
          {notebook.ready ? `Saved · ${notebook.revision}` : 'Needs attention'}
        </span>
        <button onclick={reloadNotebook} disabled={asking || preparing || notebookBusy}
          >Reload</button
        >
        <button
          onclick={() => changeRun(false)}
          disabled={asking || preparing || notebookBusy}
          title="Clear current-run notes and chat; keep your brief and references">New run</button
        >
        <button
          onclick={() => changeRun(true)}
          disabled={asking || preparing || notebookBusy}
          title="Restore the previous checkpoint and start a fresh chat">Undo checkpoint</button
        >
      </div>
    {/if}
    {#if notebookError || notebook?.error}
      <div class="notebook-error" role="alert">{notebookError || notebook?.error}</div>
    {/if}

    {#if notebookChoice}
      <section class="notebook-choice" aria-label="Notebook decision">
        {#if notebookChoice.naming}
          <p>Is this name OK for the new notebook?</p>
          <label
            >Notebook name <input
              class="text-input"
              bind:value={proposedName}
              maxlength="64"
              disabled={notebookBusy}
            /></label
          >
          <small>Use lowercase letters, numbers, hyphens or underscores.</small>
          <div class="choice-actions">
            <button
              onclick={() => answerNotebookChoice('confirm_name')}
              disabled={notebookBusy || !proposedName.trim()}>Approve name and switch</button
            >
            <button onclick={() => answerNotebookChoice('cancel')} disabled={notebookBusy}
              >Cancel</button
            >
          </div>
        {:else}
          <p>
            {#if notebookChoice.request.kind === 'create'}
              Do you need a new notebook for {notebookChoice.request.topic}?
            {:else if notebookChoice.request.kind === 'switch'}
              Switch to {notebookChoice.request.project} and write notes there for {notebookChoice
                .request.topic}?
            {:else}
              Should notes about {notebookChoice.request.topic} stay in {notebookChoice.source_project}?
            {/if}
          </p>
          <div class="choice-actions">
            <button onclick={() => answerNotebookChoice('accept')} disabled={notebookBusy}>
              {notebookChoice.request.kind === 'create'
                ? 'Yes, a new notebook'
                : notebookChoice.request.kind === 'switch'
                  ? 'Yes, switch'
                  : 'Yes, write here'}
            </button>
            {#if notebookChoice.request.kind !== 'write_here'}
              <button onclick={() => answerNotebookChoice('keep')} disabled={notebookBusy}
                >Keep notes here</button
              >
            {/if}
            <button onclick={() => answerNotebookChoice('cancel')} disabled={notebookBusy}
              >Cancel / explain</button
            >
          </div>
        {/if}
        <small>Waiting for your choice. Notes from this turn have not been saved.</small>
      </section>
    {/if}

    <!-- tabs + provider -->
    <div class="tabrow">
      <div class="tabs">
        <button class="tab" class:active={tab === 'chat'} onclick={() => (tab = 'chat')}>
          <svg
            width="14"
            height="14"
            viewBox="0 0 24 24"
            fill="none"
            stroke="currentColor"
            stroke-width="1.7"
            stroke-linecap="round"
            stroke-linejoin="round"
            ><path d="M21 15a2 2 0 0 1-2 2H7l-4 4V5a2 2 0 0 1 2-2h14a2 2 0 0 1 2 2z" /></svg
          >
          Chat
        </button>
        <button class="tab" class:active={tab === 'translate'} onclick={() => (tab = 'translate')}>
          <svg
            width="14"
            height="14"
            viewBox="0 0 24 24"
            fill="none"
            stroke="currentColor"
            stroke-width="1.7"
            stroke-linecap="round"
            stroke-linejoin="round"
            ><circle cx="12" cy="12" r="9" /><path
              d="M3 12h18M12 3a15 15 0 0 1 0 18M12 3a15 15 0 0 0 0 18"
            /></svg
          >
          Translate
        </button>
      </div>
      <button
        class="provider-pill"
        onclick={() => (dropdownOpen = !dropdownOpen)}
        disabled={available.length === 0 || asking}
      >
        <span class="prov-dot" style="background: {meta.dot}; box-shadow: 0 0 6px {meta.dot};"
        ></span>
        {available.length === 0 ? 'No providers' : meta.label}
        <span class="caret">{dropdownOpen ? '▴' : '▾'}</span>
      </button>

      {#if dropdownOpen && available.length > 0}
        <div class="dropdown">
          <div class="dropdown-head">Available providers</div>
          {#each available as p (p)}
            <button class="prov-row" onclick={() => selectProvider(p)}>
              <span class="pdot" style="background: {PROVIDERS[p].dot};"></span>
              <span class="pmeta">
                <span class="pname">{PROVIDERS[p].label}</span>
                <span class="pmodel">{PROVIDERS[p].model}</span>
              </span>
              {#if p === provider}
                <span class="pcheck">
                  <svg
                    width="15"
                    height="15"
                    viewBox="0 0 24 24"
                    fill="none"
                    stroke="currentColor"
                    stroke-width="2.2"
                    stroke-linecap="round"
                    stroke-linejoin="round"><path d="M5 13l4 4L19 7" /></svg
                  >
                </span>
              {/if}
            </button>
          {/each}
        </div>
      {/if}
    </div>

    {#if tab === 'chat'}
      <!-- chat body -->
      <div class="body">
        <div class="msglist">
          {#if available.length === 0}
            <div class="msg sage">
              <span class="avatar"></span>
              <div class="bubble">
                No AI providers are available. Add a Gemini key in config.toml, or install the
                Claude / Codex CLI.
              </div>
            </div>
          {:else if messages.length === 0}
            <div class="msg sage">
              <span class="avatar"></span>
              <div class="bubble">
                {#if game}
                  Linked to {game.title || game.exe}. I can see your screen — ask me anything, or
                  tap a prompt below.
                {:else}
                  Bring a game to the foreground and I'll link to it. Then ask me anything about
                  what's on screen.
                {/if}
              </div>
            </div>
            {#if game}
              <div class="chips">
                {#each SUGGESTIONS as s (s)}
                  <button class="chip" onclick={() => send(s)}>{s}</button>
                {/each}
              </div>
            {/if}
          {:else}
            {#each messages as m, i (i)}
              {#if m.role === 'user'}
                <div class="msg user">
                  {#if m.screenshot}
                    <span class="frame-chip"><span class="thumb"></span>frame · WGC</span>
                  {/if}
                  <div class="bubble">{m.content}</div>
                </div>
              {:else}
                <div class="msg sage">
                  <span class="avatar"></span>
                  <div>
                    <div class="bubble" data-speech-reply={i}>
                      {#if m.content}{m.content}{/if}{#if m.streaming && m.content}<span
                          class="caret-blink"
                        ></span>{/if}
                      {#if m.streaming && !m.content}
                        <span class="thinking"><i></i><i></i><i></i></span>
                      {/if}
                    </div>
                    {#if m.model && (m.content || !m.streaming)}
                      <div class="meta">{m.model}{m.streaming ? ' · streaming' : ''}</div>
                    {/if}
                    {#if !m.streaming && m.content && !m.content.includes('[error]')}
                      <button
                        class="read-reply"
                        onclick={() => void readReply(i)}
                        title="Read only this answer with Speechify">Read aloud</button
                      >
                    {/if}
                  </div>
                </div>
              {/if}
            {/each}
          {/if}
        </div>

        <div class="inputbar">
          <div class="inputrow">
            <button
              class="attach-btn"
              class:off={!(attach && canAttach)}
              disabled={!canAttach}
              onclick={() => (attach = !attach)}
              title="Attach a screenshot of the game"
              aria-label="Attach screenshot"
            >
              <svg
                width="18"
                height="18"
                viewBox="0 0 24 24"
                fill="none"
                stroke="currentColor"
                stroke-width="1.7"
                stroke-linecap="round"
                stroke-linejoin="round"
                ><rect x="3" y="3" width="18" height="18" rx="2" /><circle
                  cx="8.5"
                  cy="8.5"
                  r="1.5"
                /><path d="M21 15l-5-5L5 21" /></svg
              >
            </button>
            <input
              class="text-input"
              bind:value={prompt}
              onkeydown={onKeydown}
              disabled={!canSend || !!notebookChoice}
              placeholder={game ? `Ask Sage about ${game.title || game.exe}…` : 'No game detected'}
            />
            {#if asking}
              <button class="send-btn" onclick={stop} title="Stop" aria-label="Stop">
                <svg width="13" height="13" viewBox="0 0 24 24" fill="currentColor"
                  ><rect x="5" y="5" width="14" height="14" rx="2" /></svg
                >
              </button>
            {:else}
              <button
                class="send-btn"
                onclick={() => send()}
                disabled={!canSend || !!notebookChoice || !prompt.trim()}
                title="Send"
                aria-label="Send"
              >
                <svg width="17" height="17" viewBox="0 0 24 24" fill="currentColor"
                  ><path d="M3 11l18-8-8 18-2-7-8-3z" /></svg
                >
              </button>
            {/if}
          </div>
          <div class="footer">
            <span>{meta.model} · {asking ? 'streaming' : 'Enter to send'}</span>
            <span title={speechError}>{speechError || captureHint}</span>
          </div>
        </div>
      </div>
    {:else}
      <!-- translate -->
      <div class="body translate">
        <div class="capture-box">
          <div class="capture-head">CAPTURED · Windows.Graphics.Capture</div>
          <div class="capture-frame" class:busy={translateBusy}></div>
        </div>
        <div class="lang-row">
          <span class="lang-chip">Auto-detect</span>
          <span class="lang-arrow">→</span>
          <span class="lang-chip accent">English</span>
        </div>
        <div class="translate-result">
          {#if translateBusy}
            <div class="thinking"><i></i><i></i><i></i></div>
          {:else if translateError}
            <div class="te-title" style="color: var(--color-err);">{translateError}</div>
          {:else if translateText}
            <div class="translate-text">{translateText}</div>
          {:else}
            <div class="translate-empty">
              {#if !availability.gemini}
                <div class="te-title">Translation needs a Gemini key.</div>
                <div class="te-sub">Set api.gemini.api_key in config.toml.</div>
              {:else}
                <div class="te-title">No foreign text captured yet.</div>
                <div class="te-sub">
                  Aim at on-screen text and press {shortcutLabel('translate')}.
                </div>
              {/if}
            </div>
          {/if}
        </div>
        <div class="translate-actions">
          <button
            class="recapture live"
            onclick={runTranslate}
            disabled={translateBusy || !game || !availability.gemini}
            >Re-capture · {shortcutLabel('translate')}</button
          >
          <button class="recapture live" onclick={copyTranslation} disabled={!translateText}
            >Copy</button
          >
        </div>
      </div>
    {/if}
  </div>
</div>

<style>
  .notebook-choice {
    margin: 6px 14px;
    padding: 12px;
    border: 1px solid var(--accent);
    border-radius: 10px;
    background: var(--color-ink-2);
    font-size: 13px;
    overflow-y: auto;
    max-height: 240px;
    flex-shrink: 0;
  }
  .notebook-choice p {
    margin: 0 0 10px;
  }
  .notebook-choice label {
    display: grid;
    gap: 5px;
  }
  .notebook-choice small {
    display: block;
    color: var(--color-t-mid);
    margin-top: 8px;
  }
  .choice-actions {
    display: flex;
    flex-wrap: wrap;
    gap: 8px;
    margin-top: 10px;
  }
  .choice-actions button {
    border: 1px solid var(--color-line);
    border-radius: 6px;
    padding: 7px 10px;
    color: var(--color-t-hi);
    background: var(--color-ink-3);
    cursor: pointer;
  }
  .choice-actions button:disabled {
    opacity: 0.5;
    cursor: default;
  }
  .notebook-bar {
    display: flex;
    align-items: center;
    flex-wrap: wrap;
    gap: 5px 10px;
    padding: 8px 16px;
    border-bottom: 1px solid #ffffff12;
    color: #aeb5c0;
    font-size: 11px;
  }
  .notebook-bar button {
    color: #cbd3dd;
    background: transparent;
    border: 0;
    cursor: pointer;
  }
  .notebook-bar button:disabled {
    opacity: 0.4;
    cursor: default;
  }
  .notebook-error {
    padding: 8px 16px;
    font-size: 12px;
    color: #f3b9a9;
  }
  .overlay-root {
    width: 100vw;
    height: 100vh;
    padding: 12px;
    box-sizing: border-box;
    display: flex;
    font-family: var(--font-body);
    color: var(--color-t-hi);
    background: transparent;
  }
  * {
    box-sizing: border-box;
  }
  .panel {
    flex: 1;
    min-height: 0;
    display: flex;
    flex-direction: column;
    border-radius: 16px;
    overflow: hidden;
    background: rgba(17, 17, 21, 0.9);
    backdrop-filter: blur(30px);
    border: 1px solid color-mix(in oklab, var(--accent) 22%, var(--color-line));
    box-shadow:
      0 24px 70px -20px rgba(0, 0, 0, 0.7),
      inset 0 1px 0 rgba(255, 255, 255, 0.04);
  }

  /* titlebar */
  .titlebar {
    display: flex;
    align-items: center;
    gap: 10px;
    padding: 13px 14px;
    cursor: move;
    user-select: none;
    border-bottom: 1px solid var(--color-line-2);
  }
  .logo {
    position: relative;
    width: 22px;
    height: 22px;
    flex-shrink: 0;
  }
  .logo::before {
    content: '';
    position: absolute;
    inset: 0;
    border-radius: 50%;
    background: radial-gradient(
      circle at 50% 38%,
      #fff 0%,
      color-mix(in oklab, var(--accent) 85%, white) 26%,
      var(--accent) 60%,
      color-mix(in oklab, var(--accent) 40%, transparent) 82%,
      transparent 100%
    );
    box-shadow: 0 0 16px -2px var(--accent);
  }
  .logo::after {
    content: '';
    position: absolute;
    width: 5px;
    height: 5px;
    border-radius: 50%;
    background: rgba(255, 255, 255, 0.9);
    top: 20%;
    right: 22%;
  }
  .wordmark {
    font-family: var(--font-display);
    font-weight: 700;
    font-size: 14px;
    letter-spacing: 0.14em;
    color: var(--color-t-hi);
  }
  .drag-chip {
    font-family: var(--font-mono);
    font-size: 9px;
    letter-spacing: 0.08em;
    color: var(--color-t-lo);
    padding: 2px 6px;
    border: 1px solid var(--color-line);
    border-radius: 6px;
  }
  .title-actions {
    margin-left: auto;
    display: flex;
    gap: 6px;
  }
  .icon-btn {
    width: 30px;
    height: 30px;
    display: grid;
    place-items: center;
    border-radius: 9px;
    border: 1px solid var(--color-line);
    background: rgba(255, 255, 255, 0.02);
    color: var(--color-t-mid);
    cursor: pointer;
  }
  .icon-btn:hover {
    color: var(--color-t-hi);
    background: rgba(255, 255, 255, 0.06);
  }

  /* detected game */
  .gamebar {
    display: flex;
    align-items: center;
    gap: 11px;
    padding: 12px 14px;
  }
  .game-tile {
    width: 34px;
    height: 34px;
    border-radius: 9px;
    flex-shrink: 0;
    background: linear-gradient(135deg, color-mix(in oklab, var(--accent) 55%, #17171b), #101013);
    box-shadow: inset 0 0 0 1px rgba(255, 255, 255, 0.08);
  }
  .game-tile.muted {
    background: var(--color-ink-2);
  }
  .game-meta {
    min-width: 0;
    display: flex;
    flex-direction: column;
    gap: 2px;
  }
  .game-title {
    font-weight: 600;
    font-size: 13.5px;
    color: var(--color-t-hi);
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .game-title.dim {
    color: var(--color-t-mid);
  }
  .game-exe {
    font-family: var(--font-mono);
    font-size: 10.5px;
    color: var(--color-t-lo);
  }
  .linked-pill {
    margin-left: auto;
    display: inline-flex;
    align-items: center;
    gap: 6px;
    padding: 4px 10px;
    border-radius: 999px;
    font-size: 11px;
    font-weight: 500;
    color: var(--accent);
    background: color-mix(in oklab, var(--accent) 14%, transparent);
    border: 1px solid color-mix(in oklab, var(--accent) 30%, transparent);
  }
  .linked-pill .d {
    width: 6px;
    height: 6px;
    border-radius: 50%;
    background: var(--accent);
    box-shadow: 0 0 6px var(--accent);
  }

  /* tabs + provider */
  .tabrow {
    position: relative;
    display: flex;
    align-items: center;
    gap: 10px;
    padding: 4px 14px 14px;
  }
  .tabs {
    display: flex;
    gap: 3px;
    padding: 3px;
    border-radius: 11px;
    background: var(--color-ink-2);
    border: 1px solid var(--color-line-2);
  }
  .tab {
    display: inline-flex;
    align-items: center;
    gap: 7px;
    padding: 7px 13px;
    border-radius: 8px;
    font-size: 12.5px;
    font-weight: 500;
    color: var(--color-t-mid);
    cursor: pointer;
    border: 0;
    background: transparent;
  }
  .tab.active {
    color: var(--accent);
    background: color-mix(in oklab, var(--accent) 14%, transparent);
    box-shadow: inset 0 0 0 1px color-mix(in oklab, var(--accent) 26%, transparent);
  }
  .provider-pill {
    margin-left: auto;
    display: inline-flex;
    align-items: center;
    gap: 8px;
    padding: 8px 12px;
    border-radius: 10px;
    background: var(--color-ink-2);
    border: 1px solid var(--color-line);
    color: var(--color-t-hi);
    font-size: 12.5px;
    font-weight: 500;
    cursor: pointer;
  }
  .provider-pill:disabled {
    cursor: default;
    opacity: 0.6;
  }
  .prov-dot {
    width: 7px;
    height: 7px;
    border-radius: 50%;
  }
  .caret {
    color: var(--color-t-mid);
    font-size: 10px;
  }

  /* provider dropdown */
  .dropdown {
    position: absolute;
    top: 46px;
    right: 14px;
    width: 232px;
    z-index: 20;
    background: var(--color-ink-1);
    border: 1px solid var(--color-line);
    border-radius: 13px;
    padding: 6px;
    box-shadow: 0 20px 50px -12px rgba(0, 0, 0, 0.75);
    animation: fade-up 0.14s ease both;
  }
  .dropdown-head {
    font-family: var(--font-mono);
    font-size: 9px;
    letter-spacing: 0.14em;
    color: var(--color-t-lo);
    text-transform: uppercase;
    padding: 8px 10px 7px;
  }
  .prov-row {
    display: flex;
    align-items: center;
    gap: 11px;
    width: 100%;
    padding: 9px 10px;
    border-radius: 9px;
    cursor: pointer;
    border: 0;
    background: transparent;
    text-align: left;
  }
  .prov-row:hover {
    background: rgba(255, 255, 255, 0.03);
  }
  .pdot {
    width: 8px;
    height: 8px;
    border-radius: 50%;
    flex-shrink: 0;
  }
  .pmeta {
    display: flex;
    flex-direction: column;
    gap: 1px;
  }
  .pname {
    font-size: 13px;
    font-weight: 500;
    color: var(--color-t-hi);
  }
  .pmodel {
    font-family: var(--font-mono);
    font-size: 10px;
    color: var(--color-t-lo);
  }
  .pcheck {
    margin-left: auto;
    color: var(--accent);
    display: grid;
    place-items: center;
  }

  /* chat body */
  .body {
    flex: 1;
    min-height: 0;
    display: flex;
    flex-direction: column;
  }
  .msglist {
    flex: 1;
    min-height: 0;
    overflow-y: auto;
    padding: 6px 14px 10px;
    display: flex;
    flex-direction: column;
    gap: 16px;
  }
  .msg {
    display: flex;
    gap: 10px;
    max-width: 100%;
  }
  .avatar {
    position: relative;
    width: 26px;
    height: 26px;
    border-radius: 50%;
    flex-shrink: 0;
    margin-top: 2px;
    background: radial-gradient(
      circle at 50% 38%,
      #fff 0%,
      color-mix(in oklab, var(--accent) 85%, white) 26%,
      var(--accent) 60%,
      transparent 100%
    );
    box-shadow: 0 0 14px -3px var(--accent);
  }
  .bubble {
    padding: 11px 14px;
    border-radius: 13px;
    font-size: 13.5px;
    line-height: 1.5;
    color: var(--color-t-hi);
    white-space: pre-wrap;
    word-break: break-word;
  }
  .msg.sage .bubble {
    background: var(--color-ink-2);
    border: 1px solid var(--color-line-2);
    border-top-left-radius: 5px;
  }
  .msg.user {
    flex-direction: column;
    align-items: flex-end;
  }
  .msg.user .bubble {
    background: color-mix(in oklab, var(--accent) 16%, var(--color-ink-3));
    border: 1px solid color-mix(in oklab, var(--accent) 24%, transparent);
    border-top-right-radius: 5px;
  }
  .meta {
    font-family: var(--font-mono);
    font-size: 10px;
    color: var(--color-t-lo);
    margin-top: 7px;
    letter-spacing: 0.04em;
  }
  .read-reply {
    margin-top: 7px;
    padding: 3px 0;
    color: var(--color-t-mid);
    font-size: 10px;
    cursor: pointer;
  }
  .read-reply:hover {
    color: var(--color-t-hi);
  }
  .frame-chip {
    display: inline-flex;
    align-items: center;
    gap: 7px;
    padding: 4px 9px 4px 4px;
    border-radius: 8px;
    background: var(--color-ink-3);
    border: 1px solid var(--color-line);
    font-family: var(--font-mono);
    font-size: 9px;
    letter-spacing: 0.04em;
    color: var(--color-t-mid);
    margin-bottom: 7px;
  }
  .frame-chip .thumb {
    width: 24px;
    height: 16px;
    border-radius: 4px;
    background: linear-gradient(135deg, color-mix(in oklab, var(--accent) 52%, #17171b), #101013);
  }
  .caret-blink {
    display: inline-block;
    width: 2px;
    height: 0.95em;
    background: var(--accent);
    margin-left: 2px;
    vertical-align: text-bottom;
    animation: blink 1s steps(2) infinite;
  }
  @keyframes blink {
    0%,
    100% {
      opacity: 1;
    }
    50% {
      opacity: 0;
    }
  }
  .thinking {
    display: inline-flex;
    gap: 4px;
    align-items: center;
  }
  .thinking i {
    width: 5px;
    height: 5px;
    border-radius: 50%;
    background: var(--color-t-mid);
    animation: pulse-soft 1.2s ease-in-out infinite;
  }
  .thinking i:nth-child(2) {
    animation-delay: 0.18s;
  }
  .thinking i:nth-child(3) {
    animation-delay: 0.36s;
  }

  /* suggested prompt chips */
  .chips {
    display: flex;
    flex-wrap: wrap;
    gap: 8px;
    padding-left: 36px;
  }
  .chip {
    padding: 8px 12px;
    border-radius: 10px;
    font-size: 12px;
    color: var(--color-t-mid);
    background: rgba(255, 255, 255, 0.02);
    border: 1px solid var(--color-line);
    cursor: pointer;
  }
  .chip:hover {
    color: var(--color-t-hi);
    border-color: color-mix(in oklab, var(--accent) 34%, transparent);
  }

  /* input row + footer */
  .inputbar {
    padding: 10px 14px 8px;
    border-top: 1px solid var(--color-line-2);
  }
  .inputrow {
    display: flex;
    align-items: center;
    gap: 9px;
  }
  .attach-btn {
    width: 40px;
    height: 40px;
    flex-shrink: 0;
    display: grid;
    place-items: center;
    border-radius: 11px;
    border: 1px solid color-mix(in oklab, var(--accent) 40%, transparent);
    background: color-mix(in oklab, var(--accent) 12%, transparent);
    color: var(--accent);
    cursor: pointer;
  }
  .attach-btn.off {
    border-color: var(--color-line);
    background: rgba(255, 255, 255, 0.02);
    color: var(--color-t-mid);
  }
  .attach-btn:disabled {
    opacity: 0.4;
    cursor: default;
  }
  .text-input {
    flex: 1;
    min-width: 0;
    height: 40px;
    border-radius: 11px;
    border: 1px solid var(--color-line);
    background: var(--color-ink-2);
    color: var(--color-t-hi);
    font-family: var(--font-body);
    font-size: 13px;
    padding: 0 14px;
    outline: none;
  }
  .text-input::placeholder {
    color: var(--color-t-lo);
  }
  .text-input:disabled {
    opacity: 0.6;
  }
  .send-btn {
    width: 40px;
    height: 40px;
    flex-shrink: 0;
    display: grid;
    place-items: center;
    border-radius: 11px;
    border: 0;
    background: var(--accent);
    color: #0b0b0d;
    cursor: pointer;
  }
  .send-btn:disabled {
    opacity: 0.45;
    cursor: default;
  }
  .footer {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 10px;
    padding: 8px 2px 2px;
    font-family: var(--font-mono);
    font-size: 10px;
    color: var(--color-t-lo);
  }

  /* translate view */
  .translate {
    padding: 4px 14px 14px;
    gap: 14px;
  }
  .capture-box {
    border-radius: 13px;
    border: 1px solid var(--color-line);
    background: linear-gradient(
      135deg,
      color-mix(in oklab, var(--accent) 10%, var(--color-ink-2)),
      var(--color-ink-1)
    );
    padding: 12px;
  }
  .capture-head {
    font-family: var(--font-mono);
    font-size: 9.5px;
    letter-spacing: 0.06em;
    color: var(--color-t-mid);
    margin-bottom: 10px;
  }
  .capture-frame {
    height: 74px;
    border-radius: 9px;
    border: 1px dashed color-mix(in oklab, var(--accent) 45%, transparent);
    background: rgba(0, 0, 0, 0.18);
  }
  .lang-row {
    display: flex;
    align-items: center;
    gap: 10px;
  }
  .lang-chip {
    padding: 8px 12px;
    border-radius: 10px;
    font-size: 12px;
    color: var(--color-t-mid);
    background: var(--color-ink-2);
    border: 1px solid var(--color-line);
  }
  .lang-chip.accent {
    color: var(--accent);
    border-color: color-mix(in oklab, var(--accent) 32%, transparent);
    background: color-mix(in oklab, var(--accent) 12%, transparent);
  }
  .lang-arrow {
    color: var(--color-t-lo);
  }
  .translate-empty {
    flex: 1;
    display: flex;
    flex-direction: column;
    align-items: center;
    justify-content: center;
    text-align: center;
    gap: 6px;
  }
  .te-title {
    font-size: 13.5px;
    color: var(--color-t-mid);
  }
  .te-sub {
    font-size: 12px;
    color: var(--color-t-lo);
  }
  .translate-actions {
    display: flex;
    justify-content: center;
    gap: 10px;
  }
  .recapture {
    padding: 9px 16px;
    border-radius: 11px;
    border: 1px solid var(--color-line);
    background: var(--color-ink-2);
    color: var(--color-t-mid);
    font-size: 12.5px;
    font-weight: 500;
    cursor: not-allowed;
    opacity: 0.7;
  }
  .recapture.live {
    cursor: pointer;
    opacity: 1;
    color: var(--color-t-hi);
  }
  .recapture.live:hover {
    border-color: color-mix(in oklab, var(--accent) 34%, transparent);
  }
  .recapture.live:disabled {
    cursor: default;
    opacity: 0.45;
  }
  .translate-result {
    flex: 1;
    min-height: 0;
    overflow-y: auto;
    display: flex;
    flex-direction: column;
  }
  .translate-text {
    font-size: 13.5px;
    line-height: 1.55;
    color: var(--color-t-hi);
    white-space: pre-wrap;
    word-break: break-word;
  }
  .capture-frame.busy {
    animation: pulse-soft 1.4s ease-in-out infinite;
  }
</style>
