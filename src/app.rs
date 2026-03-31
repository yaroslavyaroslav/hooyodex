use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{Context, Result};
use axum::extract::{Path as AxumPath, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router, response::IntoResponse};
use serde_json::json;
use tokio::signal;
use tokio::sync::RwLock;
use tracing::{error, info};

use crate::approval::ApprovalManager;
use crate::channels::whatsapp::{
    WhatsAppWebhookPayload, WhatsAppWebhookVerificationQuery, normalize_webhook_event,
    verify_webhook,
};
use crate::channels::{ReplyTarget, TurnOutput, send_turn_output_to_target};
use crate::codex::{ChatRequest, ChatTurnRunner, OutputSink, SessionManager};
use crate::config::AppConfig;
use crate::telegram::{
    DownloadedAttachment, TelegramUpdate, download_attachment, normalize_update,
};
use crate::transcription::transcribe_voice_message;

#[derive(Clone)]
struct AppState {
    config: Arc<RwLock<AppConfig>>,
    sessions: Arc<dyn ChatTurnRunner>,
    approvals: Arc<ApprovalManager>,
}

pub async fn run(config: AppConfig) -> Result<()> {
    let listen: SocketAddr = config
        .server
        .listen
        .parse()
        .with_context(|| format!("invalid server.listen `{}`", config.server.listen))?;

    let approvals = ApprovalManager::new();
    let sessions = Arc::new(SessionManager::new(config.clone(), approvals.clone()).await?);
    sessions.ensure_orchestrator().await?;

    let state = AppState {
        config: Arc::new(RwLock::new(config.clone())),
        sessions,
        approvals,
    };

    let mut app = Router::new()
        .route("/healthz", get(health))
        .route("/readyz", get(ready))
        .route("/telegram/{secret}", post(telegram_webhook));
    if let Some(whatsapp) = config.whatsapp.as_ref() {
        let path: &'static str = Box::leak(whatsapp.webhook_path.clone().into_boxed_str());
        app = app
            .route(path, get(whatsapp_webhook_verify))
            .route(path, post(whatsapp_webhook));
    }
    let app = app.with_state(state.clone());

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

async fn ready(State(state): State<AppState>) -> impl IntoResponse {
    match state.sessions.ready().await {
        Ok(()) => (StatusCode::OK, Json(json!({ "ok": true }))).into_response(),
        Err(error) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({
                "ok": false,
                "error": error.to_string(),
            })),
        )
            .into_response(),
    }
}

async fn whatsapp_webhook_verify(
    State(state): State<AppState>,
    Query(query): Query<WhatsAppWebhookVerificationQuery>,
) -> impl IntoResponse {
    let config = state.config.read().await.clone();
    let Some(whatsapp) = config.whatsapp.as_ref() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    match verify_webhook(whatsapp, &query) {
        Some(challenge) => challenge.into_response(),
        None => StatusCode::FORBIDDEN.into_response(),
    }
}

async fn whatsapp_webhook(
    State(state): State<AppState>,
    Json(payload): Json<WhatsAppWebhookPayload>,
) -> StatusCode {
    let config = state.config.read().await.clone();
    let Some(whatsapp) = config.whatsapp.as_ref() else {
        return StatusCode::NOT_FOUND;
    };
    let Some(event) = normalize_webhook_event(whatsapp, &payload) else {
        return StatusCode::OK;
    };

    let sessions = state.sessions.clone();
    tokio::spawn(async move {
        if let Err(error) = process_whatsapp_update(sessions.as_ref(), &config, event).await {
            error!("failed to process WhatsApp update: {error}");
        }
    });

    StatusCode::OK
}

async fn telegram_webhook(
    State(state): State<AppState>,
    AxumPath(secret): AxumPath<String>,
    Json(update): Json<TelegramUpdate>,
) -> StatusCode {
    let config = state.config.read().await.clone();
    if secret != config.telegram.webhook_secret {
        return StatusCode::NOT_FOUND;
    }

    match state
        .approvals
        .handle_update(&config, &update, &config.telegram.allowed_user_ids)
        .await
    {
        Ok(true) => return StatusCode::OK,
        Ok(false) => {}
        Err(error) => {
            error!("failed to process Telegram approval update: {error}");
            return StatusCode::OK;
        }
    }

    let Some(inbound) = normalize_update(&update, &config.telegram.allowed_user_ids) else {
        return StatusCode::OK;
    };

    let sessions = state.sessions.clone();
    tokio::spawn(async move {
        if let Err(error) =
            process_telegram_update(sessions.as_ref(), &config, update, inbound).await
        {
            error!("failed to process Telegram update: {error}");
        }
    });

    StatusCode::OK
}

