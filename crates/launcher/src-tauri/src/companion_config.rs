//! User-editable prompts and global shortcuts, independent of provider secrets.

use std::path::{Path, PathBuf};

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager};
use tauri_plugin_global_shortcut::{GlobalShortcutExt, Shortcut};
use tauri_plugin_opener::OpenerExt;

const TEMPLATE: &str = include_str!("../../../../companion.example.toml");
const MAX_CONFIG_BYTES: u64 = 65_536;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Instructions {
    pub system_prompt: String,
    pub quick_ask_prompt: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Hotkeys {
    pub toggle_overlay: String,
    pub translate: String,
    pub quick_ask: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Speech {
    pub auto_resume: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct SessionLimits {
    pub max_image_turns: usize,
    pub max_turns: usize,
    pub handoff_messages: usize,
    pub handoff_chars: usize,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct NotebookSettings {
    /// Explicit project selection also works across multiple desktop apps.
    pub project: String,
    pub brief_chars: usize,
    pub reference_chars: usize,
    pub checkpoint_chars: usize,
    pub revisions: usize,
}

impl Default for NotebookSettings {
    fn default() -> Self {
        Self {
            project: String::new(),
            brief_chars: 12_000,
            reference_chars: 8_000,
            checkpoint_chars: 12_000,
            revisions: 20,
        }
    }
}

impl Default for SessionLimits {
    fn default() -> Self {
        Self {
            max_image_turns: 8,
            max_turns: 16,
            handoff_messages: 12,
            handoff_chars: 16_000,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CompanionConfig {
    pub instructions: Instructions,
    pub hotkeys: Hotkeys,
    pub speech: Speech,
    #[serde(default)]
    pub sessions: SessionLimits,
    #[serde(default)]
    pub notebook: NotebookSettings,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    ToggleOverlay,
    Translate,
    QuickAsk,
}

impl CompanionConfig {
    fn defaults() -> Self {
        toml::from_str(TEMPLATE).expect("embedded companion config must be valid")
    }

    fn shortcuts(&self) -> Result<Vec<(Action, Shortcut)>, String> {
        let mut parsed = Vec::new();
        for (action, name, chord) in [
            (
                Action::ToggleOverlay,
                "toggle_overlay",
                &self.hotkeys.toggle_overlay,
            ),
            (Action::Translate, "translate", &self.hotkeys.translate),
            (Action::QuickAsk, "quick_ask", &self.hotkeys.quick_ask),
        ] {
            if chord.trim().is_empty() {
                continue;
            }
            let shortcut = chord
                .trim()
                .parse::<Shortcut>()
                .map_err(|e| format!("hotkeys.{name}: {e}"))?;
            if parsed.iter().any(|(_, existing)| *existing == shortcut) {
                return Err(format!(
                    "hotkeys.{name}: {chord} is assigned to more than one action."
                ));
            }
            parsed.push((action, shortcut));
        }
        Ok(parsed)
    }
}

fn parse_config(text: &str) -> Result<CompanionConfig, String> {
    if text.len() as u64 > MAX_CONFIG_BYTES {
        return Err("companion.toml must be no larger than 64 KiB.".to_owned());
    }
    // Windows editors may prepend a UTF-8 BOM.
    let mut config: CompanionConfig = toml::from_str(text.trim_start_matches('\u{feff}'))
        .map_err(|e| format!("Invalid companion.toml: {e}"))?;
    for (name, prompt) in [
        ("system_prompt", &mut config.instructions.system_prompt),
        (
            "quick_ask_prompt",
            &mut config.instructions.quick_ask_prompt,
        ),
    ] {
        *prompt = prompt.trim().to_owned();
        if prompt.is_empty() {
            return Err(format!("instructions.{name} must not be empty."));
        }
    }
    config.shortcuts()?;
    crate::notebook::validate_project(&config.notebook.project)?;
    for (name, value, minimum, maximum) in [
        ("brief_chars", config.notebook.brief_chars, 256, 256_000),
        (
            "reference_chars",
            config.notebook.reference_chars,
            256,
            256_000,
        ),
        (
            "checkpoint_chars",
            config.notebook.checkpoint_chars,
            512,
            256_000,
        ),
        ("revisions", config.notebook.revisions, 1, 100),
    ] {
        if !(minimum..=maximum).contains(&value) {
            return Err(format!(
                "notebook.{name} must be between {minimum} and {maximum}."
            ));
        }
    }
    for (name, value, minimum, maximum) in [
        ("max_image_turns", config.sessions.max_image_turns, 1, 1_000),
        ("max_turns", config.sessions.max_turns, 1, 1_000),
        (
            "handoff_messages",
            config.sessions.handoff_messages,
            1,
            2_000,
        ),
        (
            "handoff_chars",
            config.sessions.handoff_chars,
            256,
            1_000_000,
        ),
    ] {
        if !(minimum..=maximum).contains(&value) {
            return Err(format!(
                "sessions.{name} must be between {minimum} and {maximum}."
            ));
        }
    }
    Ok(config)
}

fn read_config(path: &Path) -> Result<CompanionConfig, String> {
    let meta = std::fs::metadata(path).map_err(|e| format!("{}: {e}", path.display()))?;
    if meta.len() > MAX_CONFIG_BYTES {
        return Err("companion.toml must be no larger than 64 KiB.".to_owned());
    }
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    parse_config(&text)
}

/// Acquire replacement chords before releasing old ones. Swapping actions that
/// use the same chord set requires no OS registration changes.
fn sync_shortcuts(
    registered: &mut Vec<Shortcut>,
    desired: &[Shortcut],
    update: &mut impl FnMut(Shortcut, bool) -> Result<(), String>,
) -> Result<(), String> {
    for shortcut in desired {
        if !registered.contains(shortcut) {
            update(*shortcut, true)?;
            registered.push(*shortcut);
        }
    }
    for shortcut in registered.clone() {
        if !desired.contains(&shortcut) {
            update(shortcut, false)?;
            registered.retain(|key| *key != shortcut);
        }
    }
    Ok(())
}

fn rebind_shortcuts(
    registered: &mut Vec<Shortcut>,
    desired: &[Shortcut],
    mut update: impl FnMut(Shortcut, bool) -> Result<(), String>,
) -> Result<(), String> {
    let previous = registered.clone();
    if let Err(error) = sync_shortcuts(registered, desired, &mut update) {
        return match sync_shortcuts(registered, &previous, &mut update) {
            Ok(()) => Err(format!("Config was not applied; previous settings retained. {error}")),
            Err(rollback) => Err(format!("Config was not applied. {error} Could not fully restore shortcuts: {rollback}. Restart the app after correcting the config.")),
        };
    }
    Ok(())
}

fn update_shortcut(app: &AppHandle, shortcut: Shortcut, register: bool) -> Result<(), String> {
    let result = if register {
        app.global_shortcut().register(shortcut)
    } else {
        app.global_shortcut().unregister(shortcut)
    };
    result.map_err(|e| format!("Shortcut {shortcut}: {e}"))
}

struct RuntimeConfig {
    config: CompanionConfig,
    actions: Vec<(Action, Shortcut)>,
    registered: Vec<Shortcut>,
    error: Option<String>,
}

pub struct CompanionState {
    path: PathBuf,
    current: Mutex<RuntimeConfig>,
    // Never hold `current` during a plugin call: it can wait on the UI thread,
    // which also dispatches hotkeys and reads the current action map.
    reload_lock: Mutex<()>,
}

#[derive(Clone, Serialize)]
pub struct ConfigStatus {
    path: String,
    config: CompanionConfig,
    error: Option<String>,
}

impl CompanionState {
    pub fn initialize(app: &AppHandle, path: PathBuf) -> Self {
        let loaded = (|| {
            if !path.exists() {
                std::fs::write(&path, TEMPLATE).map_err(|e| format!("{}: {e}", path.display()))?;
            }
            read_config(&path)
        })();
        let (config, mut error) = match loaded {
            Ok(config) => (config, None),
            Err(error) => (
                CompanionConfig::defaults(),
                Some(format!("Using defaults. {error}")),
            ),
        };
        let actions = config.shortcuts().expect("validated shortcuts");
        let mut registered = Vec::new();
        for (_, shortcut) in &actions {
            match update_shortcut(app, *shortcut, true) {
                Ok(()) => registered.push(*shortcut),
                Err(message) => {
                    error = Some(
                        error.map_or_else(|| message.clone(), |old| format!("{old}\n{message}")),
                    );
                }
            }
        }
        if let Some(error) = &error {
            tracing::warn!("{error}");
        }
        Self {
            path,
            current: Mutex::new(RuntimeConfig {
                config,
                actions,
                registered,
                error,
            }),
            reload_lock: Mutex::new(()),
        }
    }

    pub fn config(&self) -> CompanionConfig {
        self.current.lock().config.clone()
    }

    /// Select only after a human notebook decision. Keep comments, prompts and
    /// other edited settings byte-for-byte through `toml_edit`.
    pub fn select_notebook(&self, expected: &str, project: &str) -> Result<(), String> {
        let _reload = self.reload_lock.lock();
        crate::notebook::validate_project(project)?;
        let text = std::fs::read_to_string(&self.path).map_err(|e| e.to_string())?;
        let updated = with_notebook_project(&text, expected, project)?;
        let mut current = self.current.lock();
        if current.config.notebook.project != expected {
            return Err("Notebook selection changed while awaiting approval.".to_owned());
        }
        crate::notebook::atomic_write(&self.path, &updated)?;
        project.clone_into(&mut current.config.notebook.project);
        Ok(())
    }

    pub fn action(&self, shortcut: &Shortcut) -> Option<Action> {
        self.current
            .lock()
            .actions
            .iter()
            .find_map(|(action, key)| (key == shortcut).then_some(*action))
    }

    fn status(&self) -> ConfigStatus {
        let current = self.current.lock();
        ConfigStatus {
            path: self.path.to_string_lossy().into_owned(),
            config: current.config.clone(),
            error: current.error.clone(),
        }
    }

    fn reload(&self, app: &AppHandle) -> Result<(), String> {
        let _reload = self.reload_lock.lock();
        let result = (|| {
            let config = read_config(&self.path)?;
            let actions = config.shortcuts()?;
            let desired: Vec<_> = actions.iter().map(|(_, shortcut)| *shortcut).collect();
            let mut registered = self.current.lock().registered.clone();
            let result = rebind_shortcuts(&mut registered, &desired, |key, register| {
                update_shortcut(app, key, register)
            });
            let mut current = self.current.lock();
            current.registered = registered;
            result?;
            current.config = config;
            current.actions = actions;
            Ok(())
        })();
        self.current.lock().error = result.as_ref().err().cloned();
        result
    }
}

fn with_notebook_project(text: &str, expected: &str, project: &str) -> Result<String, String> {
    if read_project(text)? != expected {
        return Err(
            "Notebook selection was edited in companion.toml. Reload the config before choosing."
                .to_owned(),
        );
    }
    let mut document = text
        .trim_start_matches('\u{feff}')
        .parse::<toml_edit::DocumentMut>()
        .map_err(|e| e.to_string())?;
    document["notebook"]["project"] = toml_edit::value(project);
    let updated = document.to_string();
    parse_config(&updated)?;
    Ok(updated)
}

fn read_project(text: &str) -> Result<String, String> {
    Ok(parse_config(text)?.notebook.project)
}

#[tauri::command]
#[allow(clippy::needless_pass_by_value)]
pub fn get_companion_config(state: tauri::State<'_, CompanionState>) -> ConfigStatus {
    state.status()
}

#[tauri::command]
#[allow(clippy::needless_pass_by_value)]
pub fn reload_companion_config(app: AppHandle) -> Result<ConfigStatus, String> {
    let state = app.state::<CompanionState>();
    let result = state.reload(&app);
    let status = state.status();
    let _ = app.emit("companion-config-changed", &status);
    result?;
    crate::notebook::publish_status(&app);
    Ok(status)
}

#[tauri::command]
#[allow(clippy::needless_pass_by_value)]
pub fn open_companion_config(app: AppHandle) -> Result<(), String> {
    let state = app.state::<CompanionState>();
    app.opener()
        .open_path(state.path.to_string_lossy().as_ref(), None::<&str>)
        .map_err(|e| format!("Could not open companion.toml: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notebook_selection_preserves_prompts_comments_and_unsaved_edits() {
        let text = TEMPLATE.replace("project = \"\"", "project = \"desktop-apps\"");
        let updated = with_notebook_project(&text, "desktop-apps", "crystal-project").unwrap();
        assert!(updated.contains("# These Unicode character budgets are separate"));
        assert_eq!(
            parse_config(&updated).unwrap().instructions.system_prompt,
            parse_config(&text).unwrap().instructions.system_prompt
        );
        assert_eq!(read_project(&updated).unwrap(), "crystal-project");
        assert!(with_notebook_project(&text, "different", "crystal-project").is_err());
        assert!(with_notebook_project(&text, "desktop-apps", "../escape").is_err());
    }

    #[test]
    fn notebook_is_optional_and_its_own_budgets_are_validated() {
        let old_config = TEMPLATE.split("[notebook]").next().unwrap();
        assert!(parse_config(old_config)
            .unwrap()
            .notebook
            .project
            .is_empty());
        assert!(
            parse_config(&TEMPLATE.replace("project = \"\"", "project = \"../escape\"")).is_err()
        );
        assert!(parse_config(
            &TEMPLATE.replace("checkpoint_chars = 12000", "checkpoint_chars = 0")
        )
        .is_err());
        assert!(parse_config(&TEMPLATE.replace("revisions = 20", "revisions = 101")).is_err());
    }

    #[test]
    fn config_preserves_multiline_prompts_and_accepts_windows_bom() {
        let config = parse_config(&format!("\u{feff}{TEMPLATE}")).unwrap();
        assert!(config.instructions.system_prompt.contains('\n'));
        assert_eq!(config.hotkeys.quick_ask, "Ctrl+Shift+A");
        assert!(config.speech.auto_resume);
    }

    #[test]
    fn invalid_edits_are_rejected_and_disabled_chords_are_supported() {
        assert!(parse_config(&TEMPLATE.replace("system_prompt =", "system_promt =")).is_err());
        assert!(parse_config(&TEMPLATE.replace("Ctrl+Shift+A", "Control+Shift+G")).is_err());
        assert!(parse_config(&TEMPLATE.replace("Ctrl+Shift+G", "Ctrl+BogusKey")).is_err());
        let mut config = CompanionConfig::defaults();
        config.instructions.system_prompt.clear();
        assert!(parse_config(&toml::to_string(&config).unwrap()).is_err());
        config = CompanionConfig::defaults();
        config.hotkeys.translate.clear();
        assert_eq!(config.shortcuts().unwrap().len(), 2);
        assert!(parse_config(&TEMPLATE.replace("max_turns = 16", "max_turns = 0")).is_err());
        assert!(
            parse_config(&TEMPLATE.replace("handoff_chars = 16000", "handoff_chars = 12")).is_err()
        );
    }

    #[test]
    fn conflict_rolls_back_new_chords_without_releasing_old_chords() {
        let old = "Ctrl+Shift+G".parse::<Shortcut>().unwrap();
        let first = "Ctrl+Shift+B".parse::<Shortcut>().unwrap();
        let conflict = "Ctrl+Shift+C".parse::<Shortcut>().unwrap();
        let mut registered = vec![old];
        let mut calls = Vec::new();
        let result = rebind_shortcuts(&mut registered, &[first, conflict], |key, add| {
            calls.push((key, add));
            if key == conflict {
                Err("occupied".to_owned())
            } else {
                Ok(())
            }
        });
        assert!(result.unwrap_err().contains("previous settings retained"));
        assert_eq!(registered, vec![old]);
        assert_eq!(calls, vec![(first, true), (conflict, true), (first, false)]);
    }

    #[test]
    fn swapped_actions_need_no_registration_and_disabling_releases_keys() {
        let a = "Ctrl+Shift+A".parse::<Shortcut>().unwrap();
        let g = "Ctrl+Shift+G".parse::<Shortcut>().unwrap();
        let mut registered = vec![a, g];
        rebind_shortcuts(&mut registered, &[g, a], |_, _| {
            panic!("already registered")
        })
        .unwrap();
        rebind_shortcuts(&mut registered, &[g], |key, add| {
            assert_eq!(key, a);
            assert!(!add);
            Ok(())
        })
        .unwrap();
        assert_eq!(registered, vec![g]);
    }
}
