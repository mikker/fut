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
if [ "$1" = "-F" ]; then
    [ -f "$2" ] || exit 99
    cat "$2" >> "$SSH_ARGS.configs"
    shift 2
fi
printf '%s\n' "$@" >> "$SSH_ARGS"
printf '%s\n' "$$" >> "$SSH_PIDS"
if [ -n "$SSH_STALL" ]; then exec /bin/sleep 60; fi
while [ -n "$SSH_RECONNECT_GATE" ] && [ -e "$SSH_RECONNECT_GATE" ]; do /bin/sleep 0.02; done
if [ -n "$SSH_FAILURE" ]; then
    printf '%s\n' "$SSH_FAILURE" >&2
    exit 255
fi
if [ -n "$REMOTE_HOST_ROOT" ]; then
    export HOME="$REMOTE_HOST_ROOT/home"
    export XDG_STATE_HOME="$REMOTE_HOST_ROOT/state"
    export XDG_DATA_HOME="$REMOTE_HOST_ROOT/data"
    export FUT_RUNTIME_DIR="$REMOTE_HOST_ROOT/runtime"
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
    *' --start-if-missing --restore-only')
        exec "$FUT_BIN" --no-config --socket "$socket" __stdio-bridge --start-if-missing --restore-only ;;
    *' --start-if-missing')
        exec "$FUT_BIN" --no-config --socket "$socket" __stdio-bridge --start-if-missing ;;
    *) exec "$FUT_BIN" --socket "$socket" __stdio-bridge ;;
esac
"#,
    )
    .unwrap();
    fs::set_permissions(&ssh, fs::Permissions::from_mode(0o755)).unwrap();
    bin
}

#[tokio::test]
async fn remote_layout_restore_saves_lists_and_restores_on_the_host_not_the_client() {
    use crate::layout_restore_tests::{action, approve, restart_empty, stop};
    let mut harness = Harness::start("exec /bin/sleep 600").await;
    let root = tempfile::tempdir().unwrap();
    let bin = fake_ssh(root.path());
    let socket = harness.socket.clone();
    let host_root = harness.root.path().to_owned();
    let run = |args: &[&str]| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_fut"));
        remote_env(&mut command, root.path(), &bin, &socket);
        command.env("REMOTE_HOST_ROOT", &host_root);
        command
            .args([
                "--no-config",
                "--json",
                "--machine",
                "layout-host",
                "layout",
            ])
            .args(args);
        tokio::process::Command::from(command)
    };
    let saved = run(&["save"]).output().await.unwrap();
    assert!(
        saved.status.success(),
        "{}",
        String::from_utf8_lossy(&saved.stderr)
    );
    let saved: Value = serde_json::from_slice(&saved.stdout).unwrap();
    assert!(
        PathBuf::from(saved["result"]["path"].as_str().unwrap())
            .starts_with(harness.root.path().join("state"))
    );
    let listed = run(&["list"]).output().await.unwrap();
    assert!(
        listed.status.success(),
        "{}",
        String::from_utf8_lossy(&listed.stderr)
    );
    let listed: Value = serde_json::from_slice(&listed.stdout).unwrap();
    assert_eq!(listed["result"][0]["panes"], 1);
    approve(&harness).await;
    action(&harness, fut::layout::LayoutAction::Enable).await;
    stop(&mut harness).await;
    restart_empty(&mut harness).await;
    let restored = run(&["restore", "--all", "--background"])
        .output()
        .await
        .unwrap();
    assert!(
        restored.status.success(),
        "{}",
        String::from_utf8_lossy(&restored.stderr)
    );
    let restored: Value = serde_json::from_slice(&restored.stdout).unwrap();
    assert_eq!(restored["result"]["restored"], serde_json::json!(["cwd"]));
    assert_eq!(harness.resources().await.sessions.len(), 1);
    assert!(!root.path().join("must-not-be-created").exists());
    assert!(
        fs::read_to_string(root.path().join("ssh-args"))
            .unwrap()
            .contains("--start-if-missing --restore-only")
    );
    assert_ssh_reaped(root.path()).await;
    stop(&mut harness).await;
    let attach_only = run(&["restore", "--all", "--background", "--attach-only"])
        .output()
        .await
        .unwrap();
    assert!(!attach_only.status.success());
    assert!(!harness.socket.exists());
    assert_ssh_reaped(root.path()).await;
    let restarted = run(&["restore", "--all", "--background"])
        .output()
        .await
        .unwrap();
    let restarted_resources = if restarted.status.success() {
        harness.resources().await.sessions
    } else {
        Vec::new()
    };
    let shutdown = harness.cli().args(["daemon", "shutdown"]).output().unwrap();
    assert!(
        shutdown.status.success(),
        "{}",
        String::from_utf8_lossy(&shutdown.stderr)
    );
    wait_for(DEADLINE, || !harness.socket.exists()).await;
    assert!(
        restarted.status.success(),
        "{}",
        String::from_utf8_lossy(&restarted.stderr)
    );
    let restarted: Value = serde_json::from_slice(&restarted.stdout).unwrap();
    assert_eq!(restarted["result"]["restored"], serde_json::json!(["cwd"]));
    assert_eq!(restarted_resources.len(), 1);
    assert_eq!(
        restarted_resources[0].workspaces[0].root,
        host_root.join("cwd")
    );
    assert_ssh_reaped(root.path()).await;
}

