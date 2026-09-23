//! Private, protocol-blind SSH transport. Standalone attachment may bootstrap
//! a missing daemon on the remote host; background probes remain attach-only.

use std::{
    fs::File,
    io::{self, Read, Write},
    os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd, RawFd},
    os::unix::net::UnixStream as StdUnixStream,
    process::Stdio,
    sync::{Arc, Mutex},
};

use anyhow::{Context, Result, bail};
use tokio::{
    io::AsyncReadExt,
    net::UnixStream,
    process::{Child, Command},
};

/// This runs on a blocking worker, using poll rather than Tokio's stdin helper:
/// a blocked stdin helper can keep the runtime alive after the daemon disconnects.
/// EOF on either input ends the attachment once that direction's buffer drains.
/// There is deliberately no frame parsing or handshake here.
pub(crate) fn run_stdio(socket: StdUnixStream) -> Result<()> {
    let stdin = duplicate(libc::STDIN_FILENO)?;
    let stdout = duplicate(libc::STDOUT_FILENO)?;
    let _stdin_flags = Nonblocking::new(stdin.as_raw_fd())?;
    let _stdout_flags = Nonblocking::new(stdout.as_raw_fd())?;
    socket.set_nonblocking(true)?;
    let mut upstream = Pump::new(&stdin, socket.try_clone()?);
    let mut downstream = Pump::new(socket, &stdout);
    loop {
        let mut fds = [upstream.poll_fd(), downstream.poll_fd()];
        // SAFETY: fds points to two initialized pollfd values for this call.
        let result = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as _, -1) };
        if result < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error).context("poll bridge");
        }
        if fds[0].revents != 0 && upstream.advance().context("forward stdin to daemon")? {
            return Ok(());
        }
        if fds[1].revents != 0 && downstream.advance().context("forward daemon to stdout")? {
            return Ok(());
        }
    }
}

fn duplicate(fd: RawFd) -> io::Result<File> {
    // SAFETY: fcntl validates fd and returns a new descriptor owned by us.
    let copy = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 0) };
    if copy < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: copy is a valid, uniquely owned descriptor after successful fcntl.
    Ok(unsafe { File::from_raw_fd(copy) })
}

struct Nonblocking {
    fd: RawFd,
    flags: libc::c_int,
}

impl Nonblocking {
    fn new(fd: RawFd) -> io::Result<Self> {
        // SAFETY: the caller keeps fd open until this guard is dropped.
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { fd, flags })
    }
}

impl Drop for Nonblocking {
    fn drop(&mut self) {
        // SAFETY: run_stdio keeps the duplicated descriptors open until after
        // these guards are dropped.
        unsafe { libc::fcntl(self.fd, libc::F_SETFL, self.flags) };
    }
}

struct Pump<R, W> {
    reader: R,
    writer: W,
    buffer: [u8; 8192],
    start: usize,
    end: usize,
}

impl<R: Read + AsFd, W: Write + AsFd> Pump<R, W> {
    fn new(reader: R, writer: W) -> Self {
        Self {
            reader,
            writer,
            buffer: [0; 8192],
            start: 0,
            end: 0,
        }
    }

    fn poll_fd(&self) -> libc::pollfd {
        let writing = self.start != self.end;
        libc::pollfd {
            fd: if writing {
                self.writer.as_fd().as_raw_fd()
            } else {
                self.reader.as_fd().as_raw_fd()
            },
            events: if writing { libc::POLLOUT } else { libc::POLLIN },
            revents: 0,
        }
    }

    /// Returns true on EOF. Never reads ahead while a write is pending, bounding
    /// memory and preserving every byte under partial writes/backpressure.
    fn advance(&mut self) -> io::Result<bool> {
        let result = if self.start == self.end {
            self.reader.read(&mut self.buffer).map(|count| {
                self.start = 0;
                self.end = count;
                count == 0
            })
        } else {
            self.writer
                .write(&self.buffer[self.start..self.end])
                .and_then(|count| {
                    if count == 0 {
                        return Err(io::ErrorKind::WriteZero.into());
                    }
                    self.start += count;
                    Ok(false)
                })
        };
        match result {
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                ) =>
            {
                Ok(false)
            }
            result => result,
        }
    }
}

pub(crate) struct SshBridge {
    child: Child,
    // A duplicate lets the owner close the attachment even if a client still
    // holds its UnixStream. An unnamed socket has no path to leak or unlink.
    socket: StdUnixStream,
    diagnostics: tokio::task::JoinHandle<()>,
    diagnostic_text: Arc<Mutex<String>>,
}

impl SshBridge {
    pub(crate) fn connect(destination: &str, start_if_missing: bool) -> Result<(UnixStream, Self)> {
        let mut command = ssh_command(destination, start_if_missing)?;
        Self::spawn(&mut command)
    }

