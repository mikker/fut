use super::*;
use fut::protocol::remote::{self as wire, EndpointError, RemoteHello};
use uuid::Uuid;

fn fake_ssh(root: &std::path::Path) -> PathBuf {
    let bin = root.join("fake-bin");
    fs::create_dir(&bin).unwrap();
    let ssh = bin.join("ssh");
    fs::write(
        &ssh,
        r#"#!/bin/sh
printf '%s\n' "$@" >> "$SSH_ARGS"
printf '%s\n' "$$" >> "$SSH_PIDS"
if [ -n "$SSH_STALL" ]; then exec /bin/sleep 60; fi
if [ -n "$SSH_FAILURE" ]; then
    printf '%s\n' "$SSH_FAILURE" >&2
    exit 255
fi
cd "$HOME" || exit 1
socket="$REMOTE_SOCKET"
for argument in "$@"; do
    case "$argument" in
        alpha) socket="$REMOTE_SOCKET_ALPHA" ;;
        beta) socket="$REMOTE_SOCKET_BETA" ;;
    esac
    remote_command="$argument"
done
case "$remote_command" in
    'fut __stdio-bridge --start-if-missing')
        exec "$FUT_BIN" --no-config --socket "$socket" __stdio-bridge --start-if-missing ;;
    *) exec "$FUT_BIN" --socket "$socket" __stdio-bridge ;;
esac
"#,
    )
    .unwrap();
    fs::set_permissions(&ssh, fs::Permissions::from_mode(0o755)).unwrap();
    bin
}

fn remote_env(
    command: &mut Command,
    root: &std::path::Path,
    bin: &std::path::Path,
    socket: &std::path::Path,
) {
    command
        .env_clear()
        .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
        .env("HOME", root)
        .env("TERM", "xterm-256color")
        .env("FUT_BIN", env!("CARGO_BIN_EXE_fut"))
        .env("REMOTE_SOCKET", socket)
        .env("SSH_ARGS", root.join("ssh-args"))
        .env("SSH_PIDS", root.join("ssh-pids"))
        // These local values must never select the remote endpoint or cause IO.
        .env("FUT_SOCKET", root.join("must-not-be-used.sock"))
        .env("FUT_RUNTIME_DIR", root.join("must-not-be-created"));
}

async fn assert_ssh_reaped(root: &std::path::Path) {
    let pids = fs::read_to_string(root.join("ssh-pids")).unwrap();
    time::timeout(DEADLINE, async {
        for pid in pids.lines().map(|pid| pid.parse::<u32>().unwrap()) {
            while process_alive(pid) {
                time::sleep(POLL_INTERVAL).await;
            }
        }
    })
    .await
    .expect("SSH bridge survived client exit");
}