fn assert_ssh_arguments(root: &std::path::Path, host: &str, count: usize, start_if_missing: bool) {
    let argv = fs::read_to_string(root.join("ssh-args")).unwrap();
    let prefix = format!("-T\n--\n{host}\n");
    let commands = argv
        .strip_prefix(&prefix)
        .unwrap_or_else(|| panic!("unexpected SSH arguments: {argv:?}"))
        .split(&prefix)
        .collect::<Vec<_>>();
    assert_eq!(commands.len(), count, "{argv:?}");
    for command in commands {
        let command = command.trim_end_matches('\n');
        assert!(command.starts_with("sh -c '"), "{command:?}");
        assert!(command.contains("command -v fut"), "{command:?}");
        assert!(command.contains("$HOME/.local/bin/fut"), "{command:?}");
        assert!(command.contains("/usr/local/bin/fut"), "{command:?}");
        assert!(command.contains("__stdio-bridge \"$@\""), "{command:?}");
        if start_if_missing {
            assert!(command.ends_with("' sh --start-if-missing"), "{command:?}");
        } else {
            assert!(command.ends_with("' sh"), "{command:?}");
            assert!(!command.contains("--start-if-missing"), "{command:?}");
        }
    }
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
async fn remote_dropped_png_uploads_exact_bytes_without_enter_and_can_be_disabled() {
    let harness = Harness::start("printf 'DROP_READY\\r\\n'; while IFS= read -r line; do printf '%s\\n' \"$line\" >> pasted.txt; printf 'DROP_RECEIVED\\r\\n'; done").await;
    let mut png = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut png, 1, 1);
        encoder.set_color(png::ColorType::Rgba);
        encoder
            .write_header()
            .unwrap()
            .write_image_data(&[255, 0, 0, 255])
            .unwrap();
    }
    let received = harness.root.path().join("cwd/pasted.txt");
    for enabled in [true, false] {
        let root = tempfile::tempdir().unwrap();
        let bin = fake_ssh(root.path());
        let image = root.path().join("it's a screenshot.png");
        fs::write(&image, &png).unwrap();
        let pasted = format!("'{}'", image.display().to_string().replace('\'', "'\\''"));
        let mut command = Command::new("/usr/bin/script");
        remote_env(&mut command, root.path(), &bin, &harness.socket);
        if !enabled {
            let config = root.path().join("config");
            fs::write(&config, "[remote]\nupload_image_paths = false\n").unwrap();
            command.env("FUT_CONFIG", config);
        }
        command.args(script_command_args()).arg(format!(
            "stty cols 150 rows 40; exec \"$FUT_BIN\" {} --machine clonk --attach-only",
            if enabled { "--no-config" } else { "" }
        ));
        let mut client = PtyChild::spawn(command);
        client.wait_for("navigator").await;
        client.send(b"\r");
        client.wait_for("DROP_READY").await;
        client.send(format!("\x1b[200~{pasted}\x1b[201~").as_bytes());
        client
            .wait_for(if enabled {
                "fut-attachment-"
            } else {
                "screenshot.png"
            })
            .await;
        assert!(!received.exists(), "dropping a file must not submit Enter");
        client.send(b"\r");
        client.wait_for("DROP_RECEIVED").await;
        wait_for(DEADLINE, || {
            fs::read_to_string(&received).is_ok_and(|text| text.ends_with('\n'))
        })
        .await;
        let text = fs::read_to_string(&received).unwrap();
        if enabled {
            let path = text
                .trim()
                .strip_prefix('\'')
                .unwrap()
                .strip_suffix('\'')
                .unwrap();
            assert_ne!(path, image.to_str().unwrap());
            assert_eq!(fs::read(path).unwrap(), png);
        } else {
            assert_eq!(text.trim_end_matches('\n'), pasted);
        }
        fs::remove_file(&received).unwrap();
        client.send(b"\x02d");
        client.wait_success().await;
        assert_ssh_reaped(root.path()).await;
    }
    harness.shutdown().await;
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
        // Project opening must not resolve the remote workspace root in the client.
        client.send(b"\x02S");
        client.wait_for("project opener unavailable").await;
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
    assert_ssh_arguments(root.path(), "clonk", 4, true);
    assert!(matches!(
        harness.control_command(ClientMessage::Ping).await,
        ServerMessage::Pong { .. }
    ));
    harness.shutdown().await;
}

