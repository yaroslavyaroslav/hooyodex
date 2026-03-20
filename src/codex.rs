use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Result, anyhow};
use codex_app_server_sdk::api::{
    AgentMessagePhase, ApprovalMode, Codex, Input, SandboxMode, Thread, ThreadEvent, ThreadOptions,
    TurnOptions, UserInput, WebSearchMode,
};
use codex_app_server_sdk::{WsConfig, WsServerHandle, WsStartConfig};
use tokio::sync::Mutex;
use tracing::warn;

use crate::config::AppConfig;
use crate::state::PersistentState;
use crate::telegram::{DownloadedAttachment, InboundMessage};

pub struct CodexRuntime {
    pub codex: Codex,
    pub _server: WsServerHandle,
}

pub struct SessionManager {
    runtime: CodexRuntime,
    config: AppConfig,
    sessions_path: PathBuf,
    state: Mutex<PersistentState>,
    chats: Mutex<HashMap<String, Arc<Mutex<Thread>>>>,
    orchestrator: Mutex<Option<Arc<Mutex<Thread>>>>,
}

#[derive(Debug, Clone)]
pub struct TurnOutput {
    pub markdown_messages: Vec<String>,
}

impl CodexRuntime {
    pub async fn start(config: &AppConfig) -> Result<Self> {
        let server = Codex::start_ws_daemon(WsStartConfig {
            listen_url: config.codex.listen_url.clone(),
            connect_url: config.codex.connect_url.clone(),
            env: Default::default(),
            reuse_existing: config.codex.reuse_existing_server,
        })
        .await?;
        let codex =
            Codex::connect_ws(WsConfig::default().with_url(config.codex.connect_url.clone()))
                .await?;
        Ok(Self {
            codex,
            _server: server,
        })
    }
}

impl SessionManager {
    pub async fn new(config: AppConfig) -> Result<Self> {
        let runtime = CodexRuntime::start(&config).await?;
        let state = PersistentState::load(&config.sessions_path())?;
        Ok(Self {
            runtime,
            sessions_path: config.sessions_path(),
            config,
            state: Mutex::new(state),
            chats: Mutex::new(HashMap::new()),
            orchestrator: Mutex::new(None),
        })
    }

    pub async fn ensure_orchestrator(&self) -> Result<()> {
        let mut orchestrator = self.orchestrator.lock().await;
        if orchestrator.is_some() {
            return Ok(());
        }

        let thread_id = self.state.lock().await.orchestrator_thread_id.clone();
        let thread = if let Some(thread_id) = thread_id {
            self.runtime
                .codex
                .resume_thread_by_id(thread_id, self.base_thread_options())
        } else {
            self.runtime.codex.start_thread(self.base_thread_options())
        };

        let arc = Arc::new(Mutex::new(thread));
        if self.state.lock().await.orchestrator_thread_id.is_none() {
            let mut thread = arc.lock().await;
            let _ = thread
                .run(
                    "You are the persistent top-level orchestrator for this CodexClaw service. Maintain long-lived orchestration context. Only create automations or cron-style tasks when explicitly requested.",
                    TurnOptions::default(),
                )
                .await?;
            if let Some(id) = thread.id().map(ToString::to_string) {
                let mut state = self.state.lock().await;
                state.orchestrator_thread_id = Some(id);
                state.save(&self.sessions_path)?;
            }
        }
        *orchestrator = Some(arc);
        Ok(())
    }

    pub async fn run_chat_turn(
        &self,
        inbound: InboundMessage,
        attachment: Option<DownloadedAttachment>,
    ) -> Result<TurnOutput> {
        let key = inbound.chat_id.to_string();
        let session = self.get_or_create_chat_thread(&key).await?;
        let input = self.build_input(&inbound, attachment);

        let mut sent_ids = HashSet::new();
        let mut messages = Vec::new();

        let mut thread = session.lock().await;
        let mut streamed = thread.run_streamed(input, TurnOptions::default()).await?;
        while let Some(next) = streamed.next_event().await {
            match next? {
                ThreadEvent::ItemCompleted { item } => {
                    if let codex_app_server_sdk::api::ThreadItem::AgentMessage(message) = item {
                        if message.text.trim().is_empty() {
                            continue;
                        }
                        if !sent_ids.insert(message.id.clone()) {
                            continue;
                        }
                        if matches!(
                            message.phase,
                            Some(AgentMessagePhase::Commentary)
                                | Some(AgentMessagePhase::FinalAnswer)
                                | Some(AgentMessagePhase::Unknown)
                                | None
                        ) {
                            messages.push(message.text);
                        }
                    }
                }
                ThreadEvent::TurnCompleted { .. } => break,
                ThreadEvent::TurnFailed { error } => {
                    return Err(anyhow!(error.message));
                }
                ThreadEvent::Error { message } => return Err(anyhow!(message)),
                ThreadEvent::ThreadStarted { .. }
                | ThreadEvent::TurnStarted
                | ThreadEvent::ItemStarted { .. }
                | ThreadEvent::ItemUpdated { .. } => {}
            }
        }

        if let Some(id) = thread.id().map(ToString::to_string) {
            let mut state = self.state.lock().await;
            if state.chat_threads.get(&key) != Some(&id) {
                state.chat_threads.insert(key, id);
                state.save(&self.sessions_path)?;
            }
        }

        Ok(TurnOutput {
            markdown_messages: messages,
        })
    }