#[tokio::test]
async fn remote_cli_navigator_attach_detach_preserves_daemon_and_reaps_ssh() {
    let harness = Harness::start_with("printf 'REMOTE_READY\\r\\n'; while IFS= read -r line; do printf 'REMOTE:%s\\r\\n' \"$line\"; done", |root| {
        let extension = root.join("remote-extension");
        fs::create_dir(&extension).unwrap();
        fs::write(extension.join("fut-extension.toml"), r#"
api_version = 1
version = "1.0.0"
fut = ">=0.7.0, <1.0.0"
id = "remote-only"
capabilities = ["hooks", "commands"]
[hooks]
"client.attached" = ["./run"]
"client.session_changed" = ["./run"]
"client.detached" = ["./run"]
[commands.attack]
title = "Remote attack"
argv = ["./run"]
"#).unwrap();
        let script = extension.join("run");
        fs::write(&script, format!("#!/bin/sh\ntouch '{}'\n", root.join("client-hook-ran").display())).unwrap();
        fs::set_permissions(script, fs::Permissions::from_mode(0o755)).unwrap();
        fs::create_dir_all(root.join("home/.config/fut")).unwrap();
        fs::write(root.join("home/.config/fut/config.toml"), format!("extensions = [{:?}]\n", extension.to_str().unwrap())).unwrap();
    }).await;
    let root = tempfile::tempdir().unwrap();
    let bin = fake_ssh(root.path());
    let before = harness.resources().await;
    for tail in ["", "attach"] {
        let mut command = Command::new("/usr/bin/script");
        remote_env(&mut command, root.path(), &bin, &harness.socket);
        command.args(script_command_args()).arg(format!(
            "stty cols 80 rows 24; exec \"$FUT_BIN\" --no-config --remote clonk {tail}"
        ));
        let mut client = PtyChild::spawn(command);
        client.wait_for("navigator").await;
        client.send(b"\r");
        client.wait_for("REMOTE_READY").await;
        client.send(b"ping\r");
        client.wait_for("REMOTE:ping").await;
        // All of these used to resolve workspace roots/config in the client.
        client.send(b"\x02S");
        client.wait_for("project opener unavailable").await;
        client.send(b"\x02:");
        client.send(b"Remote attack");
        client.send(b"\r");
        client.wait_for("commands unavailable").await;
        client.send(b"\x02d");
        client.wait_success().await;
        assert_ssh_reaped(root.path()).await;
        assert!(!harness.root.path().join("client-hook-ran").exists());
        assert_eq!(
            without_observations(harness.resources().await),
            without_observations(before.clone())
        );
        assert!(!root.path().join("must-not-be-created").exists());
    }
    let argv = fs::read_to_string(root.path().join("ssh-args")).unwrap();
    assert_eq!(
        argv,
        "-T\n--\nclonk\nfut __stdio-bridge --start-if-missing\n".repeat(4)
    );
    assert!(matches!(
        harness.control_command(ClientMessage::Ping).await,
        ServerMessage::Pong { .. }
    ));
    harness.shutdown().await;
}

#[tokio::test]
async fn local_client_switches_atomically_between_two_saved_remote_endpoints() {
    let local = Harness::start("printf 'LOCAL_READY\\r\\n'; while IFS= read -r line; do printf 'LOCAL:%s\\r\\n' \"$line\"; done").await;
    let alpha = Harness::start("printf 'ALPHA_READY\\r\\n'; while IFS= read -r line; do printf 'ALPHA:%s\\r\\n' \"$line\"; done").await;
    let beta = Harness::start("printf 'BETA_READY\\r\\n'; while IFS= read -r line; do printf 'BETA:%s\\r\\n' \"$line\"; done").await;
    let root = tempfile::tempdir().unwrap();
    let bin = fake_ssh(root.path());
    let state = root.path().join("state/fut");
    fs::create_dir_all(&state).unwrap();
    let machines = format!(
        "version = 1\n\n[[machines]]\nid = \"{}\"\nlabel = \"alpha\"\ntarget = \"alpha\"\nenabled = true\n\n[[machines]]\nid = \"{}\"\nlabel = \"beta\"\ntarget = \"beta\"\nenabled = true\n",
        Uuid::new_v4(),
        Uuid::new_v4(),
    );
    let catalog = state.join("machines.toml");
    fs::write(&catalog, machines).unwrap();
    fs::set_permissions(&catalog, fs::Permissions::from_mode(0o600)).unwrap();

    let mut command = Command::new("/usr/bin/script");
    remote_env(&mut command, root.path(), &bin, &alpha.socket);
    command
        .env("XDG_STATE_HOME", root.path().join("state"))
        .env("REMOTE_SOCKET_ALPHA", &alpha.socket)
        .env("REMOTE_SOCKET_BETA", &beta.socket)
        .args(script_command_args())
        .arg(format!(
            "stty cols 80 rows 24; exec \"$FUT_BIN\" --no-config --socket {} attach",
            local.socket.display()
        ));
    let mut client = PtyChild::spawn(command);
    client.wait_for("navigator").await;
    client.send(b"\r");
    client.wait_for("LOCAL_READY").await;

    client.send(b"\x02s");
    client.wait_for("alpha · Online").await;
    client.send(b"alpha\r");
    client.wait_for("ALPHA_READY").await;
    client.send(b"one\r");
    client.wait_for("ALPHA:one").await;

    client.send(b"\x02s");
    client.wait_for("beta · Online").await;
    client.send(b"beta\r");
    client.wait_for("BETA_READY").await;
    client.send(b"two\r");
    client.wait_for("BETA:two").await;
    assert!(!client.text().contains("ALPHA:two"), "{}", client.text());
    assert!(!client.text().contains("LOCAL:two"), "{}", client.text());

    client.send(b"\x02d");
    client.wait_success().await;
    assert_ssh_reaped(root.path()).await;
    assert!(matches!(
        local.control_command(ClientMessage::Ping).await,
        ServerMessage::Pong { .. }
    ));
    assert!(matches!(
        alpha.control_command(ClientMessage::Ping).await,
        ServerMessage::Pong { .. }
    ));
    assert!(matches!(
        beta.control_command(ClientMessage::Ping).await,
        ServerMessage::Pong { .. }
    ));
    local.shutdown().await;
    alpha.shutdown().await;
    beta.shutdown().await;
}

#[tokio::test]
async fn remote_cli_navigates_and_attaches_across_versions_without_optional_capabilities() {
    let harness = Harness::start("printf 'OPTIONAL_READY\\r\\n'; while IFS= read -r line; do printf 'OPTIONAL:%s\\r\\n' \"$line\"; done").await;
    let root = tempfile::tempdir().unwrap();
    let bin = fake_ssh(root.path());
    let socket = root.path().join("compatible-peer.sock");
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    let daemon_socket = harness.socket.clone();
    // Simulate a generation-1 implementation from another package release
    // which implements only the required capabilities. The real SSH bridge
    // still forwards opaque bytes; this shim exists only in the test peer.
    let peer = tokio::spawn(async move {
        for interactive in [false, true] {
            let (stream, _) = listener.accept().await.unwrap();
            let mut client = Framed::new(stream, codec());
            let mut daemon =
                Framed::new(UnixStream::connect(&daemon_socket).await.unwrap(), codec());
            let mut hello: Envelope<ClientMessage> =
                decode_payload(&client.next().await.unwrap().unwrap()).unwrap();
            let ClientMessage::RemoteHello(offer) = &mut hello.message else {
                panic!("expected remote hello")
            };
            assert_eq!(
                matches!(offer.mode, ClientMode::Interactive { .. }),
                interactive
            );
            offer.client_version = "0.1.0".into();
            offer.optional.clear();
            daemon
                .send(Bytes::from(encode_payload(&hello).unwrap()))
                .await
                .unwrap();
            let mut welcome: Envelope<ServerMessage> =
                decode_payload(&daemon.next().await.unwrap().unwrap()).unwrap();
            let ServerMessage::RemoteWelcome(selected) = &mut welcome.message else {
                panic!("expected remote welcome")
            };
            selected.server_version = "0.999.0".into();
            assert!(selected.extension_catalog.is_none());
            client
                .send(Bytes::from(encode_payload(&welcome).unwrap()))
                .await
                .unwrap();
            loop {
                tokio::select! {
                    frame = client.next() => {
                        let Some(Ok(frame)) = frame else { break };
                        let message: Envelope<ClientMessage> = decode_payload(&frame).unwrap();
                        assert!(!matches!(message.message, ClientMessage::WatchAlerts { .. } | ClientMessage::GetExtensionCatalog | ClientMessage::Ping));
                        daemon.send(frame.freeze()).await.unwrap();
                    }
                    frame = daemon.next() => {
                        let Some(Ok(frame)) = frame else { break };
                        if client.send(frame.freeze()).await.is_err() { break; }
                    }
                }
            }
        }
    });
    let mut command = Command::new("/usr/bin/script");
    remote_env(&mut command, root.path(), &bin, &socket);
    command
        .args(script_command_args())
        .arg("stty cols 80 rows 24; exec \"$FUT_BIN\" --no-config --remote clonk");
    let mut client = PtyChild::spawn(command);
    client.wait_for("navigator").await;
    client.send(b"\r");
    client.wait_for("OPTIONAL_READY").await;
    client.wait_for("bash").await;
    client.send(b"ping\r");
    client.wait_for("OPTIONAL:ping").await;
    client.send(b"\x02S");
    client.wait_for("project opener unavailable").await;
    client.send(b"\x02d");
    client.wait_success().await;
    assert_ssh_reaped(root.path()).await;
    time::timeout(DEADLINE, peer).await.unwrap().unwrap();
    assert!(matches!(
        harness.control_command(ClientMessage::Ping).await,
        ServerMessage::Pong { .. }
    ));
    assert!(!root.path().join("must-not-be-created").exists());
    harness.shutdown().await;
}

#[tokio::test]
async fn remote_failures_leave_terminal_untouched_and_reap_ssh() {
    for failure in [
        "Permission denied (publickey).",
        "Host key verification failed.",
        "fut: command not found",
    ] {
        assert_remote_failure(failure).await;
    }
}

#[tokio::test]
async fn remote_missing_daemon_starts_on_remote_and_survives_detach() {
    let root = tempfile::tempdir().unwrap();
    let bin = fake_ssh(root.path());
    let socket = root.path().join("runtime/fut.sock");
    let mut command = Command::new("/usr/bin/script");
    remote_env(&mut command, root.path(), &bin, &socket);
    command
        .args(script_command_args())
        .arg("stty cols 80 rows 24; exec \"$FUT_BIN\" --no-config --remote clonk");
    let mut client = PtyChild::spawn(command);
    client.wait_for("navigator").await;
    client.send(b"\x1b");
    client.wait_success().await;
    assert_ssh_reaped(root.path()).await;
    assert!(
        socket.exists(),
        "remote daemon must outlive the SSH connection"
    );
    let status = tokio::process::Command::new(env!("CARGO_BIN_EXE_fut"))
        .args(["--no-config", "--socket"])
        .arg(&socket)
        .args(["daemon", "shutdown"])
        .status()
        .await
        .unwrap();
    assert!(status.success());
    assert!(!root.path().join("must-not-be-created").exists());
}

#[tokio::test]
async fn remote_attach_only_flag_and_local_config_never_start_a_missing_daemon() {
    let root = tempfile::tempdir().unwrap();
    let bin = fake_ssh(root.path());
    let socket = root.path().join("runtime/fut.sock");
    let config = root.path().join("config");
    fs::create_dir(&config).unwrap();
    fs::write(config.join("config.toml"), "[remote]\nautostart = false\n").unwrap();

    for arguments in [
        "--no-config --attach-only".to_owned(),
        format!("--config-dir {}", config.display()),
    ] {
        let mut command = Command::new("/usr/bin/script");
        remote_env(&mut command, root.path(), &bin, &socket);
        command.args(script_command_args()).arg(format!(
            "before=$(stty -g); \"$FUT_BIN\" {arguments} --remote clonk attach; code=$?; after=$(stty -g); [ \"$before\" = \"$after\" ] && printf 'TERM_UNCHANGED\\n'; printf 'REMOTE_EXIT:%s\\n' \"$code\""
        ));
        let mut client = PtyChild::spawn(command);
        client.wait_success().await;
        wait_for(DEADLINE, || client.text().contains("REMOTE_EXIT:1")).await;
        let output = client.text();
        assert!(output.contains("TERM_UNCHANGED"), "{output}");
        assert!(output.contains("connect bridge to"), "{output}");
        assert!(!output.contains('\x1b'), "{output}");
        assert!(!socket.exists());
    }
    assert_eq!(
        fs::read_to_string(root.path().join("ssh-args")).unwrap(),
        "-T\n--\nclonk\nfut __stdio-bridge\n".repeat(2)
    );
    assert_ssh_reaped(root.path()).await;
    assert!(!root.path().join("must-not-be-created").exists());
}

async fn assert_remote_failure(failure: &str) {
    let root = tempfile::tempdir().unwrap();
    let bin = fake_ssh(root.path());
    let socket = root.path().join("missing.sock");
    let mut command = Command::new("/usr/bin/script");
    remote_env(&mut command, root.path(), &bin, &socket);
    command
        .env("SSH_FAILURE", failure)
        .args(script_command_args())
        .arg(
            r#"
before=$(stty -g)
"$FUT_BIN" --no-config --remote clonk
code=$?
after=$(stty -g)
[ "$before" = "$after" ] && printf 'TERM_UNCHANGED\n'
printf 'REMOTE_EXIT:%s\n' "$code"
"#,
        );
    let mut client = PtyChild::spawn(command);
    client.wait_success().await;
    // Wait for the asynchronous reader to drain the final pipe bytes.
    wait_for(DEADLINE, || client.text().contains("REMOTE_EXIT:1")).await;
    assert!(
        client.text().contains("TERM_UNCHANGED"),
        "{}",
        client.text()
    );
    assert!(
        !client.text().contains('\x1b'),
        "terminal setup occurred before failed handshake"
    );
    let output = client.text();
    assert!(output.contains("remote attachment failed"), "{output}");
    assert!(output.contains(failure), "{output}");
    assert!(!socket.exists());
    assert!(!root.path().join("must-not-be-created").exists());
    assert_ssh_reaped(root.path()).await;
}

#[tokio::test]
async fn remote_cli_protocol_mismatch_never_retries_or_changes_terminal() {
    let root = tempfile::tempdir().unwrap();
    let bin = fake_ssh(root.path());
    let socket = root.path().join("daemon.sock");
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut connection = Connection::new(stream);
        let request = connection.next().await.unwrap().unwrap();
        let hello: Envelope<ClientMessage> = decode_payload(&request).unwrap();
        assert!(matches!(
            hello.message,
            ClientMessage::RemoteHello(RemoteHello {
                generation: wire::GENERATION,
                mode: ClientMode::Control,
                ..
            })
        ));
        connection
            .send(Bytes::from(
                encode_payload(&Envelope {
                    request_id: hello.request_id,
                    message: ServerMessage::EndpointError {
                        error: EndpointError::IncompatibleGeneration {
                            client: wire::GENERATION,
                            server: wire::GENERATION + 1,
                        },
                    },
                })
                .unwrap(),
            ))
            .await
            .unwrap();
        assert!(connection.next().await.is_none());
        assert!(
            time::timeout(Duration::from_millis(100), listener.accept())
                .await
                .is_err()
        );
    });
    let mut command = Command::new(env!("CARGO_BIN_EXE_fut"));
    remote_env(&mut command, root.path(), &bin, &socket);
    command.args(["--no-config", "--remote", "clonk"]);
    let output = tokio::task::spawn_blocking(move || command.output().unwrap())
        .await
        .unwrap();
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(error.contains("incompatible remote generation"), "{error}");
    assert!(!error.contains("shutdown"));
    assert!(!error.contains('\x1b'));
    assert_ssh_reaped(root.path()).await;
    assert_eq!(
        fs::read_to_string(root.path().join("ssh-pids"))
            .unwrap()
            .lines()
            .count(),
        1
    );
    server.await.unwrap();
}