    /// Background supervision must never prompt through the user's terminal.
    pub(crate) fn connect_background(destination: &str) -> Result<(UnixStream, Self)> {
        let mut command = background_ssh_command(destination)?;
        Self::spawn_with_diagnostics(&mut command, false)
    }

    fn spawn(command: &mut Command) -> Result<(UnixStream, Self)> {
        Self::spawn_with_diagnostics(command, true)
    }

    fn spawn_with_diagnostics(
        command: &mut Command,
        echo_diagnostics: bool,
    ) -> Result<(UnixStream, Self)> {
        let (local, remote) = StdUnixStream::pair().context("create private SSH socket pair")?;
        local.set_nonblocking(true)?;
        let stream = UnixStream::from_std(local.try_clone()?)?;
        let mut child = command
            .stdin(Stdio::from(OwnedFd::from(remote.try_clone()?)))
            .stdout(Stdio::from(OwnedFd::from(remote)))
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .context("spawn SSH bridge")?;
        let mut stderr = child.stderr.take().expect("piped SSH stderr");
        let diagnostic_text = Arc::new(Mutex::new(String::new()));
        let captured = diagnostic_text.clone();
        let diagnostics = tokio::spawn(async move {
            let mut buffer = [0; 4096];
            while let Ok(count) = stderr.read(&mut buffer).await {
                if count == 0 {
                    break;
                }
                // Remote stderr is not a terminal protocol channel. In particular,
                // it must not bypass the client's OSC 8 policy or alter tty state.
                let text = sanitize_diagnostics(&buffer[..count]);
                if echo_diagnostics {
                    let _ = io::stderr().write_all(text.as_bytes());
                }
                let mut output = captured.lock().expect("SSH diagnostic lock");
                for character in text.chars() {
                    if output.len() + character.len_utf8() > MAX_DIAGNOSTIC_BYTES {
                        break;
                    }
                    output.push(character);
                }
            }
        });
        Ok((
            stream,
            Self {
                child,
                socket: local,
                diagnostics,
                diagnostic_text,
            },
        ))
    }

    pub(crate) fn diagnostic(&self) -> String {
        self.diagnostic_text
            .lock()
            .expect("SSH diagnostic lock")
            .clone()
    }

    /// Wait for the SSH stderr pipe to close so startup failures can be
    /// classified from complete diagnostics. A stuck SSH process cannot hold
    /// endpoint reconnection indefinitely.
    pub(crate) async fn finish_diagnostics(&mut self, deadline: std::time::Duration) {
        let _ = tokio::time::timeout(deadline, &mut self.diagnostics).await;
    }

    /// Explicit detach: close the attachment, terminate SSH, and reap it before
    /// returning. Drop also kills SSH (Tokio reaps it) if this future is cancelled.
    pub(crate) async fn shutdown(mut self) -> Result<()> {
        let _ = self.socket.shutdown(std::net::Shutdown::Both);
        let result = self.child.kill().await.context("stop SSH bridge");
        if tokio::time::timeout(std::time::Duration::from_secs(1), &mut self.diagnostics)
            .await
            .is_err()
        {
            self.diagnostics.abort();
            let _ = (&mut self.diagnostics).await;
        }
        result
    }
}

const MAX_DIAGNOSTIC_BYTES: usize = 16 * 1024;

impl Drop for SshBridge {
    fn drop(&mut self) {
        let _ = self.socket.shutdown(std::net::Shutdown::Both);
        self.diagnostics.abort();
        // Child's kill_on_drop covers cancellation and runtime teardown.
    }
}

fn sanitize_diagnostics(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes)
        .chars()
        .filter(|character| !character.is_control() || *character == '\n')
        .collect()
}

pub(crate) const MAX_DESTINATION_BYTES: usize = 256;

/// Accepts what OpenSSH resolves itself: a host, alias, or `user@host`.
/// Anything that could carry a secret or be misread as an option is rejected.
pub(crate) fn validate_destination(destination: &str) -> Result<()> {
    if destination.is_empty() || destination.len() > MAX_DESTINATION_BYTES {
        bail!("SSH destination must be 1 to {MAX_DESTINATION_BYTES} bytes");
    }
    if destination
        .chars()
        .any(|character| character.is_whitespace() || character.is_control())
    {
        bail!("SSH destination must not contain whitespace or control characters");
    }
    if destination.starts_with('-') {
        bail!("SSH destination must not start with a dash");
    }
    if destination.contains("://") {
        bail!("SSH destination must be a host or config alias, not a URI");
    }
    if let Some((user, host)) = destination.rsplit_once('@') {
        if user.is_empty() || host.is_empty() || user.contains('@') {
            bail!("SSH destination must contain a valid user@host pair");
        }
        if user.contains(':') {
            bail!("SSH destination must not embed a password");
        }
    }
    Ok(())
}

