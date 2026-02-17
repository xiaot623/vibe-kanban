use std::{
    env, fs,
    io::{self, ErrorKind},
    path::{Path, PathBuf},
};

const SKIP_PATH_BOOTSTRAP_ENV: &str = "VIBE_KANBAN_SKIP_PATH_BOOTSTRAP";
const PATH_BLOCK_START: &str = "# >>> Vibe Kanban CLI (managed) >>>";
const PATH_BLOCK_END: &str = "# <<< Vibe Kanban CLI (managed) <<<";
const PATH_EXPORT_LINE: &str = "export PATH=\"$HOME/.kanban/bin:$PATH\"";

/// Best-effort PATH bootstrap for desktop installs.
///
/// macOS runs the bootstrap on startup. Other platforms currently no-op.
pub fn bootstrap_cli_path() -> anyhow::Result<()> {
    if env::var(SKIP_PATH_BOOTSTRAP_ENV).as_deref() == Ok("1") {
        tracing::debug!(
            "{}=1 set, skipping CLI PATH bootstrap",
            SKIP_PATH_BOOTSTRAP_ENV
        );
        return Ok(());
    }

    #[cfg(target_os = "macos")]
    {
        let paths = BootstrapPaths::for_current_user()?;
        bootstrap_with_paths(&paths)?;
    }

    Ok(())
}

#[derive(Debug, Clone)]
struct BootstrapPaths {
    home_dir: PathBuf,
    executable: PathBuf,
    rc_files: Vec<PathBuf>,
}

impl BootstrapPaths {
    fn for_current_user() -> anyhow::Result<Self> {
        let home_dir = env::var_os("HOME")
            .map(PathBuf::from)
            .ok_or_else(|| anyhow::anyhow!("HOME is not set"))?;
        let executable = env::current_exe()?;
        let rc_files = vec![home_dir.join(".zshrc"), home_dir.join(".bashrc")];

        Ok(Self {
            home_dir,
            executable,
            rc_files,
        })
    }

    fn bin_dir(&self) -> PathBuf {
        self.home_dir.join(".kanban").join("bin")
    }
}

fn bootstrap_with_paths(paths: &BootstrapPaths) -> anyhow::Result<()> {
    let bin_dir = paths.bin_dir();
    fs::create_dir_all(&bin_dir)?;

    for command_name in ["kanban", "vibe-kanban"] {
        ensure_cli_symlink(&bin_dir.join(command_name), &paths.executable)?;
    }

    for rc_file in &paths.rc_files {
        ensure_path_block(rc_file)?;
    }

    Ok(())
}

fn ensure_cli_symlink(link_path: &Path, target: &Path) -> io::Result<()> {
    match fs::symlink_metadata(link_path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() {
                let existing_raw = fs::read_link(link_path)?;
                if symlink_points_to(link_path, &existing_raw, target) {
                    return Ok(());
                }

                if is_managed_symlink_target(&existing_raw) {
                    fs::remove_file(link_path)?;
                    create_symlink(target, link_path)?;
                    return Ok(());
                }

                tracing::warn!(
                    "Refusing to replace unmanaged symlink {} -> {}",
                    link_path.display(),
                    existing_raw.display()
                );
                return Ok(());
            }

            tracing::warn!(
                "Refusing to overwrite non-symlink command at {}",
                link_path.display()
            );
            Ok(())
        }
        Err(err) if err.kind() == ErrorKind::NotFound => create_symlink(target, link_path),
        Err(err) => Err(err),
    }
}

fn symlink_points_to(link_path: &Path, current_raw: &Path, expected_target: &Path) -> bool {
    let current_resolved = resolve_symlink_target(link_path, current_raw);
    let normalized_current = normalize_path(current_resolved);
    let normalized_expected = normalize_path(expected_target.to_path_buf());
    normalized_current == normalized_expected
}

fn resolve_symlink_target(link_path: &Path, target: &Path) -> PathBuf {
    if target.is_absolute() {
        return target.to_path_buf();
    }

    link_path
        .parent()
        .map(|parent| parent.join(target))
        .unwrap_or_else(|| target.to_path_buf())
}

fn normalize_path(path: PathBuf) -> PathBuf {
    fs::canonicalize(&path).unwrap_or(path)
}

