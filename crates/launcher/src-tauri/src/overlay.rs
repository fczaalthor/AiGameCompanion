//! External (no-injection) overlay companion: foreground-game detection and the
//! show/focus/hide state machine driven by the global toggle hotkey.
//!
//! The Win32 specifics compile only on Windows; on other hosts (the launcher's
//! pure-logic tests run on Linux) the helpers degrade to no-ops so the crate
//! still builds.

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager};

/// Snapshot of the foreground game window at the moment the overlay was opened.
#[derive(Clone, Debug, Default, Serialize)]
pub struct GameInfo {
    /// Native window handle, stored as i64 so it crosses the serde/IPC boundary.
    pub hwnd: i64,
    pub pid: u32,
    pub exe: String,
    pub title: String,
}

/// Remembers the game window that had focus before the overlay was shown, so
/// focus can be handed back when the overlay hides.
#[derive(Default)]
pub struct OverlayState {
    pub game: parking_lot::Mutex<Option<GameInfo>>,
}

/// Capture the last foreground game window to a temporary PNG file.
#[tauri::command]
#[allow(clippy::needless_pass_by_value)] // Tauri command state is injected as an owned handle.
pub fn capture_game(app: AppHandle) -> Result<String, String> {
    let hwnd = app
        .state::<OverlayState>()
        .game
        .lock()
        .as_ref()
        .map(|game| game.hwnd)
        .ok_or_else(|| "no game detected -- open the overlay over a game first".to_owned())?;

    let png = crate::overlay_capture::capture_window_png(hwnd)?;
    let byte_count = png.len();
    let path = std::env::temp_dir().join("sage-capture.png");
    std::fs::write(&path, png)
        .map_err(|error| format!("failed to write {}: {error}", path.display()))?;
    Ok(format!("captured {byte_count} bytes -> {}", path.display()))
}

/// Toggle the overlay window hidden <-> interactive. On hide, hand focus back to
/// the stored game.
pub fn toggle(app: &AppHandle) {
    let Some(overlay) = app.get_webview_window("overlay") else {
        return;
    };

    if overlay.is_visible().unwrap_or(false) {
        let _ = hide_to_game(app);
    } else {
        show_overlay(app);
    }
}

/// Hide the panel and return control to the captured game window. Used by the
/// hotkey, the panel's Hide button, and the speech handoff.
pub fn hide_to_game(app: &AppHandle) -> Result<(), String> {
    let overlay = app
        .get_webview_window("overlay")
        .ok_or_else(|| "Overlay window is unavailable.".to_owned())?;
    overlay.hide().map_err(|error| error.to_string())?;
    if let Some(state) = app.try_state::<OverlayState>() {
        if let Some(game) = state.game.lock().clone() {
            focus_window(game.hwnd);
        }
    }
    Ok(())
}

#[tauri::command]
#[allow(clippy::needless_pass_by_value)]
pub fn hide_overlay_to_game(app: AppHandle) -> Result<(), String> {
    hide_to_game(&app)
}

/// The quick-ask path returns focus and sends one Escape to close the pause
/// menu that many games open when Speechify briefly takes the foreground.
#[tauri::command]
#[allow(clippy::needless_pass_by_value)]
pub async fn hide_overlay_and_resume_game(app: AppHandle) -> Result<(), String> {
    hide_to_game(&app)?;
    let game = app
        .state::<OverlayState>()
        .game
        .lock()
        .clone()
        .ok_or_else(|| "No game window is available to resume.".to_owned())?;
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    #[cfg(windows)]
    {
        imp::resume_game(game.hwnd)
    }
    #[cfg(not(windows))]
    {
        let _ = game;
        Err("Automatic game resume is only available on Windows.".to_owned())
    }
}

