/// Verify inherited terminal context against the live process tree. A desktop
/// app can inherit FUT_* while being reparented outside the terminal's tree.
pub(crate) fn is_descendant(pid: u32, ancestor: u32) -> bool {
    lineage(pid, ancestor).is_some()
}

/// Return the process and its ancestors through `ancestor`, but only when the
/// complete chain still belongs to that ancestor. Processes may disappear
/// while the chain is inspected, in which case there is no trustworthy owner.
pub(super) fn lineage(mut pid: u32, ancestor: u32) -> Option<Vec<u32>> {
    let mut lineage = Vec::new();
    for _ in 0..128 {
        if pid <= 1 {
            return None;
        }
        lineage.push(pid);
        if pid == ancestor {
            return Some(lineage);
        }
        let parent = parent_pid(pid)?;
        if parent == pid {
            return None;
        }
        pid = parent;
    }
    None
}

/// Resolve the root of the reporting process's branch beneath a terminal
/// runtime. Unlike the short-lived reporter itself, this process represents
/// the command that owns the automatic integration.
pub(super) fn descendant_root(pid: u32, ancestor: u32) -> Option<u32> {
    let lineage = lineage(pid, ancestor)?;
    if lineage.len() <= 2 {
        Some(ancestor)
    } else {
        lineage.get(lineage.len() - 2).copied()
    }
}

#[cfg(target_os = "macos")]
fn parent_pid(pid: u32) -> Option<u32> {
    let mut info = std::mem::MaybeUninit::<libc::proc_bsdinfo>::uninit();
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as i32;
    // SAFETY: proc_pidinfo writes at most size bytes into this valid buffer.
    let written = unsafe {
        libc::proc_pidinfo(
            pid.try_into().ok()?,
            libc::PROC_PIDTBSDINFO,
            0,
            info.as_mut_ptr().cast(),
            size,
        )
    };
    if written != size {
        return None;
    }
    // SAFETY: proc_pidinfo filled the complete structure.
    Some(unsafe { info.assume_init() }.pbi_ppid)
}

#[cfg(target_os = "linux")]
fn parent_pid(pid: u32) -> Option<u32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // comm may contain spaces and parentheses; the final ')' ends that field.
    stat.rsplit_once(')')?
        .1
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn parent_pid(_pid: u32) -> Option<u32> {
    None
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct RestartProcess {
    pub program: std::path::PathBuf,
    pub argv0: String,
    pub args: Vec<String>,
}

fn is_shell(program: &std::path::Path) -> bool {
    program
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| matches!(name, "sh" | "bash" | "zsh" | "fish" | "dash" | "ksh" | "nu"))
}

fn shell_command(command: &RestartProcess) -> bool {
    is_shell(&command.program)
        || is_shell(std::path::Path::new(command.argv0.trim_start_matches('-')))
}

pub(super) fn shell_process(pid: u32) -> bool {
    raw_restart_process(pid).is_some_and(|p| shell_command(&p))
}

pub(super) struct ProcessGroups(Vec<(u32, u32, u32)>);

impl ProcessGroups {
    pub async fn observe() -> Option<Self> {
        let output = tokio::time::timeout(
            std::time::Duration::from_millis(500),
            tokio::process::Command::new("/bin/ps")
                .args(["-axo", "pid=,ppid=,pgid="])
                .kill_on_drop(true)
                .output(),
        )
        .await
        .ok()?
        .ok()?;
        if !output.status.success() {
            return None;
        }
        let text = std::str::from_utf8(&output.stdout).ok()?;
        let mut processes = Vec::new();
        for line in text.lines() {
            let mut fields = line.split_whitespace();
            processes.push((
                fields.next()?.parse().ok()?,
                fields.next()?.parse().ok()?,
                fields.next()?.parse().ok()?,
            ));
            if fields.next().is_some() {
                return None;
            }
        }
        Some(Self(processes))
    }

