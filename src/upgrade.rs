//! Standalone self-upgrade support. No daemon is stopped or restarted here.

use std::fs::{self, File, OpenOptions};
use std::io::{self, IsTerminal, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail, ensure};
use semver::Version;
use serde::Deserialize;
use sha2::{Digest, Sha256};

const LATEST_RELEASE: &str = "https://api.github.com/repos/mikker/fut/releases/latest";

#[derive(Deserialize)]
struct Release {
    tag_name: String,
    assets: Vec<Asset>,
}

#[derive(Deserialize)]
struct Asset {
    name: String,
    browser_download_url: String,
}

/// Check for or install the latest stable GitHub release.
///
/// `check` never creates staging files or changes the installation. `yes`
/// bypasses confirmation, but not checksum, version, or Homebrew checks.
pub async fn run(check: bool, yes: bool) -> Result<()> {
    tokio::task::spawn_blocking(move || run_blocking(check, yes))
        .await
        .context("upgrade worker failed")?
}

fn run_blocking(check: bool, yes: bool) -> Result<()> {
    let target = std::env::current_exe()
        .context("cannot locate the running executable")?
        .canonicalize()
        .context("cannot resolve the installed executable")?;
    let homebrew = is_homebrew(&target);
    if homebrew && !check {
        bail!("Fut is managed by Homebrew; run `brew upgrade fut` instead");
    }

    let archive_name = archive_name(std::env::consts::OS, std::env::consts::ARCH)?;
    let current = parse_version(env!("CARGO_PKG_VERSION"))?;
    let output = curl(LATEST_RELEASE).output().context("cannot run curl")?;
    ensure!(
        output.status.success(),
        "GitHub release request failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    let release: Release =
        serde_json::from_slice(&output.stdout).context("GitHub returned invalid release JSON")?;
    let latest = parse_version(&release.tag_name)?;
    if latest <= current {
        println!("Fut {current} is up to date (latest release: {latest}).");
        return Ok(());
    }
    println!("Fut {latest} is available (installed: {current}).");
    println!("Installation: {}", target.display());
    if check {
        if homebrew {
            println!("Run `brew upgrade fut` to upgrade this Homebrew installation.");
        }
        return Ok(());
    }

    let archive_url = asset_url(&release, &archive_name)?;
    let checksum_url = asset_url(&release, &format!("{archive_name}.sha256"))?;
    if !yes && !confirm(&target, &latest)? {
        println!("Upgrade cancelled.");
        return Ok(());
    }

    let parent = target
        .parent()
        .context("executable has no parent directory")?;
    let staging = tempfile::Builder::new()
        .prefix(".fut-upgrade-")
        .tempdir_in(parent)
        .context("cannot stage upgrade beside Fut; check directory permissions (no sudo is run)")?;
    let archive = staging.path().join(&archive_name);
    let checksum = staging.path().join("checksum.sha256");
    download(archive_url, &archive)?;
    download(checksum_url, &checksum)?;
    verify_checksum(
        &archive,
        &fs::read_to_string(&checksum).context("cannot read checksum")?,
    )?;

    let binary = staging.path().join("fut");
    extract_binary(&archive, &binary)?;
    set_executable_permissions(&binary, &target)?;
    validate_binary(&binary, &latest)?;
    // The staging directory is on the target filesystem. Rename replaces the
    // directory entry atomically, leaving existing processes on the old inode.
    fs::rename(&binary, &target)
        .context("cannot atomically replace Fut; check installation permissions")?;
    println!("Upgraded Fut to {latest} at {}.", target.display());
    eprintln!(
        "Any running Fut daemon still uses the old binary. Restart it safely when you can; this upgrade does not stop or restart it."
    );
    Ok(())
}

fn archive_name(os: &str, arch: &str) -> Result<String> {
    let platform = match os {
        "macos" => "macos",
        "linux" => "linux",
        _ => bail!("unsupported operating system: {os}"),
    };
    let arch = match arch {
        "aarch64" => "arm64",
        "x86_64" => "x86_64",
        _ => bail!("unsupported architecture: {arch}"),
    };
    Ok(format!("fut-{platform}-{arch}.tar.gz"))
}

fn parse_version(value: &str) -> Result<Version> {
    let value = value.trim();
    let value = value.strip_prefix('v').unwrap_or(value);
    // Fut's release tags omit the patch component; let semver validate digits.
    let normalized = if value.split('.').count() == 2 {
        format!("{value}.0")
    } else {
        value.to_owned()
    };
    Version::parse(&normalized).with_context(|| format!("invalid Fut version: {value}"))
}

fn is_homebrew(path: &Path) -> bool {
    let components: Vec<_> = path.components().collect();
    components
        .windows(2)
        .any(|pair| pair[0].as_os_str() == "Cellar" && pair[1].as_os_str() == "fut")
}

fn asset_url<'a>(release: &'a Release, name: &str) -> Result<&'a str> {
    let asset = release
        .assets
        .iter()
        .find(|asset| asset.name == name)
        .with_context(|| format!("release {} is missing asset {name}", release.tag_name))?;
    ensure!(
        asset.browser_download_url.starts_with("https://"),
        "asset URL must use HTTPS"
    );
    Ok(&asset.browser_download_url)
}