#[tokio::test]
async fn remote_configured_commands_use_remote_context_forms_input_and_activation() {
    let harness = Harness::start_with("printf 'COMMAND_HOST_READY\\r\\n'; while IFS= read -r line; do printf 'HOST:%s\\r\\n' \"$line\"; done", |root| {
        let extension = root.join("remote-commands");
        fs::create_dir_all(extension.join("opened")).unwrap();
        fs::write(extension.join("fut-extension.toml"), r#"
api_version = 1
version = '1.0.0'
fut = '>=0.8.0, <1.0.0'
id = 'remote-probe'
capabilities = ['commands']
[commands.interactive]
title = 'Remote interactive probe'
argv = ['./probe']
activate_opened = true
[[commands.interactive.fields]]
name = 'command'
label = 'Remote command'
default_config = 'command'
[[commands.interactive.fields]]
name = 'prompt'
label = 'Remote prompt'
[commands.background]
title = 'Remote background probe'
argv = ['./probe']
mode = 'background'
"#).unwrap();
        let probe = extension.join("probe");
        fs::write(&probe, format!(r#"#!/bin/sh
printf 'SOCKET=%s\nROOT=%s\nCWD=%s\nFORM=%s\n' "$FUT_SOCKET" "$FUT_EXTENSION_ROOT" "$PWD" "$FUT_EXTENSION_FORM" > "$FUT_EXTENSION_ROOT/$FUT_EXTENSION_COMMAND.context"
printf '%s\n' "$*" > "$FUT_EXTENSION_ROOT/$FUT_EXTENSION_COMMAND.args"
if [ "$FUT_EXTENSION_COMMAND" = background ]; then exit 0; fi
printf 'REMOTE_COMMAND_INPUT_READY\r\n'
IFS= read -r input
printf '%s\n' "$input" > "$FUT_EXTENSION_ROOT/input"
exec '{}' --socket "$FUT_SOCKET" open --background "$FUT_EXTENSION_ROOT/opened" --name activated -- /bin/sh -c "printf 'REMOTE_COMMAND_ACTIVATED\\r\\n'; while IFS= read -r line; do printf 'ACTIVATED:%s\\r\\n' \"\$line\"; done"
"#, env!("CARGO_BIN_EXE_fut"))).unwrap();
        fs::set_permissions(probe, fs::Permissions::from_mode(0o755)).unwrap();
        let config = root.join("home/.config/fut/config.toml");
        fs::create_dir_all(config.parent().unwrap()).unwrap();
        fs::write(config, format!("extensions = [{:?}]\n[extension.remote-probe]\ncommand = ['remote-default']\n", extension.to_str().unwrap())).unwrap();
        std::os::unix::fs::symlink("/bin/sh", root.join("home/trusted-probe")).unwrap();
    }).await;
    let root = tempfile::tempdir().unwrap();
    let bin = fake_ssh(root.path());
    let extension = harness
        .root
        .path()
        .join("remote-commands")
        .canonicalize()
        .unwrap();
    let cwd = harness.root.path().join("cwd").canonicalize().unwrap();
    let before = harness.resources().await;
    let config = root.path().join("config.toml");
    // No local extension installation: the binding and form must use the remote catalog.
    // A conflicting local default must not replace the remote extension's configuration.
    fs::write(&config, r#"
[ui.bindings]
'remote-probe:interactive' = 'g'
[extension.remote-probe]
command = ['wrong-local-default']
[extension_commands."remote-probe:background"]
args = ['--from-local-config']
[trusted_commands.local_probe]
title = 'Local trusted remote probe'
binding = 't'
program = '~/trusted-probe'
args = ['-c', 'printf "SOCKET=%s\nCWD=%s\n" "$FUT_SOCKET" "$PWD" > trusted.context; printf "TRUSTED_REMOTE_READY\r\n"; read -r input; printf "%s\n" "$input" > trusted.input']
"#).unwrap();
    let mut command = Command::new("/usr/bin/script");
    remote_env(&mut command, root.path(), &bin, &harness.socket);
    command
        .env("FUT_CONFIG", &config)
        .args(script_command_args())
        .arg("stty cols 100 rows 30; exec \"$FUT_BIN\" --machine clonk --attach-only");
    let mut client = PtyChild::spawn(command);
    client.wait_for("navigator").await;
    client.send(b"\r");
    client.wait_for("COMMAND_HOST_READY").await;

    client.send(b"\x02:remote background probe\r");
    wait_for(DEADLINE, || {
        fs::read_to_string(extension.join("background.context"))
            .is_ok_and(|text| text.contains("\nFORM=") && text.ends_with('\n'))
            && fs::read_to_string(extension.join("background.args"))
                .is_ok_and(|text| text == "--from-local-config\n")
    })
    .await;
    let background = fs::read_to_string(extension.join("background.context")).unwrap();
    assert_eq!(
        fs::read_to_string(extension.join("background.args")).unwrap(),
        "--from-local-config\n"
    );
    assert!(
        background.contains(&format!("SOCKET={}\n", harness.socket.display())),
        "{background:?}"
    );
    assert!(
        background.contains(&format!("ROOT={}\n", extension.display())),
        "{background:?}"
    );
    assert!(
        background.contains(&format!("CWD={}\n", cwd.display())),
        "{background:?}"
    );

    client.send(b"\x02t");
    client.wait_for("TRUSTED_REMOTE_READY").await;
    let trusted = fs::read_to_string(cwd.join("trusted.context")).unwrap();
    assert_eq!(
        trusted,
        format!(
            "SOCKET={}\nCWD={}\n",
            harness.socket.display(),
            cwd.display()
        )
    );
    client.send(b"trusted-input\r");
    wait_for(DEADLINE, || {
        fs::read_to_string(cwd.join("trusted.input")).is_ok_and(|text| text == "trusted-input\n")
    })
    .await;
    client.send_until(b"host-ready\n", "HOST:host-ready").await;
    assert_eq!(
        without_observations(harness.resources().await),
        without_observations(before)
    );

    client.send(b"\x02g");
    client.wait_for("Remote prompt").await;
    client.send(b"\tremote prompt value\r");
    client.wait_for("REMOTE_COMMAND_INPUT_READY").await;
    let context = fs::read_to_string(extension.join("interactive.context")).unwrap();
    assert_eq!(
        context,
        format!(
            "SOCKET={}\nROOT={}\nCWD={}\nFORM={{\"command\":\"remote-default\",\"prompt\":\"remote prompt value\"}}\n",
            harness.socket.display(),
            extension.display(),
            cwd.display(),
        )
    );
    client.send(b"remote-input\r");
    client.wait_for("REMOTE_COMMAND_ACTIVATED").await;
    assert_eq!(
        fs::read_to_string(extension.join("input")).unwrap(),
        "remote-input\n"
    );
    client.send(b"activation-ping\r");
    client.wait_for("ACTIVATED:activation-ping").await;
    assert!(!root.path().join("must-not-be-created").exists());
    assert!(!root.path().join("trusted.context").exists());
    assert!(!root.path().join("must-not-be-used.sock").exists());
    client.send(b"\x02d");
    client.wait_success().await;
    assert_ssh_reaped(root.path()).await;
    // Navigator, attachment, one connection per command, and remote form defaults.
    assert_eq!(
        fs::read_to_string(root.path().join("ssh-pids"))
            .unwrap()
            .lines()
            .count(),
        6
    );
    let arguments = fs::read_to_string(root.path().join("ssh-args")).unwrap();
    assert_eq!(
        arguments.matches("BatchMode=yes\n").count(),
        4,
        "{arguments:?}"
    );
    assert!(!arguments.contains("--start-if-missing"), "{arguments:?}");
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

    let pids = fs::read_to_string(root.path().join("ssh-pids")).unwrap();
    let active_pid = pids.lines().last().unwrap().parse::<i32>().unwrap();
    assert_eq!(unsafe { libc::kill(active_pid, libc::SIGTERM) }, 0);
    client.wait_for("Reconnecting to alpha").await;
    client.wait_for("machine reconnected").await;
    client.send(b"after-reconnect\r");
    client.wait_for("ALPHA:after-reconnect").await;

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
    // This test waits for the discovered process name to replace the initial
    // "sh" tab label. Launch Bash explicitly: /bin/sh is Dash on Linux.
    let harness = Harness::start("exec /bin/bash -c 'printf \"OPTIONAL_READY\\r\\n\"; while IFS= read -r line; do printf \"OPTIONAL:%s\\r\\n\" \"$line\"; done'").await;
    let root = tempfile::tempdir().unwrap();
    let bin = fake_ssh(root.path());
    let socket = root.path().join("compatible-peer.sock");
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    let daemon_socket = harness.socket.clone();
    // Simulate a generation-2 implementation from another package release
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
                        assert!(!matches!(message.message, ClientMessage::WatchAlerts { .. } | ClientMessage::GetExtensionCatalog | ClientMessage::Ping | ClientMessage::TerminalColors { .. }));
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
    client.wait_for("\x1b]11;?\x1b\\").await;
    client.send(b"\x1b]10;rgb:11/22/33\x07\x1b]11;rgb:ee/ee/ee\x07");
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
    assert_ssh_arguments(root.path(), "clonk", 2, false);
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
    assert!(
        error.contains("incompatible endpoint generation"),
        "{error}"
    );
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
        assert!(String::from_utf8_lossy(&output.stderr).contains("--machine"));
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
    assert_ssh_arguments(root.path(), "clonk", 1, false);
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
    assert!(
        error.contains("incompatible endpoint generation"),
        "{error}"
    );
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
#[tokio::test]
async fn remote_cli_opens_named_project_from_remote_catalog_and_attaches_directly() {
    let harness = Harness::start_with("while :; do sleep 1; done", |root| {
        let project = root.join("remote-project");
        fs::create_dir(&project).unwrap();
        let config_dir = root.join("home/.config/fut");
        fs::create_dir_all(&config_dir).unwrap();
        fs::write(
            config_dir.join("config.toml"),
            format!("[projects.10er]\npath = {:?}\n", project),
        )
        .unwrap();
    })
    .await;
    let root = tempfile::tempdir().unwrap();
    let bin = fake_ssh(root.path());
    let state = root.path().join(".local/state/fut");
    fs::create_dir_all(&state).unwrap();
    let catalog = state.join("machines.toml");
    fs::write(&catalog, format!(
        "version = 1\n[[machines]]\nid = {:?}\nlabel = \"work\"\ntarget = \"clonk\"\nenabled = false\n",
        Uuid::new_v4().to_string()
    )).unwrap();
    fs::set_permissions(&catalog, fs::Permissions::from_mode(0o600)).unwrap();
    let mut command = Command::new("/usr/bin/script");
    remote_env(&mut command, root.path(), &bin, &harness.socket);
    command
        .args(script_command_args())
        .arg(r#"stty cols 80 rows 24; exec "$FUT_BIN" p 10er --machine work --no-config"#);
    let mut client = PtyChild::spawn(command);
    client.wait_for("FUT_TEST_SHELL_READY>").await;
    client.send(b"printf 'REMOTE_PROJECT_%s\\n' ATTACHED\r");
    client.wait_for("REMOTE_PROJECT_ATTACHED").await;
    client.send(b"\x02d");
    client.wait_success().await;
    assert_ssh_reaped(root.path()).await;
    let resources = harness.resources().await;
    assert!(
        resources
            .sessions
            .iter()
            .any(|session| session.name == "remote-project")
    );
    harness.shutdown().await;
}

#[tokio::test]
async fn active_remote_connection_recovers_without_replaying_input_or_opening_project_again() {
    let harness = Harness::start_with("while :; do sleep 1; done", |root| {
        let project = root.join("remote-project");
        fs::create_dir(&project).unwrap();
        let config = root.join("home/.config/fut");
        fs::create_dir_all(&config).unwrap();
        fs::write(
            config.join("config.toml"),
            format!("[projects.10er]\npath = {:?}\n", project),
        )
        .unwrap();
    })
    .await;
    let root = tempfile::tempdir().unwrap();
    let bin = fake_ssh(root.path());
    let gate = root.path().join("pause-reconnect");
    let mut command = Command::new("/usr/bin/script");
    remote_env(&mut command, root.path(), &bin, &harness.socket);
    command.env("SSH_RECONNECT_GATE", &gate);
    command
        .args(script_command_args())
        .arg(r#"stty cols 80 rows 24; exec "$FUT_BIN" p 10er --machine clonk --no-config"#);
    let mut client = PtyChild::spawn(command);
    client.wait_for("FUT_TEST_SHELL_READY>").await;
    let before = without_observations(harness.resources().await);
    let pids = fs::read_to_string(root.path().join("ssh-pids")).unwrap();
    let pid = pids.lines().last().unwrap().parse::<i32>().unwrap();
    fs::write(&gate, "").unwrap();
    // Kill only the SSH bridge. The daemon and shell must survive.
    assert_eq!(unsafe { libc::kill(pid, libc::SIGTERM) }, 0);
    client.wait_for("Reconnecting to clonk").await;
    client.send(b"touch SHOULD_NOT_REPLAY\r");
    time::sleep(Duration::from_millis(100)).await;
    fs::remove_file(&gate).unwrap();
    client.wait_for("machine reconnected").await;
    client.send(b"printf 'RECOVERED_%s\\n' SHELL\r");
    client.wait_for("RECOVERED_SHELL").await;
    assert!(
        !harness
            .root
            .path()
            .join("remote-project/SHOULD_NOT_REPLAY")
            .exists()
    );
    assert_eq!(without_observations(harness.resources().await), before);
    client.send(b"\x02d");
    wait_for(DEADLINE, || client.text().contains("\x1b[?1049l")).await;
    client.wait_success().await;
    assert_ssh_reaped(root.path()).await;
    harness.shutdown().await;
}

#[tokio::test]
async fn remote_reconnection_can_be_cancelled_while_ssh_is_stalled() {
    let harness =
        Harness::start("printf 'REMOTE_CANCEL_READY\r\n'; while :; do sleep 1; done").await;
    let root = tempfile::tempdir().unwrap();
    let bin = fake_ssh(root.path());
    let gate = root.path().join("pause-reconnect");
    let mut command = Command::new("/usr/bin/script");
    remote_env(&mut command, root.path(), &bin, &harness.socket);
    command.env("SSH_RECONNECT_GATE", &gate);
    command
        .args(script_command_args())
        .arg(r#"stty cols 80 rows 24; exec "$FUT_BIN" --machine clonk --no-config"#);
    let mut client = PtyChild::spawn(command);
    client.wait_for("navigator").await;
    client.send(b"\r");
    client.wait_for("REMOTE_CANCEL_READY").await;
    fs::write(&gate, "").unwrap();
    let pids = fs::read_to_string(root.path().join("ssh-pids")).unwrap();
    let pid = pids.lines().last().unwrap().parse::<i32>().unwrap();
    assert_eq!(unsafe { libc::kill(pid, libc::SIGTERM) }, 0);
    client.wait_for("Reconnecting to clonk").await;
    time::sleep(Duration::from_millis(1200)).await;
    client.send(b"\x1b");
    client.wait_success().await;
    assert_ssh_reaped(root.path()).await;
    assert!(matches!(
        harness.control_command(ClientMessage::Ping).await,
        ServerMessage::Pong { .. }
    ));
    harness.shutdown().await;
}