    pub fn children(&self, parent: u32) -> impl Iterator<Item = u32> + '_ {
        self.0
            .iter()
            .filter(move |(_, ppid, _)| *ppid == parent)
            .map(|(pid, _, _)| *pid)
    }

    pub fn program_pid(&self, group: u32) -> Option<u32> {
        let members: Vec<_> = self
            .0
            .iter()
            .filter(|(_, _, pgid)| *pgid == group)
            .collect();
        let roots: Vec<_> = members
            .iter()
            .filter(|(_, parent, _)| !members.iter().any(|(pid, _, _)| pid == parent))
            .collect();
        // Multiple independent group roots are a pipeline, not one restartable program.
        if roots.len() != 1 {
            return None;
        }
        let pid = roots[0].0;
        let process = raw_restart_process(pid)?;
        if !shell_command(&process) || members.len() == 1 {
            return Some(pid);
        }
        // Only unwrap Fut's own returning-shell wrapper, never arbitrary shell scripts.
        if process.args.get(..3)?
            != [
                "-c",
                r#""$@"; exec "${SHELL:-/bin/sh}""#,
                "fut-project-command",
            ]
        {
            return None;
        }
        let children: Vec<_> = members
            .iter()
            .filter(|(_, parent, _)| *parent == pid)
            .collect();
        if children.len() != 1 {
            return None;
        }
        let child = children[0].0;
        (!shell_process(child)).then_some(child)
    }
}

pub(super) fn restart_process(pid: u32) -> Option<RestartProcess> {
    let mut command = raw_restart_process(pid)?;
    if shell_command(&command) {
        return None;
    }
    sanitize_agent_arguments(&mut command);
    if command.args.len() > 256 || command.args.iter().any(|arg| arg.len() > 64 * 1024) {
        return None;
    }
    command.program = restart_executable(&command.program, &command.argv0)?;
    Some(command)
}

fn sanitize_agent_arguments(command: &mut RestartProcess) {
    let basename = command
        .program
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or_default();
    // Agent session identifiers and positional prompts must never be replayed.
    let agent = |name: &str| {
        matches!(
            name,
            "claude" | "codex" | "pi" | "opencode" | "aider" | "gemini"
        )
    };
    let invoked = std::path::Path::new(&command.argv0)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or_default();
    if agent(basename)
        || agent(invoked)
        || command
            .program
            .to_str()
            .is_some_and(|s| s.contains("/claude/versions/"))
    {
        command.args.clear();
    } else if [basename, invoked]
        .iter()
        .any(|name| matches!(*name, "node" | "bun") || name.starts_with("python"))
    {
        if command.args.first().is_some_and(|s| s == "-m")
            && command
                .args
                .get(1)
                .is_some_and(|s| matches!(s.as_str(), "aider" | "aider.main"))
        {
            command.args.truncate(2);
        } else if let Some(index) = command.args.iter().position(|arg| {
            let entry = std::path::Path::new(arg)
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or_default();
            agent(entry)
                || [
                    "pi-coding-agent",
                    "claude-code",
                    "@openai/codex",
                    "opencode",
                    "/aider/",
                    "/bin/aider",
                    "gemini-cli",
                ]
                .iter()
                .any(|name| arg.contains(name))
        }) {
            command.args.truncate(index + 1);
        }
    }
}

fn restart_executable(executable: &std::path::Path, argv0: &str) -> Option<std::path::PathBuf> {
    use std::os::unix::fs::MetadataExt;
    let original = std::fs::metadata(executable).ok()?;
    let invoked = std::path::Path::new(argv0);
    let mut candidates = vec![invoked.to_path_buf()];
    if !invoked.is_absolute() {
        candidates.push(executable.parent()?.join(invoked));
        if let Some(path) = std::env::var_os("PATH") {
            candidates.extend(std::env::split_paths(&path).map(|dir| dir.join(invoked)));
        }
    }
    for candidate in candidates.into_iter().filter(|p| p.is_absolute()) {
        if let Ok(metadata) = std::fs::metadata(&candidate)
            && metadata.dev() == original.dev()
            && metadata.ino() == original.ino()
        {
            return Some(candidate);
        }
    }
    // Multicall executables and interpreter aliases need their invocation name.
    (!invoked.is_absolute() && executable.file_name() == invoked.file_name())
        .then(|| executable.to_path_buf())
}

