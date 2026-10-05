use super::*;
use fut::protocol::remote::{self as wire, Capabilities, Capability, EndpointError, RemoteHello};

async fn remote_hello(connection: &mut Connection, hello: RemoteHello) -> ServerMessage {
    let request_id = Some(Uuid::new_v4());
    send_envelope(
        connection,
        Envelope {
            request_id,
            message: ClientMessage::RemoteHello(hello),
        },
    )
    .await;
    let response = receive_envelope(connection).await.unwrap();
    assert_eq!(response.request_id, request_id);
    response.message
}

#[tokio::test]
async fn remote_generation_one_supports_unequal_versions_and_optional_omission_over_bridge() {
    let mut harness = Harness::start("printf 'REMOTE_V1_READY\\r\\n'; while IFS= read -r line; do printf 'ANSWER:%s\\r\\n' \"$line\"; done").await;
    let (mut metadata, mut bridge) = spawn_bridge(&harness.socket);
    let mut offer = RemoteHello::new(ClientMode::Control, "0.1.0");
    offer.optional = vec!["unknown-optional.v1".into()];
    let ServerMessage::RemoteWelcome(welcome) = remote_hello(&mut metadata, offer.clone()).await
    else {
        panic!()
    };
    assert_ne!(offer.client_version, welcome.server_version);
    assert_eq!(offer.accept(&welcome).unwrap().names(), ["metadata.v1"]);
    assert!(welcome.extension_catalog.is_none());
    assert!(matches!(
        correlated_command(&mut metadata, ClientMessage::WatchResources).await,
        ServerMessage::Resources { .. }
    ));
    for request in [
        ClientMessage::OpenProject {
            project: "10er".into(),
        },
        ClientMessage::Ping,
        ClientMessage::GetExtensionCatalog,
        ClientMessage::Shutdown,
    ] {
        assert_eq!(
            correlated_command(&mut metadata, request).await,
            ServerMessage::EndpointError {
                error: EndpointError::MethodNotNegotiated
            }
        );
    }

    let (mut alerts, mut alerts_bridge) = spawn_bridge(&harness.socket);
    let alerts_offer = RemoteHello::new(ClientMode::Control, "0.2.0");
    let ServerMessage::RemoteWelcome(alerts_welcome) =
        remote_hello(&mut alerts, alerts_offer.clone()).await
    else {
        panic!()
    };
    assert!(
        alerts_offer
            .accept(&alerts_welcome)
            .unwrap()
            .contains(Capability::ControlAlerts)
    );
    assert!(matches!(
        correlated_command(
            &mut alerts,
            ClientMessage::WatchAlerts {
                client_id: fut::domain::ClientId::new(),
            },
        )
        .await,
        ServerMessage::AlertsChanged { .. }
    ));
    harness.detach(&mut alerts).await;
    drop(alerts);
    assert!(
        time::timeout(DEADLINE, alerts_bridge.wait())
            .await
            .unwrap()
            .unwrap()
            .success()
    );

    for optional in [false, true] {
        let (mut interactive, mut interactive_bridge) = spawn_bridge(&harness.socket);
        let mut offer = RemoteHello::new(interactive_mode(None), "99.123.456");
        if !optional {
            offer.optional.clear();
        }
        let ServerMessage::RemoteWelcome(welcome) =
            remote_hello(&mut interactive, offer.clone()).await
        else {
            panic!()
        };
        let capabilities = offer.accept(&welcome).unwrap();
        assert_eq!(capabilities.contains(Capability::Alerts), optional);
        assert_eq!(welcome.extension_catalog.is_some(), optional);
        let selected = welcome.selected.unwrap();
        harness.terminal_pid = Some(selected.focused.child_pid);
        let terminal = selected.focused.terminal_id;
        snapshot_containing(&mut interactive, terminal, "REMOTE_V1_READY").await;
        let response = correlated_command(
            &mut interactive,
            ClientMessage::WatchAlerts {
                client_id: fut::domain::ClientId::new(),
            },
        )
        .await;
        assert!(if optional {
            matches!(response, ServerMessage::AlertsChanged { .. })
        } else {
            response
                == ServerMessage::EndpointError {
                    error: EndpointError::MethodNotNegotiated,
                }
        });
        send(
            &mut interactive,
            ClientMessage::Input {
                bytes: b"hello\n".to_vec(),
            },
        )
        .await;
        snapshot_containing(&mut interactive, terminal, "ANSWER:hello").await;
        let size = TerminalSize {
            columns: 93,
            rows: 31,
        };
        send(
            &mut interactive,
            ClientMessage::Resize {
                terminal_id: terminal,
                size,
            },
        )
        .await;
        snapshot_with_size(&mut interactive, terminal, size).await;
        assert_eq!(
            correlated_command(&mut interactive, ClientMessage::Shutdown).await,
            ServerMessage::EndpointError {
                error: EndpointError::MethodNotNegotiated
            }
        );
        harness.detach(&mut interactive).await;
        drop(interactive);
        assert!(
            time::timeout(DEADLINE, interactive_bridge.wait())
                .await
                .unwrap()
                .unwrap()
                .success()
        );
    }
    assert!(matches!(
        harness.control_command(ClientMessage::Ping).await,
        ServerMessage::Pong { .. }
    ));
    assert!(matches!(
        correlated_command(&mut metadata, ClientMessage::ListResources).await,
        ServerMessage::Resources { .. }
    ));
    drop(metadata);
    assert!(
        time::timeout(DEADLINE, bridge.wait())
            .await
            .unwrap()
            .unwrap()
            .success()
    );
    harness.shutdown().await;
}