fn is_managed_symlink_target(target: &Path) -> bool {
    let managed_binary_name = target
        .file_name()
        .and_then(|value| value.to_str())
        .map(|value| {
            let lowered = value.to_ascii_lowercase();
            lowered.starts_with("vibe-kanban")
        })
        .unwrap_or(false);

    let points_to_app_bundle = target.components().any(|component| {
        component
            .as_os_str()
            .to_string_lossy()
            .eq_ignore_ascii_case("Vibe Kanban.app")
    });

    managed_binary_name || points_to_app_bundle
}

fn ensure_path_block(rc_file: &Path) -> io::Result<()> {
    let existing = match fs::read_to_string(rc_file) {
        Ok(content) => content,
        Err(err) if err.kind() == ErrorKind::NotFound => String::new(),
        Err(err) => return Err(err),
    };

    let updated = upsert_path_block(&existing);
    if updated != existing {
        fs::write(rc_file, updated)?;
    }

    Ok(())
}

fn upsert_path_block(existing: &str) -> String {
    let managed_block_lines: Vec<String> = managed_path_block()
        .lines()
        .map(ToOwned::to_owned)
        .collect();
    let mut output: Vec<String> = Vec::new();
    let mut in_managed_block = false;
    let mut inserted = false;

    for line in existing.lines() {
        let trimmed = line.trim_end();
        if trimmed == PATH_BLOCK_START {
            in_managed_block = true;
            if !inserted {
                insert_managed_block(&mut output, &managed_block_lines);
                inserted = true;
            }
            continue;
        }

        if in_managed_block {
            if trimmed == PATH_BLOCK_END {
                in_managed_block = false;
            }
            continue;
        }

        output.push(line.to_string());
    }

    if !inserted {
        insert_managed_block(&mut output, &managed_block_lines);
    }

    let mut result = output.join("\n");
    if !result.ends_with('\n') {
        result.push('\n');
    }
    result
}

fn insert_managed_block(output: &mut Vec<String>, managed_block_lines: &[String]) {
    if !output.is_empty() && !output.last().is_some_and(|line| line.is_empty()) {
        output.push(String::new());
    }
    output.extend(managed_block_lines.iter().cloned());
}

fn managed_path_block() -> String {
    format!(
        "{PATH_BLOCK_START}\n# Added by the Vibe Kanban desktop app.\n{PATH_EXPORT_LINE}\n{PATH_BLOCK_END}"
    )
}

#[cfg(unix)]
fn create_symlink(target: &Path, link_path: &Path) -> io::Result<()> {
    std::os::unix::fs::symlink(target, link_path)
}

#[cfg(windows)]
fn create_symlink(target: &Path, link_path: &Path) -> io::Result<()> {
    std::os::windows::fs::symlink_file(target, link_path)
}

#[cfg(all(test, unix))]
mod tests {
    use std::{
        fs,
        path::{Path, PathBuf},
        time::{SystemTime, UNIX_EPOCH},
    };

    use super::{
        bootstrap_with_paths, create_symlink, BootstrapPaths, PATH_BLOCK_END, PATH_BLOCK_START,
        PATH_EXPORT_LINE,
    };

    struct TempHome {
        root: PathBuf,
    }

    impl TempHome {
        fn new(suffix: &str) -> Self {
            let timestamp = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock must be after unix epoch")
                .as_nanos();
            let root = std::env::temp_dir().join(format!(
                "vibe-kanban-cli-install-{suffix}-{}-{timestamp}",
                std::process::id()
            ));
            fs::create_dir_all(&root).expect("failed to create temp root");
            Self { root }
        }

        fn home_dir(&self) -> PathBuf {
            self.root.join("home")
        }
    }

    impl Drop for TempHome {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn creates_symlinks_and_rc_blocks_when_missing() {
        let tmp = TempHome::new("create-missing");
        let executable = create_fake_executable(&tmp.root, "current");
        let paths = bootstrap_paths_for_test(tmp.home_dir(), executable.clone());

        bootstrap_with_paths(&paths).expect("bootstrap should succeed");

        assert_symlink_target(paths.bin_dir().join("kanban"), &executable);
        assert_symlink_target(paths.bin_dir().join("vibe-kanban"), &executable);

        for rc_file in &paths.rc_files {
            let content = fs::read_to_string(rc_file).expect("expected rc file to be created");
            assert!(content.contains(PATH_BLOCK_START));
            assert!(content.contains(PATH_BLOCK_END));
            assert!(content.contains(PATH_EXPORT_LINE));
        }
    }