#[test]
fn remote_cli_restrictions_precede_all_local_special_cases() {
    let root = tempfile::tempdir().unwrap();
    let bin = fake_ssh(root.path());
    for tail in [
        vec!["doctor"],
        vec!["trust", "status"],
        vec!["open", "/tmp"],
        vec!["daemon", "run"],
        vec!["attach", "--ignore-protocol-mismatch"],
        vec!["__stdio-bridge"],
    ] {
        let mut command = Command::new(env!("CARGO_BIN_EXE_fut"));
        remote_env(
            &mut command,
            root.path(),
            &bin,
            &root.path().join("missing.sock"),
        );
        let output = command
            .args(["--remote", "clonk"])
            .args(tail)
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
        assert!(String::from_utf8_lossy(&output.stderr).contains("--remote"));
    }
    assert!(!root.path().join("ssh-pids").exists());
    assert!(!root.path().join("must-not-be-created").exists());
}

#[tokio::test]
async fn remote_cancelled_handshake_reaps_ssh_without_terminal_setup() {
    let root = tempfile::tempdir().unwrap();
    let bin = fake_ssh(root.path());
    let mut command = Command::new(env!("CARGO_BIN_EXE_fut"));
    remote_env(
        &mut command,
        root.path(),
        &bin,
        &root.path().join("unused.sock"),
    );
    command
        .env("SSH_STALL", "1")
        .args(["--no-config", "--remote", "clonk"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let child = command.spawn().unwrap();
    wait_for_nonempty_file(&root.path().join("ssh-pids")).await;
    // SAFETY: this PID belongs to the child created above; only the client is
    // signalled, so SSH cleanup must be performed by the client itself.
    assert_eq!(unsafe { libc::kill(child.id() as _, libc::SIGTERM) }, 0);
    let output = time::timeout(
        DEADLINE,
        tokio::task::spawn_blocking(move || child.wait_with_output().unwrap()),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("terminated by SIGTERM"));
    assert_ssh_reaped(root.path()).await;
}

#[test]
fn remote_nested_client_guard_precedes_ssh_attachment() {
    let root = tempfile::tempdir().unwrap();
    let bin = fake_ssh(root.path());
    for tail in [vec![], vec!["attach"]] {
        for allow_nested in [false, true] {
            let mut command = Command::new(env!("CARGO_BIN_EXE_fut"));
            remote_env(
                &mut command,
                root.path(),
                &bin,
                &root.path().join("missing.sock"),
            );
            command
                .env("FUT_TERMINAL_ID", "existing-terminal")
                .env("SSH_FAILURE", "nested override reached SSH");
            if allow_nested {
                command.env("FUT_ALLOW_NESTED", "1");
            }
            let output = command
                .args(["--no-config", "--remote", "clonk"])
                .args(&tail)
                .output()
                .unwrap();
            assert!(!output.status.success());
            assert!(output.stdout.is_empty());
            let error = String::from_utf8_lossy(&output.stderr);
            if allow_nested {
                assert!(error.contains("nested override reached SSH"), "{error}");
                assert!(!error.contains("clients should be nested"), "{error}");
                fs::remove_file(root.path().join("ssh-pids")).unwrap();
                fs::remove_file(root.path().join("ssh-args")).unwrap();
            } else {
                assert!(
                    error.contains("clients should be nested with care"),
                    "{error}"
                );
                assert!(error.contains("FUT_ALLOW_NESTED"), "{error}");
                assert!(!root.path().join("ssh-pids").exists());
                assert!(!root.path().join("ssh-args").exists());
            }
            assert!(!root.path().join("must-not-be-created").exists());
        }
    }
}

fn machine_env(
    command: &mut Command,
    root: &std::path::Path,
    bin: &std::path::Path,
    socket: &std::path::Path,
) {
    remote_env(command, root, bin, socket);
    command.env("XDG_STATE_HOME", root.join("state"));
}

fn last_stderr_json(output: &std::process::Output) -> Value {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let line = stderr.lines().last().unwrap_or_default();
    serde_json::from_str(line).unwrap_or_else(|error| panic!("{error}: {stderr}"))
}

fn machine_catalog(root: &std::path::Path) -> PathBuf {
    root.join("state/fut/machines.toml")
}

#[tokio::test]
async fn machine_add_confirms_the_remote_daemon_then_saves_and_other_commands_stay_local() {
    let harness = Harness::start("while IFS= read -r line; do :; done").await;
    let root = tempfile::tempdir().unwrap();
    let bin = fake_ssh(root.path());
    let before = harness.resources().await;

    // Verification runs in cooked mode and leaves the terminal untouched.
    let mut command = Command::new("/usr/bin/script");
    machine_env(&mut command, root.path(), &bin, &harness.socket);
    command.args(script_command_args()).arg(
        r#"
before=$(stty -g)
"$FUT_BIN" --no-config machine add clonk --label work
code=$?
after=$(stty -g)
[ "$before" = "$after" ] && printf 'TERM_UNCHANGED\n'
printf 'MACHINE_EXIT:%s\n' "$code"
"#,
    );
    let mut client = PtyChild::spawn(command);
    client.wait_success().await;
    time::timeout(DEADLINE, async {
        while !client.text().contains("MACHINE_EXIT:") {
            time::sleep(POLL_INTERVAL).await;
        }
    })
    .await
    .unwrap();
    let text = client.text();
    assert!(text.contains("MACHINE_EXIT:0"), "{text}");
    assert!(text.contains("TERM_UNCHANGED"), "{text}");
    assert!(!text.contains('\x1b'), "{text}");
    assert!(
        text.contains("added machine work target=clonk enabled=true id="),
        "{text}"
    );
    assert!(
        text.contains(&format!("remote_version={}", env!("CARGO_PKG_VERSION"))),
        "{text}"
    );
    assert_ssh_reaped(root.path()).await;
    assert_eq!(
        fs::read_to_string(root.path().join("ssh-args")).unwrap(),
        "-T\n--\nclonk\nfut __stdio-bridge\n"
    );
    let catalog = machine_catalog(root.path());
    assert_eq!(
        fs::metadata(&catalog).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert!(!root.path().join("must-not-be-created").exists());
    assert_eq!(
        without_observations(harness.resources().await),
        without_observations(before)
    );

    // Every other command is a local catalog edit: SSH is unreachable from here.
    let machine = |arguments: &[&str]| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_fut"));
        machine_env(
            &mut command,
            root.path(),
            &root.path().join("no-ssh-here"),
            &harness.socket,
        );
        command.env("PATH", root.path().join("no-ssh-here"));
        command.arg("--json").arg("machine").args(arguments);
        command.output().unwrap()
    };
    let json = |output: std::process::Output, command: &str| {
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let text = String::from_utf8(output.stdout).unwrap();
        assert_eq!(text.lines().count(), 1, "{text:?}");
        let value: Value = serde_json::from_str(text.trim()).unwrap();
        assert_eq!(value["version"], 1);
        assert_eq!(value["command"], command);
        value["result"].clone()
    };
    let json_error = |output: std::process::Output, code: &str| {
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
        let value = last_stderr_json(&output);
        assert_eq!(value["version"], 1);
        assert_eq!(value["error"]["code"], code, "{value}");
    };

    json_error(machine(&["add", "clonk", "--label", "work"]), "label_taken");
    json_error(
        machine(&["add", "clonk", "--label", "-bad"]),
        "invalid_arguments",
    );
    json_error(
        machine(&["add", "user:secret@clonk", "--label", "leak"]),
        "invalid_arguments",
    );
    // A second profile for the same target is allowed, but SSH is unavailable
    // here, so verification fails and nothing is saved.
    json_error(
        machine(&["add", "clonk", "--label", "second"]),
        "command_failed",
    );

    let list = json(machine(&["list"]), "machine.list");
    assert_eq!(list["machines"].as_array().unwrap().len(), 1);
    assert!(
        list["path"]
            .as_str()
            .unwrap()
            .ends_with("state/fut/machines.toml")
    );
    let saved = &list["machines"][0];
    let id = Uuid::parse_str(saved["id"].as_str().unwrap()).unwrap();
    assert_eq!(saved["label"], "work");
    assert_eq!(saved["target"], "clonk");
    assert_eq!(saved["enabled"], true);
    assert_eq!(saved.as_object().unwrap().len(), 4, "{saved}");

    let shown = json(machine(&["show", &id.to_string()]), "machine.show");
    assert_eq!(shown["machine"], *saved);
    let renamed = json(machine(&["rename", "work", "office"]), "machine.rename");
    assert_eq!(renamed["changed"], true);
    assert_eq!(renamed["machine"]["id"], id.to_string());
    assert_eq!(renamed["machine"]["label"], "office");
    json_error(machine(&["show", "work"]), "not_found");
    let disabled = json(machine(&["disable", "office"]), "machine.disable");
    assert_eq!(disabled["changed"], true);
    assert_eq!(disabled["machine"]["enabled"], false);
    assert_eq!(
        json(machine(&["disable", "office"]), "machine.disable")["changed"],
        false
    );
    let enabled = json(machine(&["enable", &id.to_string()]), "machine.enable");
    assert_eq!(enabled["changed"], true);
    assert_eq!(enabled["machine"]["enabled"], true);
    let removed = json(machine(&["remove", "office"]), "machine.remove");
    assert_eq!(removed["machine"]["id"], id.to_string());
    json_error(machine(&["remove", "office"]), "not_found");
    assert_eq!(
        json(machine(&["list"]), "machine.list")["machines"],
        Value::Array(Vec::new())
    );

    let text = fs::read_to_string(&catalog).unwrap();
    assert!(text.starts_with("version = 1"), "{text}");
    assert_eq!(
        fs::read_to_string(root.path().join("ssh-pids"))
            .unwrap()
            .lines()
            .count(),
        1,
        "only the verified add may use SSH"
    );
    assert!(matches!(
        harness.control_command(ClientMessage::Ping).await,
        ServerMessage::Pong { .. }
    ));
    harness.shutdown().await;
}

#[tokio::test]
async fn machine_add_accepts_an_unequal_version_without_optional_health_checks() {
    let root = tempfile::tempdir().unwrap();
    let bin = fake_ssh(root.path());
    let socket = root.path().join("compatible-peer.sock");
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    let peer = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut connection = Connection::new(stream);
        let hello: Envelope<ClientMessage> =
            decode_payload(&connection.next().await.unwrap().unwrap()).unwrap();
        let ClientMessage::RemoteHello(offer) = hello.message else {
            panic!("expected remote hello")
        };
        connection
            .send(Bytes::from(
                encode_payload(&Envelope {
                    request_id: hello.request_id,
                    message: ServerMessage::RemoteWelcome(wire::RemoteWelcome {
                        generation: wire::GENERATION,
                        codec: wire::CODEC.into(),
                        server_version: "0.999.0".into(),
                        capabilities: offer.required,
                        selected: None,
                        extension_catalog: None,
                    }),
                })
                .unwrap(),
            ))
            .await
            .unwrap();
        assert!(
            connection.next().await.is_none(),
            "unsupported optional health method was sent"
        );
    });
    let mut command = Command::new(env!("CARGO_BIN_EXE_fut"));
    machine_env(&mut command, root.path(), &bin, &socket);
    command.args(["--json", "--no-config", "machine", "add", "clonk"]);
    let output = tokio::task::spawn_blocking(move || command.output().unwrap())
        .await
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("0.999.0"));
    assert!(machine_catalog(root.path()).exists());
    assert_ssh_reaped(root.path()).await;
    peer.await.unwrap();
}