fn ssh_command(destination: &str, start_if_missing: bool) -> Result<Command> {
    validate_destination(destination)?;
    let mut command = Command::new("ssh");
    // Let OpenSSH resolve aliases, identities, ProxyJump, and other user config.
    // Only this fixed string reaches the remote shell; no destination/path is
    // interpolated into it. Remote socket selection uses the normal environment.
    command.args([
        "-T",
        "--",
        destination,
        if start_if_missing {
            "fut __stdio-bridge --start-if-missing"
        } else {
            "fut __stdio-bridge"
        },
    ]);
    Ok(command)
}

fn background_ssh_command(destination: &str) -> Result<Command> {
    validate_destination(destination)?;
    let mut command = Command::new("ssh");
    command.args([
        "-T",
        "-o",
        "BatchMode=yes",
        "-o",
        "StrictHostKeyChecking=yes",
        "--",
        destination,
        "fut __stdio-bridge",
    ]);
    command.kill_on_drop(true);
    Ok(command)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        time::{Duration, timeout},
    };

    #[test]
    fn remote_stderr_cannot_emit_terminal_controls_or_local_links() {
        let text = sanitize_diagnostics(
            b"error: \x1b]8;;file:///tmp/local\x07link\x1b]8;;\x07\n\xc2\x9b2J",
        );
        assert!(!text.chars().any(|c| c.is_control() && c != '\n'));
        assert!(text.contains("error:"));
    }

    #[tokio::test]
    async fn shutdown_reaps_an_already_exited_ssh_child() {
        let (_stream, mut bridge) = SshBridge::spawn(&mut Command::new("/usr/bin/true")).unwrap();
        bridge.child.wait().await.unwrap();
        bridge.shutdown().await.unwrap();
    }

    #[test]
    fn ssh_uses_direct_argv_and_a_fixed_remote_command() {
        for host in ["work", "user@host", "user@[::1]", "host;echo"] {
            let command = ssh_command(host, true).unwrap();
            assert_eq!(command.as_std().get_program(), "ssh");
            assert_eq!(
                command.as_std().get_args().collect::<Vec<_>>(),
                ["-T", "--", host, "fut __stdio-bridge --start-if-missing"]
            );
            assert_eq!(
                ssh_command(host, false)
                    .unwrap()
                    .as_std()
                    .get_args()
                    .collect::<Vec<_>>(),
                ["-T", "--", host, "fut __stdio-bridge"]
            );
            let background = background_ssh_command(host).unwrap();
            assert_eq!(
                background.as_std().get_args().collect::<Vec<_>>(),
                [
                    "-T",
                    "-o",
                    "BatchMode=yes",
                    "-o",
                    "StrictHostKeyChecking=yes",
                    "--",
                    host,
                    "fut __stdio-bridge",
                ]
            );
        }
        for invalid in [
            "",
            "-oProxyCommand=bad",
            "two hosts",
            "host\ncommand",
            "host\0",
            "host\x1b[2J",
            "ssh://user@host:22",
            "user:password@host",
            &"h".repeat(MAX_DESTINATION_BYTES + 1),
        ] {
            assert!(ssh_command(invalid, true).is_err(), "{invalid:?}");
        }
        assert!(ssh_command("hôte", true).is_ok());
    }

    #[tokio::test]
    async fn private_socket_pair_roundtrips_and_shutdown_reaps_child() {
        let (mut stream, bridge) = SshBridge::spawn(Command::new("/bin/cat").arg("-")).unwrap();
        assert!(stream.local_addr().unwrap().is_unnamed());
        assert!(stream.peer_addr().unwrap().is_unnamed());
        let pid = bridge.child.id().unwrap();
        stream.write_all(b"\0\xffraw bytes\n").await.unwrap();
        let mut bytes = [0; 12];
        timeout(Duration::from_secs(5), stream.read_exact(&mut bytes))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(&bytes, b"\0\xffraw bytes\n");
        timeout(Duration::from_secs(5), bridge.shutdown())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stream.read(&mut bytes).await.unwrap(), 0);
        // SAFETY: kill(pid, 0) only checks process existence.
        assert_eq!(unsafe { libc::kill(pid as _, 0) }, -1);
    }

    #[tokio::test]
    async fn dropping_owner_closes_stream_and_terminates_child() {
        let (mut stream, bridge) = SshBridge::spawn(Command::new("/bin/cat").arg("-")).unwrap();
        let pid = bridge.child.id().unwrap();
        drop(bridge);
        timeout(Duration::from_secs(5), async {
            assert_eq!(stream.read(&mut [0]).await.unwrap(), 0);
            // SAFETY: kill(pid, 0) only checks process existence.
            while unsafe { libc::kill(pid as _, 0) } == 0 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn spawn_failure_is_reported() {
        assert!(SshBridge::spawn(&mut Command::new("/nonexistent/fut-ssh")).is_err());
    }
}
