<div align="center">

# AI Game Companion

**Ask an AI while you play -- without alt-tabbing.**

A transparent, always-on-top companion window that composites over your game and
lets you ask Google Gemini, Claude, or OpenAI questions in-game. Bring your own
provider, attach a screenshot for context, or translate on-screen text -- all
without ever leaving the game, and **without injecting anything into it**.

[![Rust](https://img.shields.io/badge/rust-2021-DEA584?logo=rust&logoColor=white)](https://www.rust-lang.org)
[![Platform](https://img.shields.io/badge/platform-Windows%20x86__64-0078D4?logo=windows&logoColor=white)](#)
[![Tauri](https://img.shields.io/badge/Tauri-2-24C8DB?logo=tauri&logoColor=white)](https://tauri.app)
[![Gemini](https://img.shields.io/badge/Google_Gemini-free_tier-4285F4?logo=googlegemini&logoColor=white)](https://aistudio.google.com)
[![Claude](https://img.shields.io/badge/Claude-subscription-D97757?logo=anthropic&logoColor=white)](https://claude.ai/code)
[![OpenAI](https://img.shields.io/badge/OpenAI-subscription-412991?logo=openai&logoColor=white)](https://openai.com/codex/)
[![License](https://img.shields.io/badge/license-MIT-blue)](LICENSE)
[![Status](https://img.shields.io/badge/status-personal%20%C2%B7%20actively%20developed-brightgreen)](#status)

<br />

<img src="screenshots/launcher.png" alt="The Sage launcher, with an AI companion bound to a game" width="900" />

</div>

---

## Why

I built this for myself -- a way to ask an AI for a hint, a build, or a
translation while playing, without alt-tabbing to a browser and losing the
moment. Responses come from **Sage**, an in-game advisor drawn in a panel over
the running game.

Earlier versions injected a DLL and hooked the game's renderer. That was fragile
and looked like the thing anti-cheats flag, so it was **dropped entirely**. Sage
now runs as its **own transparent window** that Windows composites over the game
-- it never injects, never hooks a graphics API, and **cannot crash the game**.

It's deliberately **bring-your-own-AI**: Gemini talks to its own free API, while
Claude and OpenAI run through your *existing* CLI subscriptions -- no middleman
service, no shared keys. There's no telemetry, no analytics, and no account.

It's a personal tool, not a product -- open source under MIT. No adoption goal
and no support guarantees, but issues and PRs are read.

## Status

Actively developed personal tool, shipped as tagged Windows releases. It targets
**borderless / fullscreen-optimized** games (effectively all modern single-player
titles); genuine legacy exclusive-fullscreen games are out of scope. Treat it as
a working tool you can build and run, not a polished consumer app.

**Implemented:**
- External transparent overlay window -- topmost, click-through when idle, takes
  focus on demand; toggled with **Ctrl+Shift+G**. No injection, any graphics API.
- Multi-provider AI -- Gemini (direct API), Claude & OpenAI (through your own
  `claude` / `codex` CLIs), switchable from an in-panel dropdown that shows only
  available providers and persists your choice.
- Streaming "Sage" replies over a Tauri channel, multi-turn chat, Stop / New chat.
- Screenshot vision (Gemini, Claude, and OpenAI) via **Windows.Graphics.Capture**
  -- capture the game frame with no injection. OpenAI attaches the PNG through
  your ChatGPT-authenticated Codex CLI, including resumed turns.
- Screen translation (**Ctrl+Shift+T**) and quick-ask (**Ctrl+Shift+A**) hotkeys. Quick-ask captures and asks in the background, then briefly shows the reply for Speechify to read.
- Editable instructions, screenshot prompt, hotkeys, auto-resume, and Codex session
  limits in `companion.toml`, with Open / Reload controls in Settings -> Companion.
- Desktop launcher (Tauri 2 + Svelte 5) -- Steam library discovery, cover art,
  one-click launch, tray, launch-on-startup, and play-time via an external process
  watcher.
- In-app Settings: your Gemini key is stored in the **Windows Credential Manager**
  (not plaintext), plus CLI detection, a persisted default provider, and launcher
  toggles.

**Not done yet / out of scope:**
- Positioning the panel over the game's specific monitor (it opens centered).
- Offline / local-model translation -- translation currently runs through Gemini.
- Genuine legacy exclusive-fullscreen games -- an external window can't composite
  over those; borderless / FSO windowed is covered.
- Competitive / kernel-anti-cheat titles are a non-goal.

## Features

### The overlay
- A transparent, frameless, always-on-top panel Windows composites over the game
  -- no DLL, no swapchain hook, works regardless of graphics API.
- **Ctrl+Shift+G** toggles it; while interactive it takes keyboard focus so your
  typing doesn't reach the game, then hands focus back on hide.

<p align="center">
  <img src="screenshots/overlay.png" alt="The Sage overlay linked to a game, ready to answer questions about the screen" width="330" />
</p>

### Multi-provider AI
Sage can talk through **Gemini**, **Claude**, or **OpenAI** -- pick one from the
in-panel dropdown (only available providers are shown; the choice persists).

- **Gemini** -- direct API with a free key ([Google AI Studio](https://aistudio.google.com/apikey)),
  entered in Settings and stored in the OS Credential Manager.
- **Claude** -- your existing [Claude Code](https://claude.ai/code) CLI, no separate key.
- **OpenAI** -- your existing [Codex CLI](https://openai.com/codex/), no separate key.

The CLIs are spawned directly as subprocesses -- the same pattern documented for
[headless Claude Code](https://code.claude.com/docs/en/headless). No OAuth tokens
are extracted or shared; each user authenticates their own CLIs. Providers never
fall back to one another silently.

### Screenshot vision & translation
Attach the current frame to a question (Gemini / Claude / OpenAI) -- captured via
Windows.Graphics.Capture, no injection. Press **Ctrl+Shift+T** to translate
on-screen text through Gemini, or **Ctrl+Shift+A** to fire a preset question with
a screenshot attached. Quick-ask keeps the overlay hidden during inference.
When the answer is ready, it briefly focuses the reply so Speechify's
**Left Alt+A** shortcut can read only that answer, then hides the overlay.
The quick-ask handoff then returns focus to the captured game and sends one
Escape key. **Pause manually, then quick-ask, then auto-unpause.** The hotkey does
not pause first. Since Escape is a toggle, using quick-ask while unpaused can
pause the game at the end. Set `speech.auto_resume = false` to return focus
without Escape, including when capturing an ordinary desktop application window.

OpenAI uses `codex exec --json --image <PNG> -- -` for a fresh chat and
`codex exec resume <session-id> --json --image <PNG> -- -` for a follow-up,
with the prompt on stdin. The image option must follow `resume` and the explicit
session ID on resumed turns. Check `codex exec resume --help` on the same CLI the
launcher detects; it must advertise `--image`. No separate OpenAI API key is used.

The launcher resumes only its own matching conversation. It starts a fresh Codex
session after eight screenshots or sixteen turns by default, carrying a bounded text handoff
(up to the last twelve messages and 16,000 characters) and the current screenshot.
The newest question is always kept whole, even if it exceeds the text budget.
These limits are adjustable in `companion.toml`.
Earlier images are not reattached. This is recent text context, not a generated
long-term summary; details outside that window can be forgotten. New chat, a
changed game/context, or a failed/cancelled turn also prevents stale-session reuse.
Temporary PNG files are removed after the request. Codex's own saved sessions
follow the CLI's normal retention behavior.

Windows CLI mode receives a native absolute PNG path. WSL mode converts that same
path with `wslpath` in the detected distro; conversion errors are reported. Codex
requests have a five-minute total deadline, including quiet image inference;
there is no short timeout waiting for text. An explicitly requested capture that
fails is reported instead of silently sending a text-only question.

### Local reference files for Codex

Native Codex mode uses the read-only folder selected by `AIGC_REFERENCE_DIR` when
that environment variable names an existing directory. If it is unset, the
launcher uses `Desktop\\AI DOCS` when that folder exists; otherwise it falls back
to its empty temporary workspace. Sage may read relevant files but cannot modify
them: Codex is launched with `-s read-only` and `-a never`. The system prompt also
treats all file contents as reference data rather than instructions.

### Desktop launcher
A Tauri 2 + Svelte 5 GUI for your library: Steam auto-discovery, Steam-CDN cover
art, one-click launch, play-time tracking via an external process watcher, tray,
launch-on-startup, and an in-app Settings panel (provider key + detection,
default provider, editable companion config, launcher toggles).

### Editable companion config

On first launch, the app creates
`%APPDATA%\com.aigamecompanion.launcher\companion.toml` from
[`companion.example.toml`](companion.example.toml). The data folder is shared
across builds, so replacing or moving the executable keeps your edits.
Open **Settings -> Companion -> Open config**, save changes in your editor,
then click **Reload config**. Restarting also loads the file.

| Setting | Purpose |
|---|---|
| `instructions.system_prompt` | Standing instructions for chat; multiline TOML text |
| `instructions.quick_ask_prompt` | Question sent with each hotkey screenshot |
| `hotkeys.toggle_overlay`, `hotkeys.quick_ask`, `hotkeys.translate` | Global shortcuts; use `""` to disable an action |
| `speech.auto_resume` | Send one Escape after Speechify's existing Alt+A handoff; default `true` |
| `sessions.max_image_turns`, `sessions.max_turns` | Start a fresh Codex session after either limit; defaults `8` and `16` |
| `sessions.handoff_messages`, `sessions.handoff_chars` | Recent text retained at rollover; defaults `12` and `16000` |

Hotkeys use modifiers first and one key last, such as `Ctrl+Shift+A` or `Alt+F8`.
The app rejects duplicate chords, malformed keys, unknown setting names, empty
prompts, and invalid limits. Turn limits accept 1–1000, message count 1–2000,
and text budget 256–1,000,000 Unicode characters. The config file itself is
limited to 64 KiB. Increasing limits can increase response time and usage; it
does not increase the model's context window.

Reload applies hotkeys immediately and uses the new prompts and session limits
on the next request. An in-flight AI request keeps its original instructions.
Changing the standing instructions makes the next Codex request start a fresh
session with those instructions and the recent text handoff. Changing just a
hotkey does not reset the conversation. Lowering a turn limit below the current
count rolls over on the next request. Higher text budgets can retain more of the
current app chat, but cannot restore old screenshots or chat lost when the app closed.

A failed reload leaves the applied config intact and restores previous shortcuts
if a replacement chord is occupied. Errors appear in Settings, without popup
retry loops. At startup, an invalid file is preserved and defaults are used with
a Settings warning. Provider credentials remain in their
existing storage, and Codex model/reasoning settings stay in the Codex CLI config.

### Persistent project notebook

Set `notebook.project = "crystal-project"` in the `[notebook]` section of
`companion.toml` and reload config. An empty project disables it (the default).
Use a different project name for unrelated work; the app does not guess a project
from the window title. Notebook updates currently require the OpenAI/Codex provider.

Files live in `%APPDATA%\com.aigamecompanion.launcher\notebooks\<project>\`:

- **brief.md**: your experience, preferences, stable objectives and constraints.
  Edit it yourself; generated updates never replace it.
- **references.md**: local paths and what they contain. Only this index is included
  automatically; Codex reads relevant source files through its existing read-only tools.
- **checkpoint.json**: current objective, attributed observations, hypotheses,
  actual decisions, rejected options, open questions and next tests. It also records
  the latest question/answer for auditing, without reinjecting that raw turn.
- **history/**: previous checkpoint revisions, including the state before a reset.

The current brief, index and checkpoint accompany every fresh **and resumed** turn.
One Codex invocation returns the answer and a proposed checkpoint via
`--output-schema`; only the answer enters chat or Speechify. The app validates and
saves the checkpoint after successful completion. Codex retains its read-only
sandbox, ChatGPT login and configured model/reasoning effort. There is no separate
summarization request or API bill. With the notebook enabled, the answer appears
after the structured response finishes instead of displaying CLI commentary.

The installed CLI must advertise `--output-schema` in both `exec --help` and
`exec resume --help`. Schema and image options follow `resume SESSION`; temporary
schema/PNG paths both use `wslpath` in WSL mode. The five-minute total Codex deadline
also covers generating the checkpoint, with no automatic retry.

| Notebook setting | Default |
|---|---|
| `brief_chars` | 12000 Unicode characters |
| `reference_chars` | 8000 Unicode characters |
| `checkpoint_chars` | 12000 characters in the compact serialized checkpoint |
| `revisions` | 20 previous checkpoints |

These budgets are separate from the recent-chat handoff and rollover limits.
Brief/index budgets accept 256–256000, checkpoint 512–256000, revisions 1–100.
Oversize edits are reported without silently truncating them. An invalid or
oversize generated checkpoint keeps the previous saved version and shows a
notebook warning; a valid answer can still be displayed and spoken. Malformed
structured responses, failed/cancelled turns and superseded requests never save.
Edits detected during inference prevent that request from overwriting the notebook.

Click **Notebook** in the overlay, or **Settings -> Companion -> Open notebook folder**.
Saved file edits are read before the next request; **Reload** reads them immediately.
Changing the project, run, brief or index starts a fresh chat so old-run history
is not forwarded. Ordinary generated checkpoint updates preserve the current CLI
session until its configured rollover limit.

**New chat** keeps the notebook. **New run** clears mutable notes and chat while
preserving your brief and references. **Undo checkpoint** restores the immediately
previous saved checkpoint into a fresh chat; older files remain available up to
the revision limit. The checkpoint survives app restarts; the visible chat transcript
does not. Generated notes are editable summaries, not independently verified facts;
inspect attribution and correct mistakes in the files or your next question.

## Stack

| Layer | Choice | Notes |
|---|---|---|
| App | [Tauri 2][tauri] + [Svelte 5][svelte] + Tailwind 4 | One process, two windows: library + transparent overlay |
| Capture | [windows][windows] crate (WinRT) | Windows.Graphics.Capture -> D3D11 -> PNG; foreground-HWND game detection |
| Async / HTTP | [tokio][tokio] + [reqwest][reqwest] | Gemini streaming + CLI subprocesses off the UI thread |
| Shared state | [parking_lot][parking_lot] | Fast mutex for the shared app state |
| Secrets | [keyring][keyring] | Gemini key in the Windows Credential Manager |
| Build | [cargo-xwin][cargo-xwin] | MSVC cross-compile from WSL2 |
| Logging | [tracing][tracing] | Structured logs to `launcher.log` |

## Quick start

1. **Get a free Gemini key** at [Google AI Studio](https://aistudio.google.com/apikey)
   -- no billing required. (Or skip it and use your existing Claude / OpenAI CLI.)
2. **Run** `launcher.exe`, open **Settings -> Providers**, and paste your Gemini
   key (stored in the Windows Credential Manager). Claude / OpenAI are detected
   automatically if their CLIs are installed.
3. **Play.** With a game in the foreground, press **Ctrl+Shift+G** to open Sage,
   type a question, and optionally attach the current frame. **Ctrl+Shift+T**
   translates on-screen text; **Ctrl+Shift+A** asks a preset question.

## Layout

```
├── Cargo.toml                  # workspace root
├── crates/
│   └── launcher/               # the whole app (Tauri 2 + Svelte 5)
│       ├── src/                # Svelte frontend (runes): library + overlay UI
│       └── src-tauri/          # Rust backend: overlay window, WGC capture,
│                               #   in-process AI dispatch, Steam discovery, secrets
├── config.example.toml         # optional legacy key fallback (no real key)
└── scripts/build.sh            # release build -> release/
```

## Building from source

Built from WSL2 with [`cargo-xwin`][cargo-xwin] -- no native Visual Studio needed.

```bash
rustup target add x86_64-pc-windows-msvc
cargo install cargo-xwin
sudo apt install clang lld llvm

cargo xwin build -p launcher --target x86_64-pc-windows-msvc   # debug
./scripts/build.sh                                              # release -> release/
```

Lint + test gates:

```bash
cargo xwin clippy -p launcher --target x86_64-pc-windows-msvc -- -D warnings
cargo test -p launcher    # pure-logic tests; run on the Linux host
```

## Troubleshooting

- **Overlay doesn't appear over the game** -- use borderless / fullscreen-windowed
  mode; a topmost window can't composite over true exclusive fullscreen.
- **Claude / OpenAI missing from the dropdown** -- their CLIs may still be
  detecting (especially under WSL); open Settings and press **Re-check CLIs**.
- **Screenshot is black** -- some protected/DRM content or minimized windows can't
  be captured via Windows.Graphics.Capture; try borderless.
- **Gemini errors** -- "invalid key" re-enter it in Settings; "rate limited" the
  free tier allows roughly 250 requests/day.

## Design principles

1. **Can't crash the game.** Sage is a separate window, not injected code -- a bug
   in the companion never takes the game down with it.
2. **Bring your own AI, no middleman.** Gemini talks direct; Claude / OpenAI spawn
   your own authenticated CLIs. No tokens extracted, no shared service.
3. **No silent fallback.** Providers never switch behind your back -- a failed
   request fails loudly rather than leaking your prompt to a different vendor.
4. **Secrets stay secret.** The Gemini key lives in the OS Credential Manager,
   never plaintext; no telemetry or analytics anywhere.

## License

[MIT](LICENSE). Release notes live in [CHANGELOG.md](CHANGELOG.md).

---

<sub>AI Game Companion is a personal tool -- built for my own use, with no
telemetry or analytics. It runs as a separate transparent window composited over
the game: it does not inject code, read or modify game memory, or touch network
traffic or game logic. You are responsible for complying with each AI provider's
terms of service. Not chasing adoption, but if it looks useful, you're welcome to
try it.</sub>

[tauri]:       https://tauri.app
[svelte]:      https://svelte.dev
[windows]:     https://github.com/microsoft/windows-rs
[tokio]:       https://tokio.rs
[reqwest]:     https://docs.rs/reqwest
[parking_lot]: https://docs.rs/parking_lot
[keyring]:     https://docs.rs/keyring
[cargo-xwin]:  https://github.com/rust-cross/cargo-xwin
[tracing]:     https://docs.rs/tracing