async fn process_telegram_update(
    runner: &dyn ChatTurnRunner,
    config: &AppConfig,
    update: TelegramUpdate,
    inbound: crate::telegram::InboundMessage,
) -> Result<()> {
    if inbound.requests_new_chat() {
        let request = ChatRequest::from_telegram(
            inbound.clone(),
            operator_target_for_telegram_inbound(config, &inbound),
        );
        runner.reset_chat(&request).await?;
        send_turn_output_to_target(
            config,
            &ReplyTarget::telegram(inbound.chat_id, inbound.thread_id),
            TurnOutput::Markdown {
                text: "Started a fresh chat. History cleared for this Telegram chat.".to_string(),
                disable_notification: false,
            },
        )
        .await?;
        return Ok(());
    }

    let attachment = match download_attachment(config, &inbound, &config.attachment_root()).await {
        Ok(attachment) => attachment,
        Err(error) => {
            error!(
                chat_id = inbound.chat_id,
                sender_id = inbound.sender_id,
                update_id = update.update_id,
                "failed to download Telegram attachment: {error}"
            );
            let _ = send_turn_output_to_target(
                config,
                &ReplyTarget::telegram(inbound.chat_id, inbound.thread_id),
                TurnOutput::Markdown {
                    text: format!("Attachment download failed:\n\n```text\n{error}\n```"),
                    disable_notification: false,
                },
            )
            .await;
            return Ok(());
        }
    };
    let attachment = match maybe_transcribe_voice_attachment(config, attachment).await {
        Ok(attachment) => attachment,
        Err(error) => {
            error!(
                chat_id = inbound.chat_id,
                sender_id = inbound.sender_id,
                update_id = update.update_id,
                "failed to transcribe Telegram voice message: {error}"
            );
            let _ = send_turn_output_to_target(
                config,
                &ReplyTarget::telegram(inbound.chat_id, inbound.thread_id),
                TurnOutput::Markdown {
                    text: format!("Voice transcription failed:\n\n```text\n{error}\n```"),
                    disable_notification: false,
                },
            )
            .await;
            return Ok(());
        }
    };

    let chat_id = inbound.chat_id;
    let thread_id = inbound.thread_id;
    let message_id = inbound.message_id;
    let request = ChatRequest::from_telegram(
        inbound.clone(),
        operator_target_for_telegram_inbound(config, &inbound),
    );
    let mut sink = RoutedSink::new(
        config.clone(),
        ReplyTarget::telegram(inbound.chat_id, inbound.thread_id),
    );

    match runner.run_chat_turn(request, attachment, &mut sink).await {
        Ok(()) => Ok(()),
        Err(error) => {
            error!(chat_id, message_id, "Codex turn failed: {error}");
            let _ = send_turn_output_to_target(
                config,
                &ReplyTarget::telegram(chat_id, thread_id),
                TurnOutput::Markdown {
                    text: format!("Turn failed:\n\n```text\n{error}\n```"),
                    disable_notification: false,
                },
            )
            .await;
            Ok(())
        }
    }
}

fn operator_target_for_telegram_inbound(
    config: &AppConfig,
    inbound: &crate::telegram::InboundMessage,
) -> crate::approval::OperatorTarget {
    if let Some(target) = config.operator.telegram.as_ref() {
        return crate::approval::OperatorTarget {
            chat_id: target.chat_id,
            thread_id: target.thread_id,
        };
    }
    crate::approval::OperatorTarget {
        chat_id: inbound.chat_id,
        thread_id: inbound.thread_id,
    }
}

fn operator_target_for_whatsapp(config: &AppConfig) -> crate::approval::OperatorTarget {
    if let Some(target) = config.operator.telegram.as_ref() {
        return crate::approval::OperatorTarget {
            chat_id: target.chat_id,
            thread_id: target.thread_id,
        };
    }
    crate::approval::OperatorTarget {
        chat_id: 0,
        thread_id: None,
    }
}

async fn process_whatsapp_update(
    runner: &dyn ChatTurnRunner,
    config: &AppConfig,
    event: crate::channels::whatsapp::WhatsAppInboundEvent,
) -> Result<()> {
    let session_key = event.session_key();
    let reply_target = ReplyTarget::WhatsApp(event.reply_target());
    let request = ChatRequest {
        session_key,
        sender_name: event.sender_name.clone(),
        text: Some(event.text.clone()),
        operator_target: operator_target_for_whatsapp(config),
    };
    let mut sink = RoutedSink::new(config.clone(), reply_target);
    runner.run_chat_turn(request, None, &mut sink).await
}

async fn maybe_transcribe_voice_attachment(
    config: &AppConfig,
    attachment: Option<DownloadedAttachment>,
) -> Result<Option<DownloadedAttachment>> {
    let attachment = match attachment {
        Some(DownloadedAttachment::Voice {
            path,
            duration_seconds,
            transcript,
        }) => DownloadedAttachment::Voice {
            path,
            duration_seconds,
            transcript,
        },
        other => return Ok(other),
    };

    let DownloadedAttachment::Voice {
        path,
        duration_seconds,
        transcript,
    } = attachment
    else {
        unreachable!("voice attachment just matched");
    };

    if !config.voice.enabled {
        return Ok(Some(DownloadedAttachment::Voice {
            path,
            duration_seconds,
            transcript,
        }));
    }

    let transcript = match transcript {
        Some(transcript) => Some(transcript),
        None => Some(transcribe_voice_message(config, &path).await?),
    };

    Ok(Some(DownloadedAttachment::Voice {
        path,
        duration_seconds,
        transcript,
    }))
}

struct RoutedSink {
    config: AppConfig,
    target: ReplyTarget,
}

impl RoutedSink {
    fn new(config: AppConfig, target: ReplyTarget) -> Self {
        Self { config, target }
    }
}

