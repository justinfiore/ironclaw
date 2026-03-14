use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use axum::{Json, Router, routing::post};
use futures::{SinkExt, StreamExt};
use ironclaw::channels::wasm::{
    SlackSocketModeBridgeConfig, SlackSocketModeForwarder, start_slack_socket_mode_bridge,
};
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::sync::{Mutex, oneshot};
use tokio::time::timeout;
use tokio_tungstenite::accept_async;
use tokio_tungstenite::tungstenite::Message;

const TEST_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Default)]
struct RecordingForwarder {
    payloads: Mutex<Vec<Value>>,
}

#[async_trait]
impl SlackSocketModeForwarder for RecordingForwarder {
    async fn forward_events_api_payload(&self, payload: Value) -> Result<(), String> {
        self.payloads.lock().await.push(payload);
        Ok(())
    }
}

async fn start_json_server(body: Value) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = Router::new().route(
        "/api/apps.connections.open",
        post({
            let body = body.clone();
            move || {
                let body = body.clone();
                async move { Json(body) }
            }
        }),
    );

    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    addr
}

async fn start_socket_mode_ws_server(
    envelope: Value,
) -> (
    SocketAddr,
    oneshot::Receiver<String>,
    tokio::task::JoinHandle<()>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (ack_tx, ack_rx) = oneshot::channel();

    let handle = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut ws = accept_async(stream).await.unwrap();

        ws.send(Message::Text(r#"{"type":"hello"}"#.into()))
            .await
            .unwrap();
        ws.send(Message::Text(envelope.to_string().into()))
            .await
            .unwrap();

        let ack = ws.next().await.unwrap().unwrap();
        match ack {
            Message::Text(text) => {
                let _ = ack_tx.send(text.to_string());
            }
            other => panic!("expected text ACK, got {other:?}"),
        }
    });

    (addr, ack_rx, handle)
}

#[tokio::test]
async fn slack_socket_mode_bridge_acks_and_forwards_events_api_payloads() {
    let envelope = json!({
        "type": "events_api",
        "envelope_id": "env-123",
        "payload": {
            "type": "event_callback",
            "event": { "type": "message", "text": "hello" }
        }
    });
    let (ws_addr, ack_rx, ws_handle) = start_socket_mode_ws_server(envelope.clone()).await;
    let open_addr = start_json_server(json!({
        "ok": true,
        "url": format!("ws://{ws_addr}")
    }))
    .await;

    let forwarder = Arc::new(RecordingForwarder::default());
    let handle = start_slack_socket_mode_bridge(
        Arc::clone(&forwarder) as Arc<dyn SlackSocketModeForwarder>,
        "xapp-test".to_string(),
        SlackSocketModeBridgeConfig {
            open_url: format!("http://{open_addr}/api/apps.connections.open"),
            initial_backoff: Duration::from_millis(10),
            max_backoff: Duration::from_millis(20),
        },
    );

    let ack = timeout(TEST_TIMEOUT, ack_rx).await.unwrap().unwrap();
    assert_eq!(ack, r#"{"envelope_id":"env-123"}"#);

    let payloads = timeout(TEST_TIMEOUT, async {
        loop {
            let payloads = forwarder.payloads.lock().await.clone();
            if !payloads.is_empty() {
                break payloads;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("timed out waiting for forwarded payload");
    assert_eq!(payloads.len(), 1);
    assert_eq!(payloads[0], envelope["payload"]);

    handle.shutdown().await;
    ws_handle.await.unwrap();
}

#[tokio::test]
async fn slack_socket_mode_bridge_ignores_unsupported_envelopes() {
    let envelope = json!({
        "type": "slash_commands",
        "envelope_id": "env-456",
        "payload": { "command": "/ironclaw" }
    });
    let (ws_addr, ack_rx, ws_handle) = start_socket_mode_ws_server(envelope).await;
    let open_addr = start_json_server(json!({
        "ok": true,
        "url": format!("ws://{ws_addr}")
    }))
    .await;

    let forwarder = Arc::new(RecordingForwarder::default());
    let handle = start_slack_socket_mode_bridge(
        Arc::clone(&forwarder) as Arc<dyn SlackSocketModeForwarder>,
        "xapp-test".to_string(),
        SlackSocketModeBridgeConfig {
            open_url: format!("http://{open_addr}/api/apps.connections.open"),
            initial_backoff: Duration::from_millis(10),
            max_backoff: Duration::from_millis(20),
        },
    );

    let ack = timeout(TEST_TIMEOUT, ack_rx).await.unwrap().unwrap();
    assert_eq!(ack, r#"{"envelope_id":"env-456"}"#);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(forwarder.payloads.lock().await.is_empty());

    handle.shutdown().await;
    ws_handle.await.unwrap();
}
