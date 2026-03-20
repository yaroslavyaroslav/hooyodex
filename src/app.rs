use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{Context, Result};
use axum::extract::{Path as AxumPath, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::json;
use tokio::signal;
use tokio::sync::RwLock;
use tracing::{error, info};

use crate::codex::{ChatTurnRunner, OutputSink, SessionManager, TurnOutput};
use crate::config::AppConfig;
use crate::telegram::{
    DownloadedAttachment, TelegramUpdate, download_attachment, normalize_update,
    send_markdown_message, send_photo,
};
use crate::transcription::transcribe_voice_message;

#[derive(Clone)]
struct AppState {
    config: Arc<RwLock<AppConfig>>,
    sessions: Arc<dyn ChatTurnRunner>,
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
    AxumPath(secret): AxumPath<String>,
    Json(update): Json<TelegramUpdate>,
) -> StatusCode {
    let config = state.config.read().await.clone();
    if secret != config.telegram.webhook_secret {
        return StatusCode::NOT_FOUND;
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
        runner.reset_chat(&inbound).await?;
        send_markdown_message(
            config,
            inbound.chat_id,
            inbound.thread_id,
            "Started a fresh chat. History cleared for this Telegram chat.",
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
            let _ = send_markdown_message(
                config,
                inbound.chat_id,
                inbound.thread_id,
                &format!("Attachment download failed:\n\n```text\n{error}\n```"),
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
            let _ = send_markdown_message(
                config,
                inbound.chat_id,
                inbound.thread_id,
                &format!("Voice transcription failed:\n\n```text\n{error}\n```"),
            )
            .await;
            return Ok(());
        }
    };

    let chat_id = inbound.chat_id;
    let thread_id = inbound.thread_id;
    let message_id = inbound.message_id;
    let mut sink = TelegramSink {
        config: config.clone(),
        chat_id,
        thread_id,
    };

    match runner.run_chat_turn(inbound, attachment, &mut sink).await {
        Ok(()) => Ok(()),
        Err(error) => {
            error!(chat_id, message_id, "Codex turn failed: {error}");
            let _ = send_markdown_message(
                config,
                chat_id,
                thread_id,
                &format!("Turn failed:\n\n```text\n{error}\n```"),
            )
            .await;
            Ok(())
        }
    }
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

struct TelegramSink {
    config: AppConfig,
    chat_id: i64,
    thread_id: Option<i64>,
}

#[async_trait::async_trait]
impl OutputSink for TelegramSink {
    async fn send(&mut self, output: TurnOutput) -> Result<()> {
        match output {
            TurnOutput::Markdown(markdown) => {
                send_markdown_message(&self.config, self.chat_id, self.thread_id, &markdown).await
            }
            TurnOutput::Image(path) => {
                send_photo(&self.config, self.chat_id, self.thread_id, &path, None).await
            }
        }
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

    use crate::codex::TurnOutput;
    use crate::config::{
        AppConfig, AppPaths, CodexConfig, ServerConfig, TelegramConfig, VoiceConfig,
    };
    use crate::telegram::{
        DownloadedAttachment, InboundMessage, TelegramChat, TelegramMessage, TelegramUpdate,
        TelegramUser, TelegramVoice, normalize_update,
    };

    #[derive(Clone, Debug)]
    struct RecordedRequest {
        path: String,
        body: Vec<u8>,
    }

    #[derive(Clone, Default)]
    struct MockTelegramState {
        requests: Arc<Mutex<Vec<RecordedRequest>>>,
    }

    struct FakeTurnRunner {
        scripted_outputs: Vec<TurnOutput>,
    }

    #[async_trait::async_trait]
    impl ChatTurnRunner for FakeTurnRunner {
        async fn reset_chat(&self, _inbound: &InboundMessage) -> Result<()> {
            Ok(())
        }

        async fn run_chat_turn(
            &self,
            _inbound: InboundMessage,
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
        inbound: Arc<Mutex<Vec<InboundMessage>>>,
        attachments: Arc<Mutex<Vec<Option<DownloadedAttachment>>>>,
    }

    #[async_trait::async_trait]
    impl ChatTurnRunner for CapturingRunner {
        async fn reset_chat(&self, _inbound: &InboundMessage) -> Result<()> {
            Ok(())
        }

        async fn run_chat_turn(
            &self,
            inbound: InboundMessage,
            attachment: Option<DownloadedAttachment>,
            _sink: &mut dyn OutputSink,
        ) -> Result<()> {
            self.inbound.lock().await.push(inbound);
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
                TurnOutput::Markdown("First intermediate".to_string()),
                TurnOutput::Markdown("Second intermediate".to_string()),
                TurnOutput::Markdown("Final answer".to_string()),
            ],
        };

        process_telegram_update(&runner, &config, update, inbound).await?;

        let requests = mock.requests().await;
        let messages: Vec<String> = requests
            .iter()
            .filter(|request| request.path.ends_with("/sendMessage"))
            .map(|request| {
                let value: Value = serde_json::from_slice(&request.body).expect("sendMessage json");
                value["text"].as_str().unwrap_or_default().to_string()
            })
            .collect();

        assert_eq!(messages.len(), 3);
        assert!(
            messages
                .iter()
                .any(|text| text.contains("First intermediate"))
        );
        assert!(
            messages
                .iter()
                .any(|text| text.contains("Second intermediate"))
        );
        assert!(messages.iter().any(|text| text.contains("Final answer")));
        Ok(())
    }

    #[tokio::test]
    async fn process_update_resets_chat_on_new_command() -> Result<()> {
        #[derive(Default)]
        struct ResettableRunner {
            resets: Arc<Mutex<Vec<i64>>>,
            turns: Arc<Mutex<u32>>,
        }

        #[async_trait::async_trait]
        impl ChatTurnRunner for ResettableRunner {
            async fn reset_chat(&self, inbound: &InboundMessage) -> Result<()> {
                self.resets.lock().await.push(inbound.chat_id);
                Ok(())
            }

            async fn run_chat_turn(
                &self,
                _inbound: InboundMessage,
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
        let update = sample_update_with_text("/new");
        let inbound = normalize_update(&update, &[42]).expect("inbound");
        let runner = ResettableRunner::default();

        process_telegram_update(&runner, &config, update, inbound).await?;

        assert_eq!(runner.resets.lock().await.as_slice(), &[100]);
        assert_eq!(*runner.turns.lock().await, 0);

        let requests = mock.requests().await;
        let messages: Vec<String> = requests
            .iter()
            .filter(|request| request.path.ends_with("/sendMessage"))
            .map(|request| {
                let value: Value = serde_json::from_slice(&request.body).expect("sendMessage json");
                value["text"].as_str().unwrap_or_default().to_string()
            })
            .collect();
        assert_eq!(messages.len(), 1);
        assert!(messages[0].contains("fresh chat"));
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

        let inbounds = runner.inbound.lock().await;
        assert_eq!(inbounds.len(), 1);
        assert_eq!(inbounds[0].text, None);

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
        let sessions = SessionManager::new(config.clone()).await?;
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

        let transcript = messages.join("\n---\n");
        assert!(
            messages.iter().any(|message| message.contains("STEP_ONE")),
            "expected a streamed Telegram message containing STEP_ONE, got:\n{transcript}"
        );
        assert!(
            messages.iter().any(|message| message.contains("STEP_TWO")),
            "expected a streamed Telegram message containing STEP_TWO, got:\n{transcript}"
        );
        assert!(
            messages.iter().any(|message| message.contains("DONE")),
            "expected a final Telegram message containing DONE, got:\n{transcript}"
        );
        Ok(())
    }

    #[tokio::test]
    #[ignore = "requires live Codex auth/network; run manually"]
    async fn e2e_live_codex_time_tool_outputs_do_not_fragment_into_tiny_messages() -> Result<()> {
        let mock = start_mock_telegram_api().await?;
        let tempdir = TempDir::new()?;
        let config = live_e2e_config(tempdir.path(), &mock.base_url)?;
        let sessions = SessionManager::new(config.clone()).await?;
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

        let transcript = messages.join("\n---\n");
        assert!(
            !messages.is_empty(),
            "expected at least one Telegram message, got none"
        );
        assert!(
            messages.iter().any(|message| message.contains("STEP_ONE")),
            "expected an intermediate Telegram message containing STEP_ONE, got:\n{transcript}"
        );
        assert!(
            messages.iter().any(|message| message.contains("STEP_TWO")),
            "expected an intermediate Telegram message containing STEP_TWO, got:\n{transcript}"
        );
        assert!(
            !messages
                .iter()
                .any(|message| message == "через" || message == "внешний" || message == "источник"),
            "expected no tiny delta fragments, got:\n{transcript}"
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
            voice: VoiceConfig {
                enabled: true,
                transcriber_command: "parakeet-mlx".to_string(),
                model: "mlx-community/parakeet-tdt-0.6b-v3".to_string(),
            },
            codex: CodexConfig {
                connect_url: "ws://127.0.0.1:4222".to_string(),
                listen_url: "ws://127.0.0.1:4222".to_string(),
                reuse_existing_server: true,
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
                chat: TelegramChat {
                    id: 100,
                    kind: "private".to_string(),
                },
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
                chat: TelegramChat {
                    id: 100,
                    kind: "private".to_string(),
                },
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
        }
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
