use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{Context, Result};
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::json;
use tokio::signal;
use tokio::sync::RwLock;
use tracing::{error, info};

use crate::codex::SessionManager;
use crate::config::AppConfig;
use crate::telegram::{
    TelegramUpdate, download_attachment, normalize_update, send_markdown_message,
};

#[derive(Clone)]
struct AppState {
    config: Arc<RwLock<AppConfig>>,
    sessions: Arc<SessionManager>,
}

pub async fn run(config: AppConfig) -> Result<()> {
    let listen: SocketAddr = config
        .server
        .listen
        .parse()
        .with_context(|| format!("invalid server.listen `{}`", config.server.listen))?;

    let sessions = Arc::new(SessionManager::new(config.clone()).await?);
    sessions.ensure_orchestrator().await?;

    let state = AppState {
        config: Arc::new(RwLock::new(config.clone())),
        sessions,
    };

    let app = Router::new()
        .route("/healthz", get(health))
        .route("/readyz", get(health))
        .route("/telegram/{secret}", post(telegram_webhook))
        .with_state(state.clone());

    #[cfg(unix)]
    spawn_reload_task(state.config.clone());

    let listener = tokio::net::TcpListener::bind(listen).await?;
    info!("listening on http://{}", listen);

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}

async fn health() -> Json<serde_json::Value> {
    Json(json!({ "ok": true }))
}

async fn telegram_webhook(
    State(state): State<AppState>,
    Path(secret): Path<String>,
    Json(update): Json<TelegramUpdate>,
) -> StatusCode {
    let config = state.config.read().await.clone();
    if secret != config.telegram.webhook_secret {
        return StatusCode::NOT_FOUND;
    }

    let Some(inbound) = normalize_update(&update, &config.telegram.allowed_user_ids) else {
        return StatusCode::OK;
    };

    tokio::spawn(async move {
        let attachment =
            match download_attachment(&config, &inbound, &config.attachment_root()).await {
                Ok(attachment) => attachment,
                Err(error) => {
                    error!(
                        chat_id = inbound.chat_id,
                        sender_id = inbound.sender_id,
                        update_id = update.update_id,
                        "failed to download Telegram attachment: {error}"
                    );
                    let _ = send_markdown_message(
                        &config,
                        inbound.chat_id,
                        inbound.thread_id,
                        &format!("Attachment download failed:\n\n```text\n{error}\n```"),
                    )
                    .await;
                    return;
                }
            };

        match state
            .sessions
            .run_chat_turn(inbound.clone(), attachment)
            .await
        {
            Ok(output) => {
                for message in output.markdown_messages {
                    if let Err(error) =
                        send_markdown_message(&config, inbound.chat_id, inbound.thread_id, &message)
                            .await
                    {
                        error!(
                            chat_id = inbound.chat_id,
                            message_id = inbound.message_id,
                            "failed to send Telegram response: {error}"
                        );
                        break;
                    }
                }
            }
            Err(error) => {
                error!(
                    chat_id = inbound.chat_id,
                    message_id = inbound.message_id,
                    "Codex turn failed: {error}"
                );
                let _ = send_markdown_message(
                    &config,
                    inbound.chat_id,
                    inbound.thread_id,
                    &format!("Turn failed:\n\n```text\n{error}\n```"),
                )
                .await;
            }
        }
    });

    StatusCode::OK
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = signal::ctrl_c().await;
    };

    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let terminate = async {
            if let Ok(mut signal) = signal(SignalKind::terminate()) {
                let _ = signal.recv().await;
            }
        };
        tokio::select! {
            _ = ctrl_c => {},
            _ = terminate => {},
        }
    }

    #[cfg(not(unix))]
    {
        ctrl_c.await;
    }
}

#[cfg(unix)]
fn spawn_reload_task(config: Arc<RwLock<AppConfig>>) {
    use tokio::signal::unix::{SignalKind, signal};

    tokio::spawn(async move {
        let Ok(mut hangup) = signal(SignalKind::hangup()) else {
            return;
        };
        loop {
            if hangup.recv().await.is_none() {
                break;
            }
            let config_path = { config.read().await.paths.config_path.clone() };
            match crate::config::AppConfig::load(Some(config_path)) {
                Ok(new_config) => {
                    *config.write().await = new_config;
                    info!("reloaded config after SIGHUP");
                }
                Err(error) => {
                    error!("failed to reload config after SIGHUP: {error}");
                }
            }
        }
    });
}