/// The quick-ask hotkey captures the foreground game without opening the
/// overlay. The hidden webview can run the Codex request while play continues.
pub fn quick_ask(app: &AppHandle) {
    let Some(overlay) = app.get_webview_window("overlay") else {
        return;
    };
    let game = if overlay.is_visible().unwrap_or(false) {
        let remembered = app
            .try_state::<OverlayState>()
            .and_then(|state| state.game.lock().clone());
        let _ = hide_to_game(app);
        remembered
    } else {
        foreground_game(std::process::id())
    };
    if let Some(state) = app.try_state::<OverlayState>() {
        (*state.game.lock()).clone_from(&game);
    }
    let _ = app.emit_to("overlay", "quick-ask", game);
}

/// Speechify reads selected text in the foreground webview. Show it only once
/// the answer is ready, without replacing the game captured by `quick_ask`.
#[tauri::command]
#[allow(clippy::needless_pass_by_value)]
pub fn show_overlay_for_speech(app: AppHandle) -> Result<(), String> {
    let overlay = app
        .get_webview_window("overlay")
        .ok_or_else(|| "Overlay window is unavailable.".to_owned())?;
    overlay.show().map_err(|error| error.to_string())?;
    overlay.set_focus().map_err(|error| error.to_string())
}

/// Speechify's installed Windows app reads the current text selection with
/// Left Alt+A. The frontend selects only the completed answer before calling.
#[tauri::command]
#[allow(clippy::needless_pass_by_value)]
pub async fn speak_selected_reply(app: AppHandle) -> Result<(), String> {
    let overlay = app
        .get_webview_window("overlay")
        .ok_or_else(|| "Overlay window is unavailable.".to_owned())?;
    if !overlay.is_visible().unwrap_or(false) {
        return Err("Open the overlay before reading a reply.".to_owned());
    }
    overlay.set_focus().map_err(|error| error.to_string())?;
    tokio::time::sleep(std::time::Duration::from_millis(80)).await;
    #[cfg(windows)]
    {
        // Tauri and this crate currently depend on different `windows` crate
        // versions. Their HWND wrappers contain the same raw Win32 handle.
        let hwnd = overlay.hwnd().map_err(|error| error.to_string())?;
        imp::send_speechify_shortcut(windows::Win32::Foundation::HWND(hwnd.0))
    }
    #[cfg(not(windows))]
    {
        Err("Speechify's Windows shortcut is only available on Windows.".to_owned())
    }
}

/// Show the overlay (if hidden) and fire an action event to the overlay UI, e.g.
/// `translate-request` or `quick-ask` from a global hotkey. When already visible,
/// keep the stored game HWND (re-detecting would find the overlay itself).
pub fn trigger(app: &AppHandle, event: &str) {
    let Some(overlay) = app.get_webview_window("overlay") else {
        return;
    };
    if !overlay.is_visible().unwrap_or(false) {
        show_overlay(app);
    }
    let _ = app.emit_to("overlay", event, ());
}

/// Capture the current foreground window (the game) BEFORE the overlay steals
/// focus, store it, then show + focus the overlay and report detection to the UI.
fn show_overlay(app: &AppHandle) {
    let Some(overlay) = app.get_webview_window("overlay") else {
        return;
    };
    let game = foreground_game(std::process::id());
    if let Some(state) = app.try_state::<OverlayState>() {
        (*state.game.lock()).clone_from(&game);
    }
    let _ = overlay.show();
    let _ = overlay.set_focus();
    // A null payload tells the overlay UI "no game detected".
    let _ = app.emit_to("overlay", "overlay-status", game);
}

#[cfg(windows)]
fn foreground_game(self_pid: u32) -> Option<GameInfo> {
    imp::foreground_game(self_pid)
}

#[cfg(not(windows))]
fn foreground_game(_self_pid: u32) -> Option<GameInfo> {
    None
}

#[cfg(windows)]
fn focus_window(hwnd: i64) {
    imp::focus_window(hwnd);
}

#[cfg(not(windows))]
fn focus_window(_hwnd: i64) {}