#[tokio::test]
async fn remote_malformed_and_incompatible_handshakes_fail_in_isolation() {
    let mut harness = Harness::start("printf 'ISOLATED_READY\\r\\n'; while IFS= read -r line; do printf 'LIVE:%s\\r\\n' \"$line\"; done").await;
    let (mut local, terminal, pid) = harness.interactive().await;
    snapshot_containing(&mut local, terminal, "ISOLATED_READY").await;
    let mut healthy = harness.connect().await.unwrap();
    assert!(matches!(
        remote_hello(&mut healthy, RemoteHello::new(ClientMode::Control, "0.1.0")).await,
        ServerMessage::RemoteWelcome(_)
    ));
    let daemon_pid = match correlated_command(&mut healthy, ClientMessage::Ping).await {
        ServerMessage::Pong { daemon_pid } => daemon_pid,
        message => panic!("{message:?}"),
    };
    for case in 0..10 {
        let mut connection = harness.connect().await.unwrap();
        let mut offer = RemoteHello::new(interactive_mode(None), "0.999.0");
        let expected = match case {
            0 => {
                offer.generation += 1;
                EndpointError::IncompatibleGeneration {
                    client: wire::GENERATION + 1,
                    server: wire::GENERATION,
                }
            }
            1 => {
                offer.codec = "unknown-codec".into();
                EndpointError::UnsupportedCodec
            }
            2 => {
                offer.required.push("unknown-required.v1".into());
                EndpointError::MissingRequiredCapability
            }
            3 => {
                offer.optional.push("metadata.v1".into());
                EndpointError::InvalidHandshake
            }
            4 => {
                offer.client_version = "0.1.0\x1b[2J".into();
                EndpointError::InvalidHandshake
            }
            5 => {
                offer.optional = (0..wire::MAX_CAPABILITIES + 1)
                    .map(|i| format!("optional-{i}"))
                    .collect();
                EndpointError::InvalidHandshake
            }
            _ => EndpointError::InvalidHandshake,
        };
        let mut payload = encode_payload(&Envelope {
            request_id: None,
            message: ClientMessage::RemoteHello(offer),
        })
        .unwrap();
        match case {
            6 => payload.push(0),
            7 => payload = vec![0xc1],
            8 => {
                payload = encode_payload(
                    &serde_json::json!({"message": {"type": "remote_hello", "generation": 1}}),
                )
                .unwrap()
            }
            _ => {}
        }
        if case == 9 {
            connection
                .get_mut()
                .write_all(&((fut::protocol::MAX_FRAME_LEN + 1) as u32).to_be_bytes())
                .await
                .unwrap();
        } else {
            connection.send(Bytes::from(payload)).await.unwrap();
        }
        assert_eq!(
            receive(&mut connection).await,
            Some(ServerMessage::EndpointError { error: expected }),
            "case {case}"
        );
        assert!(
            time::timeout(DEADLINE, connection.next())
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(
            correlated_command(&mut healthy, ClientMessage::Ping).await,
            ServerMessage::Pong { daemon_pid }
        );
        assert!(process_alive(pid));
    }
    // A failing remote never acquired/changed the active local attachment.
    send(
        &mut local,
        ClientMessage::Input {
            bytes: b"still-here\n".to_vec(),
        },
    )
    .await;
    snapshot_containing(&mut local, terminal, "LIVE:still-here").await;
    harness.detach(&mut local).await;
    drop(local);
    drop(healthy);
    assert!(!Capabilities::ALL.allows_client(&ClientMessage::Shutdown));
    harness.shutdown().await;
}