    #[test]
    fn bootstrap_is_idempotent() {
        let tmp = TempHome::new("idempotent");
        let executable = create_fake_executable(&tmp.root, "current");
        let paths = bootstrap_paths_for_test(tmp.home_dir(), executable);

        bootstrap_with_paths(&paths).expect("first bootstrap should succeed");
        bootstrap_with_paths(&paths).expect("second bootstrap should succeed");

        for rc_file in &paths.rc_files {
            let content = fs::read_to_string(rc_file).expect("expected rc file to exist");
            assert_eq!(content.matches(PATH_BLOCK_START).count(), 1);
            assert_eq!(content.matches(PATH_BLOCK_END).count(), 1);
            assert_eq!(content.matches(PATH_EXPORT_LINE).count(), 1);
        }
    }

    #[test]
    fn does_not_clobber_foreign_command_file() {
        let tmp = TempHome::new("foreign-file");
        let executable = create_fake_executable(&tmp.root, "current");
        let paths = bootstrap_paths_for_test(tmp.home_dir(), executable.clone());
        fs::create_dir_all(paths.bin_dir()).expect("expected bin dir creation");

        let foreign_command = paths.bin_dir().join("kanban");
        fs::write(&foreign_command, "echo external command").expect("expected foreign file");

        bootstrap_with_paths(&paths).expect("bootstrap should succeed");

        let metadata =
            fs::symlink_metadata(&foreign_command).expect("foreign command should exist");
        assert!(!metadata.file_type().is_symlink());
        let content = fs::read_to_string(&foreign_command).expect("expected foreign command");
        assert_eq!(content, "echo external command");

        assert_symlink_target(paths.bin_dir().join("vibe-kanban"), &executable);
    }

    #[test]
    fn does_not_replace_unmanaged_symlink() {
        let tmp = TempHome::new("foreign-symlink");
        let executable = create_fake_executable(&tmp.root, "current");
        let paths = bootstrap_paths_for_test(tmp.home_dir(), executable.clone());
        fs::create_dir_all(paths.bin_dir()).expect("expected bin dir creation");

        let external_target = tmp.root.join("tools/kanban");
        if let Some(parent) = external_target.parent() {
            fs::create_dir_all(parent).expect("expected external parent");
        }
        fs::write(&external_target, "external").expect("expected external command");

        let kanban_link = paths.bin_dir().join("kanban");
        create_symlink(&external_target, &kanban_link).expect("expected unmanaged symlink");

        bootstrap_with_paths(&paths).expect("bootstrap should succeed");

        assert_symlink_target(kanban_link, &external_target);
        assert_symlink_target(paths.bin_dir().join("vibe-kanban"), &executable);
    }

    #[test]
    fn updates_stale_managed_symlink_target() {
        let tmp = TempHome::new("stale-symlink");
        let executable = create_fake_executable(&tmp.root, "current");
        let paths = bootstrap_paths_for_test(tmp.home_dir(), executable.clone());
        fs::create_dir_all(paths.bin_dir()).expect("expected bin dir creation");

        let stale_target = tmp
            .root
            .join("Vibe Kanban.app/Contents/MacOS/vibe-kanban-desktop-old");
        let kanban_link = paths.bin_dir().join("kanban");
        create_symlink(&stale_target, &kanban_link).expect("expected stale symlink");

        bootstrap_with_paths(&paths).expect("bootstrap should succeed");

        assert_symlink_target(kanban_link, &executable);
    }

    fn bootstrap_paths_for_test(home_dir: PathBuf, executable: PathBuf) -> BootstrapPaths {
        let rc_files = vec![home_dir.join(".zshrc"), home_dir.join(".bashrc")];
        BootstrapPaths {
            home_dir,
            executable,
            rc_files,
        }
    }

    fn create_fake_executable(root: &Path, profile: &str) -> PathBuf {
        let path = root
            .join("apps")
            .join(profile)
            .join("Vibe Kanban.app/Contents/MacOS/vibe-kanban-desktop");
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("failed to create executable parent");
        }
        fs::write(&path, "fake-desktop-executable").expect("failed to write executable");
        path
    }

    fn assert_symlink_target(link_path: PathBuf, expected_target: &Path) {
        let metadata = fs::symlink_metadata(&link_path).expect("expected symlink to exist");
        assert!(metadata.file_type().is_symlink());
        let target = fs::read_link(&link_path).expect("expected to read symlink target");
        assert_eq!(target, expected_target);
    }
}