#[tokio::test]
async fn machine_add_saves_nothing_after_failed_or_cancelled_verification() {
    // SSH failures and a missing remote daemon.
    for failure in [
        Some("Permission denied (publickey)."),
        Some("fut: command not found"),
        None,
    ] {
        let root = tempfile::tempdir().unwrap();
        let bin = fake_ssh(root.path());
        let mut command = Command::new(env!("CARGO_BIN_EXE_fut"));
        machine_env(
            &mut command,
            root.path(),
            &bin,
            &root.path().join("missing.sock"),
        );
        let output = command
            .env("SSH_FAILURE", failure.unwrap_or_default())
            .args(["--json", "--no-config", "machine", "add", "clonk"])
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
        // SSH's own diagnostics pass through ahead of the error envelope.
        let error = last_stderr_json(&output);
        assert_eq!(error["error"]["code"], "command_failed");
        let message = error["error"]["message"].as_str().unwrap();
        assert!(message.contains("remote attachment failed"), "{message}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains(failure.unwrap_or("connect bridge to")),
            "{stderr}"
        );
        assert!(!stderr.contains('\x1b'));
        assert!(!machine_catalog(root.path()).exists());
        assert!(!root.path().join("state").exists());
        assert_ssh_reaped(root.path()).await;
    }

    // An incompatible daemon.
    let root = tempfile::tempdir().unwrap();
    let bin = fake_ssh(root.path());
    let socket = root.path().join("daemon.sock");
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut connection = Connection::new(stream);
        let hello: Envelope<ClientMessage> =
            decode_payload(&connection.next().await.unwrap().unwrap()).unwrap();
        assert!(matches!(
            hello.message,
            ClientMessage::RemoteHello(RemoteHello {
                generation: wire::GENERATION,
                mode: ClientMode::Control,
                ..
            })
        ));
        connection
            .send(Bytes::from(
                encode_payload(&Envelope {
                    request_id: hello.request_id,
                    message: ServerMessage::EndpointError {
                        error: EndpointError::IncompatibleGeneration {
                            client: wire::GENERATION,
                            server: wire::GENERATION + 1,
                        },
                    },
                })
                .unwrap(),
            ))
            .await
            .unwrap();
        assert!(connection.next().await.is_none());
    });
    let mut command = Command::new(env!("CARGO_BIN_EXE_fut"));
    machine_env(&mut command, root.path(), &bin, &socket);
    command.args(["--no-config", "machine", "add", "clonk"]);
    let output = tokio::task::spawn_blocking(move || command.output().unwrap())
        .await
        .unwrap();
    assert!(!output.status.success());
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(error.contains("incompatible remote generation"), "{error}");
    assert!(!machine_catalog(root.path()).exists());
    assert_ssh_reaped(root.path()).await;
    server.await.unwrap();

    // Cancellation while SSH is still authenticating.
    let root = tempfile::tempdir().unwrap();
    let bin = fake_ssh(root.path());
    let mut command = Command::new(env!("CARGO_BIN_EXE_fut"));
    machine_env(
        &mut command,
        root.path(),
        &bin,
        &root.path().join("unused.sock"),
    );
    command
        .env("SSH_STALL", "1")
        .args(["--no-config", "machine", "add", "clonk"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let child = command.spawn().unwrap();
    wait_for_nonempty_file(&root.path().join("ssh-pids")).await;
    // SAFETY: this PID belongs to the child created above.
    assert_eq!(unsafe { libc::kill(child.id() as _, libc::SIGTERM) }, 0);
    let output = time::timeout(
        DEADLINE,
        tokio::task::spawn_blocking(move || child.wait_with_output().unwrap()),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("terminated by SIGTERM"));
    assert!(!machine_catalog(root.path()).exists());
    assert_ssh_reaped(root.path()).await;

    // Cancellation while waiting for a concurrent catalog writer must not
    // cross the catalog commit point after remote verification succeeds.
    use std::os::{fd::AsRawFd, unix::fs::OpenOptionsExt};
    let harness = Harness::start("while IFS= read -r line; do :; done").await;
    let root = tempfile::tempdir().unwrap();
    let bin = fake_ssh(root.path());
    let catalog = machine_catalog(root.path());
    fs::create_dir_all(catalog.parent().unwrap()).unwrap();
    let lock_path = catalog.with_extension("lock");
    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(&lock_path)
        .unwrap();
    // SAFETY: lock owns this descriptor until after the child exits.
    assert_eq!(unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) }, 0);
    let mut command = Command::new(env!("CARGO_BIN_EXE_fut"));
    machine_env(&mut command, root.path(), &bin, &harness.socket);
    command
        .args(["--no-config", "machine", "add", "clonk"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let child = command.spawn().unwrap();
    wait_for_nonempty_file(&root.path().join("ssh-pids")).await;
    assert_ssh_reaped(root.path()).await;
    // SAFETY: this PID belongs to the child created above.
    assert_eq!(unsafe { libc::kill(child.id() as _, libc::SIGTERM) }, 0);
    let output = time::timeout(
        DEADLINE,
        tokio::task::spawn_blocking(move || child.wait_with_output().unwrap()),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("machine setup terminated"));
    assert!(!catalog.exists());
    drop(lock);
    harness.shutdown().await;
}