    async fn get_or_create_chat_thread(&self, key: &str) -> Result<Arc<Mutex<Thread>>> {
        if let Some(existing) = self.chats.lock().await.get(key).cloned() {
            return Ok(existing);
        }

        let existing_id = self.state.lock().await.chat_threads.get(key).cloned();
        let thread = if let Some(thread_id) = existing_id {
            self.runtime
                .codex
                .resume_thread_by_id(thread_id, self.base_thread_options())
        } else {
            self.runtime.codex.start_thread(self.base_thread_options())
        };
        let session = Arc::new(Mutex::new(thread));
        self.chats
            .lock()
            .await
            .insert(key.to_string(), session.clone());
        Ok(session)
    }

    fn build_input(
        &self,
        inbound: &InboundMessage,
        attachment: Option<DownloadedAttachment>,
    ) -> Input {
        match attachment {
            Some(DownloadedAttachment::Image(path)) => {
                let mut items = Vec::new();
                if let Some(text) = &inbound.text {
                    items.push(UserInput::Text { text: text.clone() });
                } else {
                    items.push(UserInput::Text {
                        text: format!(
                            "User {} attached an image. Reply to them in the same chat.",
                            inbound.sender_name
                        ),
                    });
                }
                items.push(UserInput::LocalImage {
                    path: path.display().to_string(),
                });
                Input::items(items)
            }
            Some(DownloadedAttachment::Voice {
                path,
                duration_seconds,
            }) => {
                let duration = duration_seconds
                    .map(|seconds| format!(" ({seconds}s)"))
                    .unwrap_or_default();
                let prompt = format!(
                    "Telegram user {} sent a voice message{}.\nLocal file path: `{}`.\nUse that file directly if needed.\n\n{}",
                    inbound.sender_name,
                    duration,
                    path.display(),
                    inbound.text.clone().unwrap_or_default()
                );
                Input::text(prompt)
            }
            Some(DownloadedAttachment::File(path)) => {
                let prompt = format!(
                    "Telegram user {} sent a file.\nLocal file path: `{}`.\nUse that file directly if needed.\n\n{}",
                    inbound.sender_name,
                    path.display(),
                    inbound.text.clone().unwrap_or_default()
                );
                Input::text(prompt)
            }
            None => Input::text(inbound.text.clone().unwrap_or_default()),
        }
    }

    fn base_thread_options(&self) -> ThreadOptions {
        let mut builder = ThreadOptions::builder()
            .working_directory(self.config.codex.working_directory.display().to_string())
            .skip_git_repo_check(self.config.codex.skip_git_repo_check)
            .network_access_enabled(self.config.codex.network_access_enabled)
            .web_search_enabled(self.config.codex.web_search_enabled)
            .web_search_mode(WebSearchMode::Live)
            .additional_directories(self.additional_directories());

        if let Some(model) = &self.config.codex.model {
            builder = builder.model(model.clone());
        }

        builder = match self.config.codex.approval_policy.as_str() {
            "on-request" => builder.approval_policy(ApprovalMode::OnRequest),
            "on-failure" => builder.approval_policy(ApprovalMode::OnFailure),
            "untrusted" => builder.approval_policy(ApprovalMode::Untrusted),
            _ => builder.approval_policy(ApprovalMode::Never),
        };

        builder = match self.config.codex.sandbox_mode.as_str() {
            "read-only" => builder.sandbox_mode(SandboxMode::ReadOnly),
            "workspace-write" => builder.sandbox_mode(SandboxMode::WorkspaceWrite),
            _ => builder.sandbox_mode(SandboxMode::DangerFullAccess),
        };

        builder.build()
    }

    fn additional_directories(&self) -> Vec<String> {
        let mut directories = Vec::new();
        directories.push(self.config.attachment_root().display().to_string());
        directories.extend(
            self.config
                .codex
                .additional_directories
                .iter()
                .map(|path| path.display().to_string()),
        );
        directories
    }
}

impl Drop for SessionManager {
    fn drop(&mut self) {
        if let Ok(state) = self.state.try_lock()
            && let Err(error) = state.save(&self.sessions_path)
        {
            warn!("failed to persist session state on drop: {error}");
        }
    }
}