#[cfg(target_os = "linux")]
fn raw_restart_process(pid: u32) -> Option<RestartProcess> {
    let program = std::fs::read_link(format!("/proc/{pid}/exe")).ok()?;
    let data = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    if data.len() > 256 * 1024 || !data.ends_with(&[0]) {
        return None;
    }
    let mut args = data[..data.len() - 1].split(|byte| *byte == 0);
    let argv0 = std::str::from_utf8(args.next()?).ok()?.to_owned();
    let args = args
        .map(|bytes| std::str::from_utf8(bytes).map(str::to_owned))
        .collect::<Result<Vec<_>, _>>()
        .ok()?;
    Some(RestartProcess {
        program,
        argv0,
        args,
    })
}

#[cfg(target_os = "macos")]
fn mac_process_arguments(pid: u32) -> Option<Vec<u8>> {
    let mut mib = [
        libc::CTL_KERN,
        libc::KERN_PROCARGS2,
        i32::try_from(pid).ok()?,
    ];
    let mut bytes = vec![0u8; 256 * 1024];
    let mut len = bytes.len();
    // SAFETY: sysctl is given a valid MIB and a bounded writable buffer.
    let result = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            3,
            bytes.as_mut_ptr().cast(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if result != 0 || len < 4 {
        return None;
    }
    bytes.truncate(len);
    Some(bytes)
}

#[cfg(target_os = "macos")]
fn raw_restart_process(pid: u32) -> Option<RestartProcess> {
    let bytes = mac_process_arguments(pid)?;
    let argc = i32::from_ne_bytes(bytes[..4].try_into().ok()?);
    if !(1..=257).contains(&argc) {
        return None;
    }
    let executable_end = bytes[4..].iter().position(|b| *b == 0)? + 4;
    let program = std::str::from_utf8(&bytes[4..executable_end]).ok()?.into();
    let start = bytes[executable_end..].iter().position(|b| *b != 0)? + executable_end;
    let mut arguments = bytes[start..]
        .split(|b| *b == 0)
        .take(usize::try_from(argc).ok()?);
    let argv0 = std::str::from_utf8(arguments.next()?).ok()?.to_owned();
    let args = arguments
        .map(|bytes| std::str::from_utf8(bytes).map(str::to_owned))
        .collect::<Result<Vec<_>, _>>()
        .ok()?;
    if args.len() + 1 != usize::try_from(argc).ok()? {
        return None;
    }
    Some(RestartProcess {
        program,
        argv0,
        args,
    })
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn raw_restart_process(_pid: u32) -> Option<RestartProcess> {
    None
}

pub(super) fn terminal_context(pid: u32) -> Option<crate::domain::TerminalId> {
    #[cfg(target_os = "linux")]
    let environment = std::fs::read(format!("/proc/{pid}/environ")).ok()?;
    #[cfg(target_os = "macos")]
    let environment = {
        let bytes = mac_process_arguments(pid)?;
        let argc = i32::from_ne_bytes(bytes[..4].try_into().ok()?);
        if !(1..=257).contains(&argc) {
            return None;
        }
        let executable_end = bytes[4..].iter().position(|b| *b == 0)? + 4;
        let mut start = bytes[executable_end..].iter().position(|b| *b != 0)? + executable_end;
        for _ in 0..argc {
            start += bytes[start..].iter().position(|b| *b == 0)? + 1;
        }
        bytes[start..].to_vec()
    };
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    let environment: Vec<u8> = {
        let _ = pid;
        return None;
    };
    // Read only the host-owned identity; no captured environment is persisted or replayed.
    environment.split(|b| *b == 0).find_map(|entry| {
        entry
            .strip_prefix(b"FUT_TERMINAL_ID=")
            .and_then(|value| std::str::from_utf8(value).ok()?.parse().ok())
    })
}

pub(super) fn foreground_group(pid: u32) -> Option<u32> {
    #[cfg(target_os = "linux")]
    {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        let group: u32 = stat
            .rsplit_once(')')?
            .1
            .split_whitespace()
            .nth(5)?
            .parse()
            .ok()?;
        (group > 1).then_some(group)
    }
    #[cfg(target_os = "macos")]
    {
        let mut info = std::mem::MaybeUninit::<libc::proc_bsdinfo>::uninit();
        let size = std::mem::size_of::<libc::proc_bsdinfo>() as i32;
        // SAFETY: the bounded buffer is read only after proc_pidinfo fills it.
        if unsafe {
            libc::proc_pidinfo(
                pid.try_into().ok()?,
                libc::PROC_PIDTBSDINFO,
                0,
                info.as_mut_ptr().cast(),
                size,
            )
        } != size
        {
            return None;
        }
        Some(unsafe { info.assume_init() }.e_tpgid).filter(|pid| *pid > 1)
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = pid;
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agents_drop_sessions_and_prompts_but_keep_interpreter_entry_points() {
        for (program, argv0, args, expected) in [
            (
                "/usr/bin/pi",
                "pi",
                vec!["--resume", "abc", "private prompt"],
                vec![],
            ),
            (
                "/home/me/.local/share/claude/versions/2.1.7",
                "2.1.7",
                vec!["--continue"],
                vec![],
            ),
            (
                "/usr/bin/node",
                "node",
                vec![
                    "--no-warnings",
                    "/node_modules/pi-coding-agent/dist/cli.js",
                    "--session",
                    "secret",
                    "prompt",
                ],
                vec!["--no-warnings", "/node_modules/pi-coding-agent/dist/cli.js"],
            ),
            (
                "/usr/bin/node",
                "node",
                vec![
                    "/home/me/.local/bin/pi",
                    "--resume",
                    "secret",
                    "private prompt",
                ],
                vec!["/home/me/.local/bin/pi"],
            ),
            (
                "/usr/bin/node",
                "node",
                vec![
                    "/usr/local/bin/codex.js",
                    "resume",
                    "secret",
                    "private prompt",
                ],
                vec!["/usr/local/bin/codex.js"],
            ),
            (
                "/usr/bin/python3.13",
                "python3",
                vec!["/home/me/.local/bin/aider", "--restore-chat-history"],
                vec!["/home/me/.local/bin/aider"],
            ),
            (
                "/usr/bin/python3.13",
                "python3",
                vec!["-m", "aider", "--message", "private prompt"],
                vec!["-m", "aider"],
            ),
            (
                "/usr/bin/editor",
                "editor",
                vec!["two words", "", "quoted ' argument"],
                vec!["two words", "", "quoted ' argument"],
            ),
        ] {
            let mut command = RestartProcess {
                program: program.into(),
                argv0: argv0.into(),
                args: args.into_iter().map(str::to_owned).collect(),
            };
            sanitize_agent_arguments(&mut command);
            assert_eq!(command.args, expected, "{program}");
        }
    }

    #[test]
    fn executable_aliases_keep_the_invoked_utility_name() {
        assert_eq!(
            restart_executable(&std::fs::canonicalize("/bin/sleep").unwrap(), "/bin/sleep"),
            Some("/bin/sleep".into())
        );
        let temporary = tempfile::tempdir().unwrap();
        let other = temporary.path().join("sleep");
        std::fs::copy("/bin/sleep", &other).unwrap();
        assert_eq!(
            restart_executable(
                &std::fs::canonicalize("/bin/sleep").unwrap(),
                other.to_str().unwrap()
            ),
            None
        );
    }

    #[test]
    fn native_arguments_preserve_spaces_quotes_and_empty_values() {
        let mut child = std::process::Command::new("/bin/sh")
            .args([
                "-c",
                "while :; do sleep 1; done",
                "quoted ' value",
                "",
                "two words",
            ])
            .spawn()
            .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        let observed = loop {
            let observed = raw_restart_process(child.id());
            if observed.is_some() || std::time::Instant::now() >= deadline {
                break observed;
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        };
        let restart = restart_process(child.id());
        child.kill().unwrap();
        child.wait().unwrap();
        let observed = observed.expect("native arguments should become available after exec");
        assert_eq!(
            observed.args,
            [
                "-c",
                "while :; do sleep 1; done",
                "quoted ' value",
                "",
                "two words"
            ]
        );
        assert!(restart.is_none());
    }

    #[test]
    fn lineage_reaches_the_requested_ancestor() {
        let pid = std::process::id();
        let lineage = lineage(pid, pid).unwrap();
        assert_eq!(lineage, [pid]);
        assert_eq!(descendant_root(pid, pid), Some(pid));
    }

    #[test]
    fn lineage_rejects_an_unrelated_process() {
        assert!(lineage(std::process::id(), u32::MAX).is_none());
    }
}