fn curl(url: &str) -> Command {
    let mut command = Command::new("curl");
    command
        .args([
            "--disable",
            "--fail",
            "--silent",
            "--show-error",
            "--location",
            "--proto",
            "=https",
            "--proto-redir",
            "=https",
            "--connect-timeout",
            "15",
            "--max-time",
            "180",
            "--user-agent",
            concat!("fut/", env!("CARGO_PKG_VERSION")),
            "--url",
            url,
        ])
        .stdin(Stdio::null());
    command
}

fn download(url: &str, destination: &Path) -> Result<()> {
    let output = curl(url)
        .arg("--output")
        .arg(destination)
        .output()
        .context("cannot run curl to download release asset")?;
    ensure!(
        output.status.success(),
        "download failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(())
}

fn verify_checksum(archive: &Path, checksum: &str) -> Result<()> {
    let expected = checksum
        .split_whitespace()
        .next()
        .context("empty checksum file")?;
    ensure!(
        expected.len() == 64 && expected.bytes().all(|b| b.is_ascii_hexdigit()),
        "invalid SHA-256 checksum"
    );
    let mut file = File::open(archive).context("cannot read downloaded archive")?;
    let mut hash = Sha256::new();
    let mut buffer = [0; 64 * 1024];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hash.update(&buffer[..count]);
    }
    ensure!(
        format!("{:x}", hash.finalize()).eq_ignore_ascii_case(expected),
        "release archive checksum verification failed"
    );
    Ok(())
}

fn extract_binary(archive: &Path, binary: &Path) -> Result<()> {
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(binary)?;
    // Stream ONLY the exact member to a file we created. Never let tar create
    // paths, links, permissions, or other archive members on the filesystem.
    let output = Command::new("tar")
        .env_remove("TAR_OPTIONS")
        .arg("-xOzf")
        .arg(archive)
        .args(["--", "fut"])
        .stdin(Stdio::null())
        .stdout(Stdio::from(file))
        .stderr(Stdio::piped())
        .output()
        .context("cannot run tar")?;
    ensure!(
        output.status.success(),
        "cannot extract fut: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    ensure!(
        fs::metadata(binary)?.len() > 0,
        "archive contains no Fut binary"
    );
    Ok(())
}

fn set_executable_permissions(binary: &Path, target: &Path) -> Result<()> {
    // Preserve ordinary permission bits, ensure executability, and never copy
    // setuid/setgid/sticky bits to a downloaded executable.
    let mode = (fs::metadata(target)?.permissions().mode() & 0o777) | 0o100;
    fs::set_permissions(binary, fs::Permissions::from_mode(mode))?;
    Ok(())
}

fn validate_binary(binary: &Path, expected: &Version) -> Result<()> {
    let output = Command::new(binary)
        .arg("--version")
        .stdin(Stdio::null())
        .output()
        .context("cannot run downloaded Fut binary")?;
    ensure!(output.status.success(), "downloaded Fut failed --version");
    let text = std::str::from_utf8(&output.stdout).context("invalid Fut version output")?;
    let mut words = text.split_whitespace();
    ensure!(
        words.next() == Some("fut"),
        "unexpected Fut version output: {text:?}"
    );
    let actual = parse_version(words.next().context("missing Fut version")?)?;
    ensure!(
        words.next().is_none() && actual == *expected,
        "downloaded Fut version does not match release {expected}: {text:?}"
    );
    Ok(())
}

