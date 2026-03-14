use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::{SinkExt, StreamExt};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;

const DEFAULT_SOCKET_MODE_OPEN_URL: &str = "https://slack.com/api/apps.connections.open";
const DEFAULT_INITIAL_BACKOFF_MS: u64 = 1_000;
const DEFAULT_MAX_BACKOFF_MS: u64 = 30_000;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SlackTransportMode {
    #[default]
    Webhook,
    SocketMode,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SlackSocketModeSettings {
    #[serde(default)]
    pub transport: SlackTransportMode,
    #[serde(default = "default_app_token_secret_name")]
    pub app_token_secret_name: String,
    #[serde(default = "default_socket_mode_open_url")]
    pub socket_mode_open_url: String,
    #[serde(default = "default_initial_backoff_ms")]
    pub socket_mode_initial_backoff_ms: u64,
    #[serde(default = "default_max_backoff_ms")]
    pub socket_mode_max_backoff_ms: u64,
}

impl Default for SlackSocketModeSettings {
    fn default() -> Self {
        Self {
            transport: SlackTransportMode::Webhook,
            app_token_secret_name: default_app_token_secret_name(),
            socket_mode_open_url: default_socket_mode_open_url(),
            socket_mode_initial_backoff_ms: default_initial_backoff_ms(),
            socket_mode_max_backoff_ms: default_max_backoff_ms(),
        }
    }
}

impl SlackSocketModeSettings {
    pub fn from_config(config: &HashMap<String, Value>) -> Result<Self, serde_json::Error> {
        serde_json::from_value(serde_json::to_value(config)?)
    }

    pub fn is_enabled(&self) -> bool {
        self.transport == SlackTransportMode::SocketMode
    }

    pub fn bridge_config(&self) -> SlackSocketModeBridgeConfig {
        SlackSocketModeBridgeConfig {
            open_url: self.socket_mode_open_url.clone(),
            initial_backoff: Duration::from_millis(self.socket_mode_initial_backoff_ms.max(1)),
            max_backoff: Duration::from_millis(
                self.socket_mode_max_backoff_ms
                    .max(self.socket_mode_initial_backoff_ms.max(1)),
            ),
        }
    }
}

fn default_app_token_secret_name() -> String {
    "slack_app_token".to_string()
}

fn default_socket_mode_open_url() -> String {
    DEFAULT_SOCKET_MODE_OPEN_URL.to_string()
}

fn default_initial_backoff_ms() -> u64 {
    DEFAULT_INITIAL_BACKOFF_MS
}

fn default_max_backoff_ms() -> u64 {
    DEFAULT_MAX_BACKOFF_MS
}

#[derive(Debug, Clone)]
pub struct SlackSocketModeBridgeConfig {
    pub open_url: String,
    pub initial_backoff: Duration,
    pub max_backoff: Duration,
}

impl Default for SlackSocketModeBridgeConfig {
    fn default() -> Self {
        SlackSocketModeSettings::default().bridge_config()
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct SocketModeEnvelope {
    #[serde(rename = "type")]
    pub envelope_type: String,
    #[serde(default)]
    pub envelope_id: Option<String>,
    #[serde(default)]
    pub payload: Option<Value>,
    #[serde(default)]
    pub reason: Option<String>,
}

impl SocketModeEnvelope {
    pub fn from_message(message: &Message) -> Result<Option<Self>, String> {
        match message {
            Message::Text(text) => Self::from_json_bytes(text.as_bytes()).map(Some),
            Message::Binary(bytes) => Self::from_json_bytes(bytes).map(Some),
            Message::Ping(_) | Message::Pong(_) => Ok(None),
            Message::Close(_) => Ok(None),
            Message::Frame(_) => Ok(None),
        }
    }

    pub fn from_json_bytes(bytes: &[u8]) -> Result<Self, String> {
        serde_json::from_slice(bytes)
            .map_err(|e| format!("Failed to parse Socket Mode envelope: {e}"))
    }

    pub fn ack_message(&self) -> Option<Message> {
        self.envelope_id
            .as_ref()
            .map(|id| Message::Text(ack_payload(id).into()))
    }

    pub fn should_reconnect(&self) -> bool {
        self.envelope_type == "disconnect"
    }

    pub fn events_api_payload(&self) -> Option<&Value> {
        if self.envelope_type == "events_api" {
            self.payload.as_ref()
        } else {
            None
        }
    }
}

pub fn ack_payload(envelope_id: &str) -> String {
    serde_json::json!({ "envelope_id": envelope_id }).to_string()
}

fn next_backoff(current: Duration, max: Duration) -> Duration {
    let doubled = current.saturating_mul(2);
    if doubled > max { max } else { doubled }
}

#[async_trait]
pub trait SlackSocketModeForwarder: Send + Sync {
    async fn forward_events_api_payload(&self, payload: Value) -> Result<(), String>;
}

pub struct SlackSocketModeHandle {
    shutdown_tx: Option<oneshot::Sender<()>>,
    join_handle: JoinHandle<()>,
}

impl SlackSocketModeHandle {
    pub async fn shutdown(mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
        let _ = self.join_handle.await;
    }
}

pub fn start_slack_socket_mode_bridge(
    forwarder: Arc<dyn SlackSocketModeForwarder>,
    app_token: String,
    config: SlackSocketModeBridgeConfig,
) -> SlackSocketModeHandle {
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let join_handle = tokio::spawn(async move {
        let client = Client::new();
        run_socket_mode_loop(client, forwarder, app_token, config, shutdown_rx).await;
    });

    SlackSocketModeHandle {
        shutdown_tx: Some(shutdown_tx),
        join_handle,
    }
}

async fn run_socket_mode_loop(
    client: Client,
    forwarder: Arc<dyn SlackSocketModeForwarder>,
    app_token: String,
    config: SlackSocketModeBridgeConfig,
    shutdown_rx: oneshot::Receiver<()>,
) {
    let mut shutdown = std::pin::pin!(shutdown_rx);
    let mut backoff = config.initial_backoff.max(Duration::from_millis(1));

    loop {
        match open_socket_connection(&client, &app_token, &config.open_url).await {
            Ok(socket_url) => {
                tracing::info!("Slack Socket Mode connection opened");
                match connect_async(&socket_url).await {
                    Ok((stream, _)) => {
                        backoff = config.initial_backoff.max(Duration::from_millis(1));
                        let reconnect =
                            process_socket_messages(stream, Arc::clone(&forwarder), &mut shutdown)
                                .await;
                        if !reconnect {
                            break;
                        }
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "Failed to connect Slack Socket Mode websocket");
                    }
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "Failed to open Slack Socket Mode connection");
            }
        }

        tokio::select! {
            _ = &mut shutdown => break,
            _ = tokio::time::sleep(backoff) => {}
        }
        backoff = next_backoff(backoff, config.max_backoff.max(backoff));
    }
}

async fn process_socket_messages(
    mut stream: tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    forwarder: Arc<dyn SlackSocketModeForwarder>,
    shutdown: &mut std::pin::Pin<&mut oneshot::Receiver<()>>,
) -> bool {
    loop {
        tokio::select! {
            _ = shutdown.as_mut() => {
                let _ = stream.close(None).await;
                return false;
            }
            message = stream.next() => {
                let Some(message) = message else {
                    tracing::info!("Slack Socket Mode websocket closed");
                    return true;
                };
                match message {
                    Ok(Message::Ping(payload)) => {
                        if let Err(e) = stream.send(Message::Pong(payload)).await {
                            tracing::warn!(error = %e, "Failed to reply to Slack Socket Mode ping");
                            return true;
                        }
                    }
                    Ok(Message::Close(frame)) => {
                        tracing::info!(?frame, "Slack Socket Mode websocket closed");
                        return true;
                    }
                    Ok(message) => {
                        let envelope = match SocketModeEnvelope::from_message(&message) {
                            Ok(Some(envelope)) => envelope,
                            Ok(None) => continue,
                            Err(e) => {
                                tracing::warn!(error = %e, "Ignoring invalid Slack Socket Mode message");
                                continue;
                            }
                        };

                        if let Some(ack) = envelope.ack_message()
                            && let Err(e) = stream.send(ack).await
                        {
                            tracing::warn!(error = %e, "Failed to ACK Slack Socket Mode envelope");
                            return true;
                        }

                        if envelope.should_reconnect() {
                            tracing::info!(reason = ?envelope.reason, "Slack requested Socket Mode reconnect");
                            return true;
                        }

                        if let Some(payload) = envelope.events_api_payload() {
                            if let Err(e) = forwarder.forward_events_api_payload(payload.clone()).await {
                                tracing::warn!(error = %e, "Slack Socket Mode payload forwarding failed");
                            }
                        } else {
                            tracing::debug!(envelope_type = %envelope.envelope_type, "Ignoring unsupported Slack Socket Mode envelope");
                        }
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "Slack Socket Mode websocket error");
                        return true;
                    }
                }
            }
        }
    }
}

#[derive(Debug, Deserialize)]
struct OpenSocketResponse {
    ok: bool,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    error: Option<String>,
}

async fn open_socket_connection(
    client: &Client,
    app_token: &str,
    open_url: &str,
) -> Result<String, String> {
    let response = client
        .post(open_url)
        .bearer_auth(app_token)
        .send()
        .await
        .map_err(|e| format!("apps.connections.open request failed: {e}"))?;

    if !response.status().is_success() {
        return Err(format!(
            "apps.connections.open returned HTTP {}",
            response.status()
        ));
    }

    let body: OpenSocketResponse = response
        .json()
        .await
        .map_err(|e| format!("Invalid apps.connections.open response: {e}"))?;

    if !body.ok {
        return Err(format!(
            "apps.connections.open error: {}",
            body.error.unwrap_or_else(|| "unknown".to_string())
        ));
    }

    body.url
        .ok_or_else(|| "apps.connections.open response missing websocket url".to_string())
}

#[cfg(test)]
mod tests {
    use super::{
        SlackSocketModeSettings, SlackTransportMode, SocketModeEnvelope, ack_payload, next_backoff,
    };
    use std::collections::HashMap;
    use std::time::Duration;

    #[test]
    fn parses_socket_mode_config_from_channel_config() {
        let mut config = HashMap::new();
        config.insert(
            "transport".to_string(),
            serde_json::Value::String("socket_mode".to_string()),
        );
        config.insert(
            "app_token_secret_name".to_string(),
            serde_json::Value::String("custom_slack_app_token".to_string()),
        );

        let settings = match SlackSocketModeSettings::from_config(&config) {
            Ok(settings) => settings,
            Err(e) => panic!("expected valid config fixture: {e}"),
        };
        if settings.transport != SlackTransportMode::SocketMode {
            panic!("unexpected transport: {:?}", settings.transport);
        }
        if settings.app_token_secret_name != "custom_slack_app_token" {
            panic!(
                "unexpected app token secret name: {}",
                settings.app_token_secret_name
            );
        }
        if !settings.is_enabled() {
            panic!("socket mode should be enabled");
        }
    }

    #[test]
    fn parses_events_api_envelope() {
        let envelope = match SocketModeEnvelope::from_json_bytes(
            br#"{"type":"events_api","envelope_id":"env-1","payload":{"type":"event_callback"}}"#,
        ) {
            Ok(envelope) => envelope,
            Err(e) => panic!("expected valid envelope fixture: {e}"),
        };

        if envelope.envelope_type != "events_api" {
            panic!("unexpected envelope type: {}", envelope.envelope_type);
        }
        if envelope.envelope_id.as_deref() != Some("env-1") {
            panic!("unexpected envelope id: {:?}", envelope.envelope_id);
        }
        if envelope.events_api_payload().is_none() {
            panic!("expected events_api payload");
        }
        if envelope.should_reconnect() {
            panic!("events_api envelope should not reconnect");
        }
    }

    #[test]
    fn constructs_ack_payload() {
        let payload = ack_payload("env-123");
        if payload != r#"{"envelope_id":"env-123"}"# {
            panic!("unexpected ack payload: {payload}");
        }
    }

    #[test]
    fn disconnect_envelope_requests_reconnect() {
        let envelope = match SocketModeEnvelope::from_json_bytes(
            br#"{"type":"disconnect","reason":"refresh_requested"}"#,
        ) {
            Ok(envelope) => envelope,
            Err(e) => panic!("expected valid disconnect fixture: {e}"),
        };
        if !envelope.should_reconnect() {
            panic!("disconnect envelope should reconnect");
        }
    }

    #[test]
    fn backoff_caps_at_maximum() {
        let max = Duration::from_secs(8);
        if next_backoff(Duration::from_secs(1), max) != Duration::from_secs(2) {
            panic!("backoff should double below max");
        }
        if next_backoff(Duration::from_secs(8), max) != Duration::from_secs(8) {
            panic!("backoff should cap at max");
        }
    }
}
