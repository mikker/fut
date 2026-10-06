//! Exercise upgrades on disposable copies with a fake curl, never the user's installation.
#![cfg(unix)]

use std::{fs, os::unix::fs::PermissionsExt, path::PathBuf, process::Command};

use sha2::{Digest, Sha256};

struct Fixture {
    _directory: tempfile::TempDir,
    target: PathBuf,
    tools: PathBuf,
    archive: PathBuf,
    checksum: PathBuf,
    release: String,
    replacement: Vec<u8>,
}

impl Fixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("fut");
        fs::copy(assert_cmd::cargo::cargo_bin!("fut"), &target).unwrap();
        let tools = directory.path().join("tools");
        fs::create_dir(&tools).unwrap();
        let curl = tools.join("curl");
        fs::write(
            &curl,
            r#"#!/bin/sh
destination=''
url=''
while [ "$#" -gt 0 ]; do
  case "$1" in
    --output) destination="$2"; shift ;;
    --url) url="$2"; shift ;;
  esac
  shift
done
if [ -z "$destination" ]; then
  printf '%s' "$MOCK_RELEASE"
else
  case "$url" in
    *.sha256) cp "$MOCK_CHECKSUM" "$destination" ;;
    *.tar.gz) cp "$MOCK_ARCHIVE" "$destination" ;;
    *) exit 1 ;;
  esac
fi
"#,
        )
        .unwrap();
        fs::set_permissions(&curl, fs::Permissions::from_mode(0o755)).unwrap();
        let source = directory.path().join("source");
        fs::create_dir(&source).unwrap();
        let replacement = b"#!/bin/sh\nprintf 'fut 999.0.0\\n'\n".to_vec();
        fs::write(source.join("fut"), &replacement).unwrap();
        let archive = directory.path().join("release.tar.gz");
        assert!(
            Command::new("tar")
                .arg("-czf")
                .arg(&archive)
                .arg("-C")
                .arg(&source)
                .arg("fut")
                .status()
                .unwrap()
                .success()
        );
        let checksum = directory.path().join("release.sha256");
        fs::write(
            &checksum,
            format!("{:x}", Sha256::digest(fs::read(&archive).unwrap())),
        )
        .unwrap();
        let platform = if cfg!(target_os = "macos") {
            "macos"
        } else {
            "linux"
        };
        let arch = if cfg!(target_arch = "aarch64") {
            "arm64"
        } else {
            "x86_64"
        };
        let name = format!("fut-{platform}-{arch}.tar.gz");
        let release = serde_json::json!({
            "tag_name": "999.0.0",
            "assets": [
                {"name": name, "browser_download_url": format!("https://github.com/mock/{name}")},
                {"name": format!("{name}.sha256"), "browser_download_url": format!("https://github.com/mock/{name}.sha256")}
            ]
        }).to_string();
        Self {
            _directory: directory,
            target,
            tools,
            archive,
            checksum,
            release,
            replacement,
        }
    }

    fn command(&self) -> Command {
        let mut command = Command::new(&self.target);
        let mut paths = vec![self.tools.clone()];
        paths.extend(std::env::split_paths(
            &std::env::var_os("PATH").unwrap_or_default(),
        ));
        command
            .arg("upgrade")
            .env("PATH", std::env::join_paths(paths).unwrap())
            .env("MOCK_RELEASE", &self.release)
            .env("MOCK_ARCHIVE", &self.archive)
            .env("MOCK_CHECKSUM", &self.checksum)
            // Upgrade must not load configuration or contact this socket.
            .arg("--config-dir")
            .arg("/nonexistent/fut-upgrade-config")
            .arg("--socket")
            .arg("/nonexistent/fut-upgrade.sock");
        command
    }
}

#[test]
fn check_is_read_only_even_with_yes() {
    let fixture = Fixture::new();
    let original = fs::read(&fixture.target).unwrap();
    // Check must not attempt to download either of these missing files.
    fs::remove_file(&fixture.archive).unwrap();
    fs::remove_file(&fixture.checksum).unwrap();
    let output = fixture
        .command()
        .args(["--check", "--yes"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("999.0.0"));
    assert_eq!(fs::read(&fixture.target).unwrap(), original);
}

#[test]
fn verified_upgrade_replaces_only_the_invoked_binary() {
    let fixture = Fixture::new();
    let output = fixture.command().arg("--yes").output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(fs::read(&fixture.target).unwrap(), fixture.replacement);
    assert!(String::from_utf8_lossy(&output.stderr).contains("does not stop or restart"));
    assert!(
        Command::new(&fixture.target)
            .arg("--version")
            .output()
            .unwrap()
            .status
            .success()
    );
}

#[test]
fn bad_checksum_leaves_installation_untouched() {
    let fixture = Fixture::new();
    let original = fs::read(&fixture.target).unwrap();
    fs::write(&fixture.checksum, "0".repeat(64)).unwrap();
    let output = fixture.command().arg("--yes").output().unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("checksum verification failed"));
    assert_eq!(fs::read(&fixture.target).unwrap(), original);
}

#[test]
fn mismatched_binary_version_leaves_installation_untouched() {
    let mut fixture = Fixture::new();
    fixture.release = fixture.release.replace("999.0.0", "999.1.0");
    let original = fs::read(&fixture.target).unwrap();
    let output = fixture.command().arg("--yes").output().unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("does not match release"));
    assert_eq!(fs::read(&fixture.target).unwrap(), original);
}

#[test]
fn current_version_does_not_download_or_replace() {
    let mut fixture = Fixture::new();
    fixture.release = fixture
        .release
        .replace("999.0.0", env!("CARGO_PKG_VERSION"));
    let original = fs::read(&fixture.target).unwrap();
    fs::remove_file(&fixture.archive).unwrap();
    fs::remove_file(&fixture.checksum).unwrap();
    let output = fixture.command().arg("--yes").output().unwrap();
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("up to date"));
    assert_eq!(fs::read(&fixture.target).unwrap(), original);
}

#[test]
fn homebrew_installation_refuses_replacement() {
    let mut fixture = Fixture::new();
    let homebrew = fixture._directory.path().join("Cellar/fut/999/bin");
    fs::create_dir_all(&homebrew).unwrap();
    let target = homebrew.join("fut");
    fs::rename(&fixture.target, &target).unwrap();
    fixture.target = target;
    let original = fs::read(&fixture.target).unwrap();
    let output = fixture.command().arg("--yes").output().unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("brew upgrade fut"));
    assert_eq!(fs::read(&fixture.target).unwrap(), original);
}

#[test]
fn noninteractive_upgrade_requires_yes() {
    let fixture = Fixture::new();
    let original = fs::read(&fixture.target).unwrap();
    let output = fixture
        .command()
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("--yes"));
    assert_eq!(fs::read(&fixture.target).unwrap(), original);
}