#[cfg(windows)]
mod imp {
    use super::GameInfo;
    use windows::core::PWSTR;
    use windows::Win32::Foundation::{CloseHandle, HWND};
    use windows::Win32::System::Threading::{
        OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
        PROCESS_QUERY_LIMITED_INFORMATION,
    };
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYBD_EVENT_FLAGS, KEYEVENTF_KEYUP,
        VK_A, VK_ESCAPE, VK_LMENU,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        GetForegroundWindow, GetWindowTextW, GetWindowThreadProcessId, SetForegroundWindow,
    };

    pub fn foreground_game(self_pid: u32) -> Option<GameInfo> {
        unsafe {
            let hwnd = GetForegroundWindow();
            if hwnd.0.is_null() {
                return None;
            }
            let mut pid = 0u32;
            GetWindowThreadProcessId(hwnd, Some(&raw mut pid));
            if pid == 0 || pid == self_pid {
                return None;
            }
            let exe = exe_path(pid).unwrap_or_default();
            let mut buf = [0u16; 512];
            let n = GetWindowTextW(hwnd, &mut buf);
            let title = String::from_utf16_lossy(&buf[..usize::try_from(n).unwrap_or(0)]);
            Some(GameInfo {
                hwnd: hwnd.0 as i64,
                pid,
                exe,
                title,
            })
        }
    }

    unsafe fn exe_path(pid: u32) -> Option<String> {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buf = [0u16; 1024];
        let mut len = u32::try_from(buf.len()).unwrap_or(0);
        let res = QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_WIN32,
            PWSTR(buf.as_mut_ptr()),
            &raw mut len,
        );
        let _ = CloseHandle(handle);
        res.ok()?;
        Some(String::from_utf16_lossy(&buf[..len as usize]))
    }

    pub fn focus_window(hwnd: i64) {
        unsafe {
            let handle = usize::try_from(hwnd).unwrap_or(0) as *mut core::ffi::c_void;
            let _ = SetForegroundWindow(HWND(handle));
        }
    }

    pub fn resume_game(hwnd: i64) -> Result<(), String> {
        let target = HWND(usize::try_from(hwnd).unwrap_or(0) as *mut core::ffi::c_void);
        if unsafe { GetForegroundWindow() } != target {
            unsafe { SetForegroundWindow(target) }
                .ok()
                .map_err(|error| format!("Could not focus the game: {error}"))?;
        }
        if unsafe { GetForegroundWindow() } != target {
            return Err("Game did not regain focus; Escape was not sent.".to_owned());
        }
        let key = |flags| INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: VK_ESCAPE,
                    dwFlags: flags,
                    ..Default::default()
                },
            },
        };
        let events = [key(KEYBD_EVENT_FLAGS::default()), key(KEYEVENTF_KEYUP)];
        let input_size = i32::try_from(std::mem::size_of::<INPUT>())
            .map_err(|_| "Windows keyboard input size is invalid.".to_owned())?;
        if unsafe { SendInput(&events, input_size) } != 2 {
            return Err("Windows did not deliver Escape to the game.".to_owned());
        }
        Ok(())
    }

    pub fn send_speechify_shortcut(overlay: HWND) -> Result<(), String> {
        // SendInput targets the foreground window. Never inject this shortcut
        // after a focus change to another app (or while the user is typing).
        if unsafe { GetForegroundWindow() } != overlay {
            return Err("Overlay lost focus before Speechify could read the reply.".to_owned());
        }
        let key = |vk, flags| INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: vk,
                    dwFlags: flags,
                    ..Default::default()
                },
            },
        };
        let events = [
            key(VK_LMENU, KEYBD_EVENT_FLAGS::default()),
            key(VK_A, KEYBD_EVENT_FLAGS::default()),
            key(VK_A, KEYEVENTF_KEYUP),
            key(VK_LMENU, KEYEVENTF_KEYUP),
        ];
        let input_size = i32::try_from(std::mem::size_of::<INPUT>())
            .map_err(|_| "Windows keyboard input size is invalid.".to_owned())?;
        let expected = u32::try_from(events.len())
            .map_err(|_| "Windows keyboard event count is invalid.".to_owned())?;
        let sent = unsafe { SendInput(&events, input_size) };
        if sent != expected {
            return Err("Windows did not deliver Speechify's reading shortcut.".to_owned());
        }
        Ok(())
    }
}
