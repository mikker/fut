use std::{env, fs, io::Read, os::unix::fs::OpenOptionsExt, path::PathBuf};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

const MAX_CONFIG_BYTES: u64 = 64 * 1024;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum TabBarPosition {
    #[default]
    Top,
    Bottom,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum WorkspaceSidebarPosition {
    #[default]
    Left,
    Right,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum PaneLayoutPolicy {
    #[default]
    Splits,
    Accordion,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct UiConfig {
    pub(super) tab_bar_position: TabBarPosition,
    pub(super) workspace_sidebar_position: WorkspaceSidebarPosition,
    pub(super) pane_layout: PaneLayoutPolicy,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Config {
    ui: UiConfig,
}

pub(super) fn load() -> Result<UiConfig> {
    let (path, explicit) = match env::var_os("FUT_CONFIG") {
        Some(path) => {
            let path = PathBuf::from(path);
            if !path.is_absolute() {
                bail!("FUT_CONFIG must be an absolute path");
            }
            (path, true)
        }
        None => match env::var_os("XDG_CONFIG_HOME") {
            Some(directory) if PathBuf::from(&directory).is_absolute() => {
                (PathBuf::from(directory).join("fut/config.toml"), false)
            }
            _ => {
                let Some(home) = env::var_os("HOME") else {
                    return Ok(UiConfig::default());
                };
                let home = PathBuf::from(home);
                if !home.is_absolute() {
                    bail!("HOME must be an absolute path when resolving Fut config");
                }
                (home.join(".config/fut/config.toml"), false)
            }
        },
    };
    load_path(&path, explicit)
}

fn load_path(path: &std::path::Path, explicit: bool) -> Result<UiConfig> {
    let file = match fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound && !explicit => {
            return Ok(UiConfig::default());
        }
        Err(error) => {
            return Err(error).with_context(|| format!("read Fut config {}", path.display()));
        }
    };
    let metadata = file
        .metadata()
        .with_context(|| format!("inspect Fut config {}", path.display()))?;
    if !metadata.file_type().is_file() {
        bail!("Fut config {} is not a regular file", path.display());
    }
    if metadata.len() > MAX_CONFIG_BYTES {
        bail!(
            "Fut config {} is {} bytes; maximum is {MAX_CONFIG_BYTES}",
            path.display(),
            metadata.len()
        );
    }
    let mut source = String::new();
    file.take(MAX_CONFIG_BYTES + 1)
        .read_to_string(&mut source)
        .with_context(|| format!("read Fut config {}", path.display()))?;
    if source.len() as u64 > MAX_CONFIG_BYTES {
        bail!(
            "Fut config {} exceeds the {MAX_CONFIG_BYTES}-byte maximum",
            path.display()
        );
    }
    toml::from_str::<Config>(&source)
        .with_context(|| format!("parse Fut config {}", path.display()))
        .map(|config| config.ui)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_implicit_config_and_empty_file_use_defaults() {
        let temporary = tempfile::tempdir().unwrap();
        let missing = temporary.path().join("missing.toml");
        assert_eq!(load_path(&missing, false).unwrap(), UiConfig::default());
        assert!(
            load_path(&missing, true)
                .unwrap_err()
                .to_string()
                .contains("read Fut config")
        );

        let empty = temporary.path().join("empty.toml");
        fs::write(&empty, "").unwrap();
        assert_eq!(load_path(&empty, true).unwrap(), UiConfig::default());
    }

    #[test]
    fn positions_are_independent_and_explicit() {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("config.toml");
        fs::write(
            &path,
            "[ui]\ntab_bar_position = \"bottom\"\nworkspace_sidebar_position = \"right\"\npane_layout = \"accordion\"\n",
        )
        .unwrap();

        assert_eq!(
            load_path(&path, true).unwrap(),
            UiConfig {
                tab_bar_position: TabBarPosition::Bottom,
                workspace_sidebar_position: WorkspaceSidebarPosition::Right,
                pane_layout: PaneLayoutPolicy::Accordion,
            }
        );
    }

    #[test]
    fn malformed_unknown_and_oversized_configs_are_rejected_with_the_path() {
        let temporary = tempfile::tempdir().unwrap();
        for source in [
            "[ui]\ntab_bar_position = \"sideways\"\n",
            "[ui]\nexecute = \"surprise\"\n",
            "[project]\ncommand = \"nope\"\n",
        ] {
            let path = temporary.path().join(format!("bad-{}.toml", source.len()));
            fs::write(&path, source).unwrap();
            let error = load_path(&path, true).unwrap_err().to_string();
            assert!(error.contains(&path.display().to_string()), "{error}");
        }

        let path = temporary.path().join("large.toml");
        fs::write(&path, vec![b' '; MAX_CONFIG_BYTES as usize + 1]).unwrap();
        let error = load_path(&path, true).unwrap_err().to_string();
        assert!(error.contains("maximum"));
        assert!(error.contains(&path.display().to_string()));

        let path = temporary.path().join("pipe.toml");
        let path_bytes = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
        // SAFETY: path_bytes is a valid NUL-terminated path and the mode is valid.
        assert_eq!(unsafe { libc::mkfifo(path_bytes.as_ptr(), 0o600) }, 0);
        let error = load_path(&path, true).unwrap_err().to_string();
        assert!(error.contains("not a regular file"), "{error}");
    }
}