fn confirm(target: &Path, latest: &Version) -> Result<bool> {
    ensure!(
        io::stdin().is_terminal(),
        "upgrade requires interactive confirmation; pass --yes to upgrade non-interactively"
    );
    eprint!("Replace {} with Fut {latest}? [y/N] ", target.display());
    io::stderr().flush()?;
    let mut response = String::new();
    io::stdin().read_line(&mut response)?;
    Ok(matches!(
        response.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn platform_asset_names() {
        for os in ["macos", "linux"] {
            for (arch, name) in [("aarch64", "arm64"), ("x86_64", "x86_64")] {
                assert_eq!(
                    archive_name(os, arch).unwrap(),
                    format!("fut-{os}-{name}.tar.gz")
                );
            }
        }
        assert!(archive_name("windows", "x86_64").is_err());
        assert!(archive_name("linux", "riscv64").is_err());
    }

    #[test]
    fn release_tags_and_semver() {
        for value in ["0.29", "0.29.0", "v0.29", " v0.29.0 "] {
            assert_eq!(parse_version(value).unwrap(), Version::new(0, 29, 0));
        }
        assert!(parse_version("0.30").unwrap() > parse_version("0.29.9").unwrap());
        assert!(parse_version("0.29.0-rc.1").unwrap() < parse_version("0.29").unwrap());
        for value in ["", "0", "0.", "0.29/evil", "release", "0.029"] {
            assert!(parse_version(value).is_err(), "{value}");
        }
    }

    #[test]
    fn detects_canonical_homebrew_paths() {
        for path in [
            "/opt/homebrew/Cellar/fut/0.29/bin/fut",
            "/usr/local/Cellar/fut/0.29/bin/fut",
            "/home/linuxbrew/.linuxbrew/Cellar/fut/0.29/bin/fut",
        ] {
            assert!(is_homebrew(Path::new(path)));
        }
        for path in [
            "/usr/local/bin/fut",
            "/home/me/.local/bin/fut",
            "/tmp/NotCellar/fut/bin/fut",
            "/opt/homebrew/Cellar/other/bin/fut",
        ] {
            assert!(!is_homebrew(Path::new(path)));
        }
    }

    #[test]
    fn verifies_checksums() {
        let directory = tempfile::tempdir().unwrap();
        let archive = directory.path().join("archive");
        fs::write(&archive, b"abc").unwrap();
        let hash = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
        verify_checksum(&archive, &format!("{hash}  dist/fut-linux-arm64.tar.gz\n")).unwrap();
        verify_checksum(&archive, &hash.to_uppercase()).unwrap();
        assert!(verify_checksum(&archive, &"0".repeat(64)).is_err());
        assert!(verify_checksum(&archive, "").is_err());
        assert!(verify_checksum(&archive, "not-a-checksum").is_err());
        fs::write(&archive, b"changed").unwrap();
        assert!(verify_checksum(&archive, hash).is_err());
    }

    #[test]
    fn assets_must_exist_and_use_https() {
        let mut release = Release {
            tag_name: "0.30".into(),
            assets: vec![Asset {
                name: "fut.tar.gz".into(),
                browser_download_url: "https://github.com/asset".into(),
            }],
        };
        assert_eq!(
            asset_url(&release, "fut.tar.gz").unwrap(),
            "https://github.com/asset"
        );
        assert!(asset_url(&release, "missing").is_err());
        release.assets[0].browser_download_url = "http://github.com/asset".into();
        assert!(asset_url(&release, "fut.tar.gz").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn homebrew_detection_after_resolving_a_symlink() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let cellar = directory.path().join("Cellar/fut/0.30/bin");
        fs::create_dir_all(&cellar).unwrap();
        fs::write(cellar.join("fut"), b"binary").unwrap();
        let link = directory.path().join("fut");
        symlink(cellar.join("fut"), &link).unwrap();
        assert!(!is_homebrew(&link));
        assert!(is_homebrew(&link.canonicalize().unwrap()));
    }

    #[cfg(unix)]
    #[test]
    fn permissions_preserve_ordinary_bits_not_setuid_or_setgid() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("installed");
        let binary = directory.path().join("downloaded");
        fs::write(&target, b"old").unwrap();
        fs::write(&binary, b"new").unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o6750)).unwrap();
        set_executable_permissions(&binary, &target).unwrap();
        assert_eq!(
            fs::metadata(&binary).unwrap().permissions().mode() & 0o7777,
            0o750
        );
    }

    #[cfg(unix)]
    #[test]
    fn extracts_only_fut_and_validates_version() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("fut"), "#!/bin/sh\nprintf 'fut 0.30.0\\n'\n").unwrap();
        fs::write(source.join("other"), "must not extract").unwrap();
        let archive = directory.path().join("archive.tar.gz");
        assert!(
            Command::new("tar")
                .arg("-czf")
                .arg(&archive)
                .arg("-C")
                .arg(&source)
                .args(["fut", "other"])
                .status()
                .unwrap()
                .success()
        );
        let binary = directory.path().join("fut");
        extract_binary(&archive, &binary).unwrap();
        assert!(!directory.path().join("other").exists());
        set_executable_permissions(&binary, &source.join("fut")).unwrap();
        validate_binary(&binary, &Version::new(0, 30, 0)).unwrap();
        assert!(validate_binary(&binary, &Version::new(0, 29, 0)).is_err());
        assert!(extract_binary(&archive, &binary).is_err());
    }
}
