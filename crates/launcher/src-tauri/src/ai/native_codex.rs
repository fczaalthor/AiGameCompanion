//! Native Windows Codex discovery shared by availability checks and dispatch.

use std::path::{Path, PathBuf};

/// Preserve an existing PATH installation; otherwise discover the newest desktop
/// executable using the installer's file timestamp, not its opaque folder name.
/// Each call observes the filesystem anew, including removals during updates.
pub(super) fn resolve(
    path_dirs: impl IntoIterator<Item = PathBuf>,
    desktop_bin: Option<&Path>,
) -> Option<PathBuf> {
    if let Some(program) = path_dirs
        .into_iter()
        .filter(|dir| dir.is_absolute())
        .map(|dir| dir.join("codex.exe"))
        .find(|program| program.is_file())
    {
        return Some(program);
    }

    std::fs::read_dir(desktop_bin?)
        .ok()?
        .filter_map(Result::ok)
        .map(|entry| entry.path().join("codex.exe"))
        .filter_map(|program| {
            let metadata = program.metadata().ok()?;
            if !metadata.is_file() {
                return None;
            }
            Some((metadata.modified().ok()?, program))
        })
        .max()
        .map(|(_, program)| program)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{Duration, SystemTime};

    struct Fixture(PathBuf);

    impl Fixture {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let stamp = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let root = std::env::temp_dir().join(format!(
                "aigc-cli-update-{}-{stamp}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir(&root).unwrap();
            Self(root)
        }

        fn executable(&self, folder: &str, age: u64) -> PathBuf {
            let path = self.0.join(folder).join("codex.exe");
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            let file = std::fs::File::create(&path).unwrap();
            file.set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(age))
                .unwrap();
            path
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn finds_replacement_after_update_removes_inherited_path() {
        let fixture = Fixture::new();
        let old = fixture.executable("desktop/old", 100);
        let inherited = vec![old.parent().unwrap().to_path_buf()];
        let bin = fixture.0.join("desktop");
        assert_eq!(resolve(inherited.clone(), Some(&bin)), Some(old.clone()));

        std::fs::remove_file(old).unwrap();
        let new = fixture.executable("desktop/new", 200);
        assert_eq!(resolve(inherited, Some(&bin)), Some(new));
    }

    #[test]
    fn respects_explicit_path_order_before_desktop_installation() {
        let fixture = Fixture::new();
        let first = fixture.executable("custom first", 100);
        let second = fixture.executable("custom second", 200);
        fixture.executable("desktop/latest", 300);
        let dirs = vec![
            fixture.0.join("missing"),
            first.parent().unwrap().to_path_buf(),
            second.parent().unwrap().to_path_buf(),
        ];
        assert_eq!(resolve(dirs, Some(&fixture.0.join("desktop"))), Some(first));
    }

    #[test]
    fn chooses_newest_executable_and_ignores_incomplete_installs() {
        let fixture = Fixture::new();
        fixture.executable("desktop/zzz-old", 100);
        let latest = fixture.executable("desktop/aaa-new", 200);
        std::fs::create_dir_all(fixture.0.join("desktop/partial")).unwrap();
        std::fs::create_dir_all(fixture.0.join("desktop/directory/codex.exe")).unwrap();
        assert_eq!(
            resolve(Vec::new(), Some(&fixture.0.join("desktop"))),
            Some(latest)
        );
    }

    #[test]
    fn absence_is_not_a_cached_success() {
        let fixture = Fixture::new();
        let bin = fixture.0.join("desktop");
        assert_eq!(resolve(Vec::new(), Some(&bin)), None);
        let installed = fixture.executable("desktop/current", 100);
        assert_eq!(resolve(Vec::new(), Some(&bin)), Some(installed.clone()));
        std::fs::remove_file(installed).unwrap();
        assert_eq!(resolve(Vec::new(), Some(&bin)), None);
        assert_eq!(resolve(Vec::new(), None), None);
    }
}