#[async_trait::async_trait]
impl OutputSink for RoutedSink {
    async fn send(&mut self, output: TurnOutput) -> Result<()> {
        send_turn_output_to_target(&self.config, &self.target, output).await
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    use std::path::PathBuf;
    use std::sync::Arc;

    use anyhow::Result;
    use axum::body::Bytes;
    use axum::extract::State;
    use axum::routing::{get, post};
    use serde_json::Value;
    use tempfile::TempDir;
    use tokio::net::TcpListener;
    use tokio::sync::Mutex;

    use crate::approval::{ApprovalManager, ApprovalOutcome, OperatorTarget};
    use crate::channels::TurnOutput;
    use crate::config::{
        AppConfig, AppPaths, CodexConfig, OperatorConfig, OperatorTelegramConfig, ServerConfig,
        TelegramConfig, VoiceConfig,
    };
    use crate::telegram::{
        DownloadedAttachment, TelegramCallbackQuery, TelegramChat, TelegramMessage, TelegramUpdate,
        TelegramUser, TelegramVoice, normalize_update,
    };

    #[derive(Clone, Debug)]
    struct RecordedRequest {
        path: String,
        body: Vec<u8>,
    }

    type ResetCalls = Arc<Mutex<Vec<(i64, Option<i64>)>>>;

    #[derive(Clone, Default)]
    struct MockTelegramState {
        requests: Arc<Mutex<Vec<RecordedRequest>>>,
    }

    struct FakeTurnRunner {
        scripted_outputs: Vec<TurnOutput>,
    }

    #[async_trait::async_trait]
    impl ChatTurnRunner for FakeTurnRunner {
        async fn reset_chat(&self, _request: &ChatRequest) -> Result<()> {
            Ok(())
        }

        async fn run_chat_turn(
            &self,
            _request: ChatRequest,
            _attachment: Option<DownloadedAttachment>,
            sink: &mut dyn OutputSink,
        ) -> Result<()> {
            for output in self.scripted_outputs.clone() {
                sink.send(output).await?;
            }
            Ok(())
        }
    }

    #[derive(Default)]
    struct CapturingRunner {
        requests: Arc<Mutex<Vec<ChatRequest>>>,
        attachments: Arc<Mutex<Vec<Option<DownloadedAttachment>>>>,
    }

    #[async_trait::async_trait]
    impl ChatTurnRunner for CapturingRunner {
        async fn reset_chat(&self, _request: &ChatRequest) -> Result<()> {
            Ok(())
        }

        async fn run_chat_turn(
            &self,
            request: ChatRequest,
            attachment: Option<DownloadedAttachment>,
            _sink: &mut dyn OutputSink,
        ) -> Result<()> {
            self.requests.lock().await.push(request);
            self.attachments.lock().await.push(attachment);
            Ok(())
        }
    }

    #[tokio::test]
    async fn process_update_sends_each_turn_output_to_telegram() -> Result<()> {
        let mock = start_mock_telegram_api().await?;
        let tempdir = TempDir::new()?;
        let config = test_config(tempdir.path(), &mock.base_url);
        let update = sample_update();
        let inbound = normalize_update(&update, &[42]).expect("inbound");
        let runner = FakeTurnRunner {
            scripted_outputs: vec![
                TurnOutput::Markdown {
                    text: "First intermediate".to_string(),
                    disable_notification: true,
                },
                TurnOutput::Markdown {
                    text: "Second intermediate".to_string(),
                    disable_notification: true,
                },
                TurnOutput::Markdown {
                    text: "Final answer".to_string(),
                    disable_notification: false,
                },
            ],
        };

        process_telegram_update(&runner, &config, update, inbound).await?;

        let requests = mock.requests().await;
        let messages: Vec<Value> = requests
            .iter()
            .filter(|request| request.path.ends_with("/sendMessage"))
            .map(|request| serde_json::from_slice(&request.body).expect("sendMessage json"))
            .collect();

        assert_eq!(messages.len(), 3);
        assert!(messages.iter().any(|body| {
            body["text"]
                .as_str()
                .unwrap_or_default()
                .contains("First intermediate")
        }));
        assert!(messages.iter().any(|body| {
            body["text"]
                .as_str()
                .unwrap_or_default()
                .contains("Second intermediate")
        }));
        assert!(messages.iter().any(|body| {
            body["text"]
                .as_str()
                .unwrap_or_default()
                .contains("Final answer")
        }));
        assert_eq!(
            messages
                .iter()
                .map(|body| body["disable_notification"].as_bool())
                .collect::<Vec<_>>(),
            vec![Some(true), Some(true), Some(false)]
        );
        Ok(())
    }

    #[tokio::test]
    async fn approval_callbacks_resolve_pending_request_as_approved() -> Result<()> {
        let mock = start_mock_telegram_api().await?;
        let tempdir = TempDir::new()?;
        let config = test_config(tempdir.path(), &mock.base_url);
        let approvals = ApprovalManager::new();
        let target = OperatorTarget {
            chat_id: 100,
            thread_id: Some(777),
        };

        let pending = {
            let approvals = approvals.clone();
            let config = config.clone();
            tokio::spawn(async move {
                approvals
                    .request(&config, target, "Need approval", "Draft reply")
                    .await
            })
        };

        let callback_data = loop {
            let requests = mock.requests().await;
            if let Some(send_message) = requests
                .iter()
                .find(|req| req.path.ends_with("/sendMessage"))
            {
                let body: Value =
                    serde_json::from_slice(&send_message.body).expect("sendMessage json");
                let data = body["reply_markup"]["inline_keyboard"][0][0]["callback_data"]
                    .as_str()
                    .expect("approve callback data");
                break data.to_string();
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        };

        let handled = approvals
            .handle_update(
                &config,
                &TelegramUpdate {
                    update_id: 999,
                    message: None,
                    edited_message: None,
                    callback_query: Some(TelegramCallbackQuery {
                        id: "cb-1".to_string(),
                        from: TelegramUser {
                            id: 42,
                            first_name: Some("Test".to_string()),
                            last_name: None,
                        },
                        message: Some(TelegramMessage {
                            message_id: 500,
                            chat: TelegramChat { id: 100 },
                            from: Some(TelegramUser {
                                id: 42,
                                first_name: Some("Test".to_string()),
                                last_name: None,
                            }),
                            sender_chat: None,
                            text: Some("approval".to_string()),
                            caption: None,
                            photo: None,
                            document: None,
                            voice: None,
                            message_thread_id: Some(777),
                        }),
                        data: Some(callback_data),
                    }),
                },
                &[42],
            )
            .await?;

        assert!(handled);
        assert_eq!(pending.await??, ApprovalOutcome::Approved);

        let requests = mock.requests().await;
        assert!(
            requests
                .iter()
                .any(|request| request.path.ends_with("/answerCallbackQuery"))
        );
        assert!(
            requests
                .iter()
                .any(|request| request.path.ends_with("/editMessageReplyMarkup"))
        );
        Ok(())
    }

    #[tokio::test]
    async fn approval_steer_consumes_next_operator_message() -> Result<()> {
        let mock = start_mock_telegram_api().await?;
        let tempdir = TempDir::new()?;
        let config = test_config(tempdir.path(), &mock.base_url);
        let approvals = ApprovalManager::new();
        let target = OperatorTarget {
            chat_id: 100,
            thread_id: Some(777),
        };

        let pending = {
            let approvals = approvals.clone();
            let config = config.clone();
            tokio::spawn(async move {
                approvals
                    .request(&config, target, "Need approval", "Draft reply")
                    .await
            })
        };

        let steer_callback_data = loop {
            let requests = mock.requests().await;
            if let Some(send_message) = requests
                .iter()
                .find(|req| req.path.ends_with("/sendMessage"))
            {
                let body: Value =
                    serde_json::from_slice(&send_message.body).expect("sendMessage json");
                let data = body["reply_markup"]["inline_keyboard"][0][2]["callback_data"]
                    .as_str()
                    .expect("steer callback data");
                break data.to_string();
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        };

        assert!(
            approvals
                .handle_update(
                    &config,
                    &TelegramUpdate {
                        update_id: 1000,
                        message: None,
                        edited_message: None,
                        callback_query: Some(TelegramCallbackQuery {
                            id: "cb-2".to_string(),
                            from: TelegramUser {
                                id: 42,
                                first_name: Some("Test".to_string()),
                                last_name: None,
                            },
                            message: Some(TelegramMessage {
                                message_id: 500,
                                chat: TelegramChat { id: 100 },
                                from: Some(TelegramUser {
                                    id: 42,
                                    first_name: Some("Test".to_string()),
                                    last_name: None,
                                }),
                                sender_chat: None,
                                text: Some("approval".to_string()),
                                caption: None,
                                photo: None,
                                document: None,
                                voice: None,
                                message_thread_id: Some(777),
                            }),
                            data: Some(steer_callback_data),
                        }),
                    },
                    &[42],
                )
                .await?
        );

        assert!(
            approvals
                .handle_update(
                    &config,
                    &TelegramUpdate {
                        update_id: 1001,
                        message: Some(TelegramMessage {
                            message_id: 33,
                            chat: TelegramChat { id: 100 },
                            from: Some(TelegramUser {
                                id: 42,
                                first_name: Some("Test".to_string()),
                                last_name: None,
                            }),
                            sender_chat: None,
                            text: Some("Make it shorter".to_string()),
                            caption: None,
                            photo: None,
                            document: None,
                            voice: None,
                            message_thread_id: Some(777),
                        }),
                        edited_message: None,
                        callback_query: None,
                    },
                    &[42],
                )
                .await?
        );

        assert_eq!(
            pending.await??,
            ApprovalOutcome::Steer("Make it shorter".to_string())
        );
        Ok(())
    }

    #[tokio::test]
    async fn process_update_routes_media_outputs_through_telegram_tooling() -> Result<()> {
        let mock = start_mock_telegram_api().await?;
        let tempdir = TempDir::new()?;
        let config = test_config(tempdir.path(), &mock.base_url);
        let update = sample_group_thread_update_with_text("send file", 777);
        let inbound = normalize_update(&update, &[42]).expect("inbound");
        let file_path = tempdir.path().join("report.txt");
        std::fs::write(&file_path, "report-body")?;
        let runner = FakeTurnRunner {
            scripted_outputs: vec![TurnOutput::Media {
                kind: crate::channels::MediaKind::Document,
                path: file_path,
                caption_markdown: Some("Attached report".to_string()),
                file_name: Some("final-report.txt".to_string()),
                mime_type: Some("text/plain".to_string()),
                disable_notification: true,
            }],
        };

        process_telegram_update(&runner, &config, update, inbound).await?;

        let requests = mock.requests().await;
        let upload = requests
            .iter()
            .find(|request| request.path.ends_with("/sendDocument"))
            .expect("sendDocument request");
        let body = String::from_utf8_lossy(&upload.body);

        assert!(body.contains("name=\"document\""));
        assert!(body.contains("filename=\"final-report.txt\""));
        assert!(body.contains("Attached report"));
        assert!(body.contains("message_thread_id"));
        assert!(body.contains("777"));
        assert!(body.contains("disable_notification"));
        assert!(body.contains("true"));
        Ok(())
    }

    #[tokio::test]
    async fn process_update_replies_inside_same_telegram_thread() -> Result<()> {
        let mock = start_mock_telegram_api().await?;
        let tempdir = TempDir::new()?;
        let config = test_config(tempdir.path(), &mock.base_url);
        let update = sample_group_thread_update_with_text("hello", 777);
        let inbound = normalize_update(&update, &[42]).expect("inbound");
        let runner = FakeTurnRunner {
            scripted_outputs: vec![TurnOutput::Markdown {
                text: "Threaded reply".to_string(),
                disable_notification: true,
            }],
        };

        process_telegram_update(&runner, &config, update, inbound).await?;

        let requests = mock.requests().await;
        let send_message: Value = requests
            .iter()
            .find(|request| request.path.ends_with("/sendMessage"))
            .map(|request| serde_json::from_slice(&request.body).expect("sendMessage json"))
            .expect("sendMessage request");

        assert_eq!(send_message["message_thread_id"].as_i64(), Some(777));
        assert_eq!(send_message["text"].as_str(), Some("Threaded reply"));
        assert_eq!(send_message["disable_notification"].as_bool(), Some(true));
        Ok(())
    }

    #[tokio::test]
    async fn process_update_resets_chat_on_new_command() -> Result<()> {
        #[derive(Default)]
        struct ResettableRunner {
            resets: ResetCalls,
            turns: Arc<Mutex<u32>>,
        }

        #[async_trait::async_trait]
        impl ChatTurnRunner for ResettableRunner {
            async fn reset_chat(&self, request: &ChatRequest) -> Result<()> {
                self.resets
                    .lock()
                    .await
                    .push(telegram_target_from_session_key(&request.session_key));
                Ok(())
            }

            async fn run_chat_turn(
                &self,
                _request: ChatRequest,
                _attachment: Option<DownloadedAttachment>,
                _sink: &mut dyn OutputSink,
            ) -> Result<()> {
                *self.turns.lock().await += 1;
                Ok(())
            }
        }

        let mock = start_mock_telegram_api().await?;
        let tempdir = TempDir::new()?;
        let config = test_config(tempdir.path(), &mock.base_url);
        let update = sample_group_thread_update_with_text("новый тред", 777);
        let inbound = normalize_update(&update, &[42]).expect("inbound");
        let runner = ResettableRunner::default();

        process_telegram_update(&runner, &config, update, inbound).await?;

        assert_eq!(runner.resets.lock().await.as_slice(), &[(100, Some(777))]);
        assert_eq!(*runner.turns.lock().await, 0);

        let requests = mock.requests().await;
        let responses: Vec<Value> = requests
            .iter()
            .filter(|request| request.path.ends_with("/sendMessage"))
            .map(|request| serde_json::from_slice(&request.body).expect("sendMessage json"))
            .collect();
        assert_eq!(responses.len(), 1);
        assert_eq!(responses[0]["message_thread_id"].as_i64(), Some(777));
        assert!(
            responses[0]["text"]
                .as_str()
                .unwrap_or_default()
                .contains("fresh chat")
        );
        Ok(())
    }

    #[tokio::test]
    async fn process_update_resets_only_current_telegram_thread() -> Result<()> {
        #[derive(Default)]
        struct ResettableRunner {
            resets: ResetCalls,
        }

        #[async_trait::async_trait]
        impl ChatTurnRunner for ResettableRunner {
            async fn reset_chat(&self, request: &ChatRequest) -> Result<()> {
                self.resets
                    .lock()
                    .await
                    .push(telegram_target_from_session_key(&request.session_key));
                Ok(())
            }

            async fn run_chat_turn(
                &self,
                _request: ChatRequest,
                _attachment: Option<DownloadedAttachment>,
                _sink: &mut dyn OutputSink,
            ) -> Result<()> {
                Ok(())
            }
        }

        let mock = start_mock_telegram_api().await?;
        let tempdir = TempDir::new()?;
        let config = test_config(tempdir.path(), &mock.base_url);
        let mut update = sample_update_with_text("новый тред");
        update.message.as_mut().expect("message").message_thread_id = Some(321);
        let inbound = normalize_update(&update, &[42]).expect("inbound");
        let runner = ResettableRunner::default();

        process_telegram_update(&runner, &config, update, inbound).await?;

        assert_eq!(runner.resets.lock().await.as_slice(), &[(100, Some(321))]);

        let requests = mock.requests().await;
        let thread_ids: Vec<Option<i64>> = requests
            .iter()
            .filter(|request| request.path.ends_with("/sendMessage"))
            .map(|request| {
                let value: Value = serde_json::from_slice(&request.body).expect("sendMessage json");
                value.get("message_thread_id").and_then(Value::as_i64)
            })
            .collect();
        assert_eq!(thread_ids, vec![Some(321)]);
        Ok(())
    }

    #[tokio::test]
    async fn process_update_replies_into_private_message_thread() -> Result<()> {
        let mock = start_mock_telegram_api().await?;
        let tempdir = TempDir::new()?;
        let config = test_config(tempdir.path(), &mock.base_url);
        let mut update = sample_update();
        update.message.as_mut().expect("message").message_thread_id = Some(777);
        let inbound = normalize_update(&update, &[42]).expect("inbound");
        let runner = FakeTurnRunner {
            scripted_outputs: vec![TurnOutput::Markdown {
                text: "Reply in thread".to_string(),
                disable_notification: true,
            }],
        };

        process_telegram_update(&runner, &config, update, inbound).await?;

        let requests = mock.requests().await;
        let send_message = requests
            .iter()
            .find(|request| request.path.ends_with("/sendMessage"))
            .expect("sendMessage request");
        let value: Value = serde_json::from_slice(&send_message.body).expect("sendMessage json");
        assert_eq!(value["message_thread_id"].as_i64(), Some(777));
        assert_eq!(value["disable_notification"].as_bool(), Some(true));
        Ok(())
    }

    #[tokio::test]
    async fn process_update_transcribes_voice_before_running_turn() -> Result<()> {
        let mock = start_mock_telegram_api().await?;
        let tempdir = TempDir::new()?;
        let mut config = test_config(tempdir.path(), &mock.base_url);
        config.voice.transcriber_command =
            write_fake_parakeet_script(tempdir.path(), "decoded voice text")?;

        let update = sample_voice_update();
        let inbound = normalize_update(&update, &[42]).expect("inbound");
        let runner = CapturingRunner::default();

        process_telegram_update(&runner, &config, update, inbound).await?;

        let requests = runner.requests.lock().await;
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].text, None);

        let attachments = runner.attachments.lock().await;
        match attachments[0].as_ref().expect("attachment") {
            DownloadedAttachment::Voice {
                transcript,
                duration_seconds,
                ..
            } => {
                assert_eq!(transcript.as_deref(), Some("decoded voice text"));
                assert_eq!(*duration_seconds, Some(7));
            }
            other => panic!("expected voice attachment, got {other:?}"),
        }

        let requests = mock.requests().await;
        assert!(
            requests
                .iter()
                .any(|request| request.path.ends_with("/getFile")),
            "expected Telegram getFile request"
        );
        Ok(())
    }

    #[tokio::test]
    #[ignore = "requires live Codex auth/network; run manually"]
    async fn e2e_live_codex_streams_intermediate_messages_to_mock_telegram() -> Result<()> {
        let mock = start_mock_telegram_api().await?;
        let tempdir = TempDir::new()?;
        let config = live_e2e_config(tempdir.path(), &mock.base_url)?;
        let sessions = SessionManager::new(config.clone(), ApprovalManager::new()).await?;
        sessions.ensure_orchestrator().await?;

        let update = sample_update_with_text(
            "Before the final answer, send two short intermediary user-visible updates exactly `STEP_ONE` and `STEP_TWO` in separate messages while you think. Then send final answer exactly `DONE`.",
        );
        let inbound = normalize_update(&update, &[42]).expect("inbound");

        process_telegram_update(&sessions, &config, update, inbound).await?;

        let requests = mock.requests().await;
        let messages: Vec<String> = requests
            .iter()
            .filter(|request| request.path.ends_with("/sendMessage"))
            .map(|request| {
                let value: Value = serde_json::from_slice(&request.body).expect("sendMessage json");
                value["text"].as_str().unwrap_or_default().to_string()
            })
            .collect();

        assert!(
            messages.len() >= 3,
            "expected at least three Telegram messages (two commentary + final), got {:?}",
            messages
        );

        let step_one_index = messages
            .iter()
            .position(|message| message.trim() == "STEP_ONE")
            .unwrap_or_else(|| {
                panic!(
                    "expected STEP_ONE as its own message, transcript:\n{}",
                    messages.join("\n---\n")
                )
            });
        let step_two_index = messages
            .iter()
            .position(|message| message.trim() == "STEP_TWO")
            .unwrap_or_else(|| {
                panic!(
                    "expected STEP_TWO as its own message, transcript:\n{}",
                    messages.join("\n---\n")
                )
            });
        let done_index = messages
            .iter()
            .position(|message| message.trim() == "DONE")
            .unwrap_or_else(|| {
                panic!(
                    "expected DONE as its own message, transcript:\n{}",
                    messages.join("\n---\n")
                )
            });

        assert!(
            step_one_index < step_two_index && step_two_index < done_index,
            "expected STEP_ONE -> STEP_TWO -> DONE order, got transcript:\n{}",
            messages.join("\n---\n")
        );
        Ok(())
    }

    #[tokio::test]
    #[ignore = "requires live Codex auth/network; run manually"]
    async fn e2e_live_codex_time_tool_outputs_do_not_fragment_into_tiny_messages() -> Result<()> {
        let mock = start_mock_telegram_api().await?;
        let tempdir = TempDir::new()?;
        let config = live_e2e_config(tempdir.path(), &mock.base_url)?;
        let sessions = SessionManager::new(config.clone(), ApprovalManager::new()).await?;
        sessions.ensure_orchestrator().await?;

        let update = sample_update_with_text(
            "Use the external time service to check the current time in UTC+02:00. While you work, send exactly two intermediate user-visible updates, `STEP_ONE` and `STEP_TWO`, and then return one short final answer in English.",
        );
        let inbound = normalize_update(&update, &[42]).expect("inbound");

        process_telegram_update(&sessions, &config, update, inbound).await?;

        let requests = mock.requests().await;
        let messages: Vec<String> = requests
            .iter()
            .filter(|request| request.path.ends_with("/sendMessage"))
            .map(|request| {
                let value: Value = serde_json::from_slice(&request.body).expect("sendMessage json");
                value["text"].as_str().unwrap_or_default().to_string()
            })
            .collect();

        assert_eq!(
            messages.len(),
            3,
            "expected exactly three Telegram messages (STEP_ONE, STEP_TWO, final), got {:?}",
            messages
        );
        assert_eq!(
            messages[0].trim(),
            "STEP_ONE",
            "expected first message to be STEP_ONE, transcript:\n{}",
            messages.join("\n---\n")
        );
        assert_eq!(
            messages[1].trim(),
            "STEP_TWO",
            "expected second message to be STEP_TWO, transcript:\n{}",
            messages.join("\n---\n")
        );
        assert!(
            !messages[2].contains("STEP_ONE")
                && !messages[2].contains("STEP_TWO")
                && !messages[2].trim().is_empty(),
            "expected a standalone final answer, got transcript:\n{}",
            messages.join("\n---\n")
        );
        Ok(())
    }

    #[tokio::test]
    #[ignore = "requires live Codex auth/network; run manually"]
    async fn e2e_live_codex_keeps_telegram_threads_separate_via_named_sessions() -> Result<()> {
        let mock = start_mock_telegram_api().await?;
        let tempdir = TempDir::new()?;
        let config = live_e2e_config(tempdir.path(), &mock.base_url)?;

        let sessions = SessionManager::new(config.clone(), ApprovalManager::new()).await?;
        sessions.ensure_orchestrator().await?;
        let alpha_store = sample_group_thread_update_with_text(
            "For this Telegram thread only, remember the token ALPHA_THREAD_ONLY. Do not mention any other thread. Reply with exactly STORED_ALPHA and nothing else.",
            111,
        );
        let inbound = normalize_update(&alpha_store, &[42]).expect("inbound");
        process_telegram_update(&sessions, &config, alpha_store, inbound).await?;

        let sessions = SessionManager::new(config.clone(), ApprovalManager::new()).await?;
        sessions.ensure_orchestrator().await?;
        let beta_check = sample_group_thread_update_with_text(
            "Reply with exactly THREAD_BETA_EMPTY if this Telegram thread has not yet been told any token. Do not guess and do not mention other threads.",
            222,
        );
        let inbound = normalize_update(&beta_check, &[42]).expect("inbound");
        process_telegram_update(&sessions, &config, beta_check, inbound).await?;

        let sessions = SessionManager::new(config.clone(), ApprovalManager::new()).await?;
        sessions.ensure_orchestrator().await?;
        let alpha_recall = sample_group_thread_update_with_text(
            "Reply with exactly ALPHA_THREAD_ONLY if you remember the token previously stored in this same Telegram thread.",
            111,
        );
        let inbound = normalize_update(&alpha_recall, &[42]).expect("inbound");
        process_telegram_update(&sessions, &config, alpha_recall, inbound).await?;

        let requests = mock.requests().await;
        let send_messages: Vec<(Option<i64>, String)> = requests
            .iter()
            .filter(|request| request.path.ends_with("/sendMessage"))
            .map(|request| {
                let value: Value = serde_json::from_slice(&request.body).expect("sendMessage json");
                (
                    value.get("message_thread_id").and_then(Value::as_i64),
                    value["text"]
                        .as_str()
                        .unwrap_or_default()
                        .trim()
                        .to_string(),
                )
            })
            .collect();

        assert!(
            send_messages
                .iter()
                .any(|(thread_id, text)| *thread_id == Some(111) && text == "STORED_ALPHA"),
            "expected STORED_ALPHA in Telegram thread 111, got {:?}",
            send_messages
        );
        assert!(
            send_messages
                .iter()
                .find(|(thread_id, _)| *thread_id == Some(222))
                .is_some_and(|(_, text)| !text.contains("ALPHA_THREAD_ONLY")),
            "expected Telegram thread 222 to stay isolated from ALPHA_THREAD_ONLY, got {:?}",
            send_messages
        );
        assert!(
            send_messages
                .iter()
                .any(|(thread_id, text)| *thread_id == Some(111) && text == "ALPHA_THREAD_ONLY"),
            "expected ALPHA_THREAD_ONLY recall in Telegram thread 111, got {:?}",
            send_messages
        );
        Ok(())
    }

    fn test_config(working_directory: &std::path::Path, api_base_url: &str) -> AppConfig {
        AppConfig {
            paths: AppPaths {
                config_path: working_directory.join("config.toml"),
                state_dir: working_directory.join("state"),
                cache_dir: working_directory.join("cache"),
            },
            server: ServerConfig {
                listen: "127.0.0.1:0".to_string(),
            },
            telegram: TelegramConfig {
                bot_token: "token".to_string(),
                allowed_user_ids: vec![42],
                public_base_url: "https://example.com".to_string(),
                webhook_secret: "secret".to_string(),
                api_base_url: api_base_url.to_string(),
            },
            whatsapp: None,
            operator: OperatorConfig {
                telegram: Some(OperatorTelegramConfig {
                    chat_id: 100,
                    thread_id: Some(777),
                }),
            },
            voice: VoiceConfig {
                enabled: true,
                transcriber_command: "parakeet-mlx".to_string(),
                model: "mlx-community/parakeet-tdt-0.6b-v3".to_string(),
            },
            codex: CodexConfig {
                connect_url: "ws://127.0.0.1:4222".to_string(),
                listen_url: "ws://127.0.0.1:4222".to_string(),
                reuse_existing_server: true,
                experimental_api: false,
                working_directory: working_directory.to_path_buf(),
                model: None,
                additional_directories: Vec::new(),
                approval_policy: "never".to_string(),
                sandbox_mode: "danger-full-access".to_string(),
                skip_git_repo_check: true,
                network_access_enabled: true,
                web_search_enabled: true,
                orchestrator_name: None,
            },
        }
    }

    fn sample_voice_update() -> TelegramUpdate {
        TelegramUpdate {
            update_id: 2,
            message: Some(TelegramMessage {
                message_id: 11,
                chat: TelegramChat { id: 100 },
                from: Some(TelegramUser {
                    id: 42,
                    first_name: Some("Test".to_string()),
                    last_name: None,
                }),
                sender_chat: None,
                text: None,
                caption: None,
                photo: None,
                document: None,
                voice: Some(TelegramVoice {
                    file_id: "voice-1".to_string(),
                    duration: Some(7),
                }),
                message_thread_id: None,
            }),
            edited_message: None,
            callback_query: None,
        }
    }

    fn live_e2e_config(temp_root: &std::path::Path, api_base_url: &str) -> Result<AppConfig> {
        let config_path = std::env::var("CODEXCLAW_E2E_CONFIG")
            .map(PathBuf::from)
            .unwrap_or_else(|_| {
                dirs::config_dir()
                    .unwrap_or_else(|| PathBuf::from("/tmp"))
                    .join("codexclaw")
                    .join("config.toml")
            });

        let mut config = AppConfig::load(Some(config_path.clone()))?;
        config.paths = AppPaths {
            config_path,
            state_dir: temp_root.join("state"),
            cache_dir: temp_root.join("cache"),
        };
        std::fs::create_dir_all(&config.paths.state_dir)?;
        std::fs::create_dir_all(&config.paths.cache_dir)?;
        config.telegram.api_base_url = api_base_url.to_string();
        config.telegram.allowed_user_ids = vec![42];
        config.telegram.bot_token = "token".to_string();
        config.telegram.public_base_url = "https://example.com".to_string();
        config.telegram.webhook_secret = "secret".to_string();
        config.codex.model = Some("gpt-5.4-mini".to_string());
        Ok(config)
    }

    fn sample_update() -> TelegramUpdate {
        sample_update_with_text("hello")
    }

    fn sample_update_with_text(text: &str) -> TelegramUpdate {
        TelegramUpdate {
            update_id: 1,
            message: Some(TelegramMessage {
                message_id: 10,
                chat: TelegramChat { id: 100 },
                from: Some(TelegramUser {
                    id: 42,
                    first_name: Some("Test".to_string()),
                    last_name: None,
                }),
                sender_chat: None,
                text: Some(text.to_string()),
                caption: None,
                photo: None,
                document: None,
                voice: None,
                message_thread_id: None,
            }),
            edited_message: None,
            callback_query: None,
        }
    }

    fn sample_group_thread_update_with_text(text: &str, thread_id: i64) -> TelegramUpdate {
        TelegramUpdate {
            update_id: 3,
            message: Some(TelegramMessage {
                message_id: 12,
                chat: TelegramChat { id: 100 },
                from: Some(TelegramUser {
                    id: 42,
                    first_name: Some("Test".to_string()),
                    last_name: None,
                }),
                sender_chat: None,
                text: Some(text.to_string()),
                caption: None,
                photo: None,
                document: None,
                voice: None,
                message_thread_id: Some(thread_id),
            }),
            edited_message: None,
            callback_query: None,
        }
    }

    fn telegram_target_from_session_key(session_key: &str) -> (i64, Option<i64>) {
        let rest = session_key
            .strip_prefix("telegram:default:")
            .expect("telegram session key");
        let mut parts = rest.split(':');
        let chat_id = parts
            .next()
            .expect("chat id")
            .parse::<i64>()
            .expect("chat id int");
        let thread_id = parts
            .next()
            .map(|value| value.parse::<i64>().expect("thread id int"));
        (chat_id, thread_id)
    }

    struct MockTelegramApi {
        base_url: String,
        state: MockTelegramState,
    }

    impl MockTelegramApi {
        async fn requests(&self) -> Vec<RecordedRequest> {
            self.state.requests.lock().await.clone()
        }
    }

    async fn start_mock_telegram_api() -> Result<MockTelegramApi> {
        async fn record_request(
            State(state): State<MockTelegramState>,
            uri: axum::http::Uri,
            body: Bytes,
        ) -> Json<Value> {
            state.requests.lock().await.push(RecordedRequest {
                path: uri.path().to_string(),
                body: body.to_vec(),
            });
            if uri.path().ends_with("/getFile") {
                Json(json!({ "ok": true, "result": { "file_path": "voice/file.ogg" } }))
            } else if uri.path().ends_with("/sendMessage") {
                Json(json!({ "ok": true, "result": { "message_id": 500 } }))
            } else {
                Json(json!({ "ok": true, "result": {} }))
            }
        }

        async fn file_response() -> &'static [u8] {
            b"file"
        }

        let state = MockTelegramState::default();
        let app = Router::new()
            .route("/{*path}", post(record_request))
            .route("/file/{*path}", get(file_response))
            .with_state(state.clone());

        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        Ok(MockTelegramApi {
            base_url: format!("http://{}", address),
            state,
        })
    }

    fn write_fake_parakeet_script(root: &std::path::Path, transcript: &str) -> Result<String> {
        let script_path = root.join("fake-parakeet.sh");
        let script = format!(
            "#!/bin/sh\nset -eu\noutput_dir=''\ntemplate=''\nwhile [ \"$#\" -gt 0 ]; do\n  case \"$1\" in\n    --output-dir)\n      output_dir=\"$2\"\n      shift 2\n      ;;\n    --output-template)\n      template=\"$2\"\n      shift 2\n      ;;\n    *)\n      shift\n      ;;\n  esac\ndone\nmkdir -p \"$output_dir\"\nprintf '%s\\n' '{}' > \"$output_dir/$template.txt\"\n",
            transcript.replace('\'', "'\"'\"'")
        );
        std::fs::write(&script_path, script)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = std::fs::metadata(&script_path)?.permissions();
            permissions.set_mode(0o755);
            std::fs::set_permissions(&script_path, permissions)?;
        }
        Ok(script_path.display().to_string())
    }
}
