use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Result, anyhow};
use async_trait::async_trait;
use codex_app_server_sdk::api::{
    AgentMessageItem, AgentMessagePhase, ApprovalMode, Codex, DynamicToolSpec, Input, SandboxMode,
    Thread, ThreadItem, ThreadOptions, TurnOptions, UserInput, WebSearchMode,
};
use codex_app_server_sdk::events::{ServerEvent, ServerNotification};
use codex_app_server_sdk::protocol::notifications::ItemLifecycleNotification;
use codex_app_server_sdk::protocol::{requests, server_requests};
use codex_app_server_sdk::{ClientError, CodexClient, WsConfig, WsServerHandle, WsStartConfig};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use tokio::sync::{Mutex, Notify, mpsc};
use tracing::{debug, warn};

use crate::config::AppConfig;
use crate::state::PersistentState;
use crate::telegram::{DownloadedAttachment, InboundMessage};

pub struct CodexRuntime {
    pub codex: Codex,
    pub _server: WsServerHandle,
    tool_router: Arc<DynamicToolRouter>,
}

pub struct SessionManager {
    runtime: CodexRuntime,
    config: AppConfig,
    sessions_path: PathBuf,
    state: Mutex<PersistentState>,
    chats: Mutex<HashMap<String, Arc<Mutex<Thread>>>>,
    active_turns: Mutex<HashMap<String, Arc<ActiveChatTurn>>>,
    orchestrator: Mutex<Option<Arc<Mutex<Thread>>>>,
}

#[derive(Debug, Clone)]
pub enum TurnOutput {
    Markdown {
        text: String,
        disable_notification: bool,
    },
    Media {
        kind: TelegramMediaKind,
        path: PathBuf,
        caption_markdown: Option<String>,
        file_name: Option<String>,
        mime_type: Option<String>,
        disable_notification: bool,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TelegramMediaKind {
    Photo,
    Document,
    Audio,
    Voice,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
struct TelegramSendMediaArgs {
    path: String,
    kind: TelegramMediaKind,
    #[serde(default)]
    caption_markdown: Option<String>,
    #[serde(default)]
    file_name: Option<String>,
    #[serde(default)]
    mime_type: Option<String>,
    #[serde(default)]
    disable_notification: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DynamicToolCallEnvelope {
    tool: String,
    turn_id: String,
    arguments: Value,
}

#[derive(Default)]
struct DynamicToolRouter {
    allowed_roots: Vec<PathBuf>,
    turn_outputs: Mutex<HashMap<String, mpsc::Sender<TurnOutput>>>,
}

impl DynamicToolRouter {
    fn new(config: &AppConfig) -> Self {
        let mut allowed_roots = vec![
            config.codex.working_directory.clone(),
            config.attachment_root(),
        ];
        append_temp_allow_roots(&mut allowed_roots);
        allowed_roots.extend(config.codex.additional_directories.iter().cloned());
        Self {
            allowed_roots,
            turn_outputs: Mutex::new(HashMap::new()),
        }
    }

    async fn register_turn(&self, turn_id: &str, sender: mpsc::Sender<TurnOutput>) {
        self.turn_outputs
            .lock()
            .await
            .insert(turn_id.to_string(), sender);
    }

    async fn unregister_turn(&self, turn_id: &str) {
        self.turn_outputs.lock().await.remove(turn_id);
    }

    async fn handle_call(
        &self,
        params: server_requests::DynamicToolCallParams,
    ) -> std::result::Result<server_requests::DynamicToolCallResponse, ClientError> {
        let envelope = parse_dynamic_tool_call_envelope(&params)?;
        if envelope.tool != "telegram_send_media" {
            return Err(ClientError::InvalidMessage(format!(
                "unsupported dynamic tool `{}`",
                envelope.tool
            )));
        }

        let arguments: TelegramSendMediaArgs = serde_json::from_value(envelope.arguments)?;
        let display_name = arguments
            .file_name
            .as_deref()
            .unwrap_or_else(|| file_name_or_path(&arguments.path))
            .to_string();
        let path = self.resolve_allowed_path(&arguments.path).await?;
        let output = TurnOutput::Media {
            kind: arguments.kind,
            path,
            caption_markdown: arguments.caption_markdown,
            file_name: arguments.file_name,
            mime_type: arguments.mime_type,
            disable_notification: arguments.disable_notification.unwrap_or(false),
        };

        let sender = self
            .turn_outputs
            .lock()
            .await
            .get(&envelope.turn_id)
            .cloned()
            .ok_or_else(|| {
                ClientError::InvalidMessage(format!(
                    "no active Telegram sink for turn {}",
                    envelope.turn_id
                ))
            })?;

        sender
            .send(output)
            .await
            .map_err(|_| ClientError::TransportClosed)?;

        Ok(server_requests::DynamicToolCallResponse {
            extra: serde_json::from_value(json!({
                "success": true,
                "contentItems": [{
                    "type": "inputText",
                    "text": format!(
                        "Queued Telegram {} upload for {}.",
                        media_kind_label(arguments.kind),
                        display_name
                    )
                }]
            }))?,
        })
    }

    async fn resolve_allowed_path(
        &self,
        raw_path: &str,
    ) -> std::result::Result<PathBuf, ClientError> {
        let candidate = PathBuf::from(raw_path);
        let canonical_candidate = match tokio::fs::canonicalize(&candidate).await {
            Ok(path) => path,
            Err(first_error) if candidate.is_absolute() => {
                return Err(ClientError::Io(first_error));
            }
            Err(_) => {
                let joined = self
                    .allowed_roots
                    .first()
                    .map(|root| root.join(&candidate))
                    .ok_or_else(|| {
                        ClientError::InvalidMessage("no allowed directories configured".to_string())
                    })?;
                tokio::fs::canonicalize(joined).await?
            }
        };

        for root in &self.allowed_roots {
            let Ok(canonical_root) = tokio::fs::canonicalize(root).await else {
                continue;
            };
            if canonical_candidate.starts_with(&canonical_root) {
                return Ok(canonical_candidate);
            }
        }

        Err(ClientError::InvalidMessage(format!(
            "path `{}` is outside allowed directories",
            canonical_candidate.display()
        )))
    }
}

fn append_temp_allow_roots(allowed_roots: &mut Vec<PathBuf>) {
    fn push_unique(roots: &mut Vec<PathBuf>, path: PathBuf) {
        if !roots.iter().any(|existing| existing == &path) {
            roots.push(path);
        }
    }

    for raw in ["/tmp", "/private/tmp", "/var/tmp", "/private/var/tmp"] {
        push_unique(allowed_roots, PathBuf::from(raw));
    }

    if let Some(tmpdir) = std::env::var_os("TMPDIR") {
        push_unique(allowed_roots, PathBuf::from(tmpdir));
    }

    push_unique(allowed_roots, std::env::temp_dir());
}

#[derive(Default)]
struct LiveTextState {
    pending_key: Option<String>,
    pending_item: Option<ThreadItem>,
    sent_keys: HashSet<String>,
}

struct ActiveChatTurn {
    thread_id: String,
    start_state: Mutex<TurnStartState>,
    start_notify: Notify,
    steer_lock: Mutex<()>,
    finished: AtomicBool,
}

#[derive(Clone, Debug)]
enum TurnStartState {
    Pending,
    Ready(String),
    Failed(String),
}

#[async_trait]
pub trait OutputSink: Send {
    async fn send(&mut self, output: TurnOutput) -> Result<()>;
}

#[async_trait]
pub trait ChatTurnRunner: Send + Sync {
    async fn reset_chat(&self, inbound: &InboundMessage) -> Result<()>;

    async fn run_chat_turn(
        &self,
        inbound: InboundMessage,
        attachment: Option<DownloadedAttachment>,
        sink: &mut dyn OutputSink,
    ) -> Result<()>;
}

#[async_trait]
trait LiveThreadHandle: Send {
    async fn read_minimal(&mut self) -> Result<()>;
    fn id_string(&self) -> Option<String>;
}

#[async_trait]
impl LiveThreadHandle for Thread {
    async fn read_minimal(&mut self) -> Result<()> {
        self.read(Some(false)).await?;
        Ok(())
    }

    fn id_string(&self) -> Option<String> {
        self.id().map(ToString::to_string)
    }
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
        let client =
            CodexClient::connect_ws(WsConfig::default().with_url(config.codex.connect_url.clone()))
                .await?;
        let codex = Codex::with_initialize_params(client, build_initialize_params(config));
        let tool_router = Arc::new(DynamicToolRouter::new(config));
        let tool_router_for_handler = tool_router.clone();
        codex
            .client()
            .set_dynamic_tool_call_handler(move |params| {
                let tool_router = tool_router_for_handler.clone();
                async move { tool_router.handle_call(params).await }
            })
            .await;
        Ok(Self {
            codex,
            _server: server,
            tool_router,
        })
    }
}

fn build_initialize_params(config: &AppConfig) -> requests::InitializeParams {
    let mut params = requests::InitializeParams::new(requests::ClientInfo::new(
        "codex_sdk_rs",
        "Codex Rust SDK",
        env!("CARGO_PKG_VERSION"),
    ));
    if config.codex.experimental_api {
        params.capabilities = Some(requests::InitializeCapabilities {
            experimental_api: Some(true),
            extra: Map::new(),
        });
    }
    params
}

impl ActiveChatTurn {
    fn new(thread_id: String) -> Self {
        Self {
            thread_id,
            start_state: Mutex::new(TurnStartState::Pending),
            start_notify: Notify::new(),
            steer_lock: Mutex::new(()),
            finished: AtomicBool::new(false),
        }
    }

    async fn publish_turn_start(&self, result: std::result::Result<String, String>) {
        let mut state = self.start_state.lock().await;
        *state = match result {
            Ok(turn_id) => TurnStartState::Ready(turn_id),
            Err(error) => TurnStartState::Failed(error),
        };
        drop(state);
        self.start_notify.notify_waiters();
    }

    async fn wait_for_turn_id(&self) -> Result<String> {
        loop {
            let state = self.start_state.lock().await.clone();
            match state {
                TurnStartState::Pending => self.start_notify.notified().await,
                TurnStartState::Ready(turn_id) => return Ok(turn_id),
                TurnStartState::Failed(error) => return Err(anyhow!(error)),
            }
        }
    }

    fn mark_finished(&self) {
        self.finished.store(true, Ordering::Release);
        self.start_notify.notify_waiters();
    }

    fn is_finished(&self) -> bool {
        self.finished.load(Ordering::Acquire)
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
            active_turns: Mutex::new(HashMap::new()),
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

    pub async fn stream_chat_turn(
        &self,
        inbound: InboundMessage,
        attachment: Option<DownloadedAttachment>,
        sink: &mut dyn OutputSink,
    ) -> Result<()> {
        let key = chat_session_key(&inbound);
        let input = self.build_input(&inbound, attachment);

        if self.try_steer_active_turn(&key, &input).await? {
            return Ok(());
        }

        let session = self.get_or_create_chat_thread(&key).await?;
        let mut thread = session.lock().await;
        if let Some(active) = self.current_active_turn(&key).await {
            drop(thread);
            self.steer_active_turn(active, input).await?;
            return Ok(());
        }

        let mut text_state = LiveTextState::default();

        let thread_id = prepare_live_thread(&mut *thread).await?;
        let active_turn = Arc::new(ActiveChatTurn::new(thread_id.clone()));
        self.active_turns
            .lock()
            .await
            .insert(key.clone(), active_turn.clone());
        drop(thread);

        self.persist_chat_thread_id(&key, &thread_id).await?;

        let turn_params = self.build_live_turn_start_params(&thread_id, input);
        let client = self.runtime.codex.client();
        let mut server_events = client.subscribe();
        let (tx, mut rx) = mpsc::channel(512);
        let (tool_output_tx, mut tool_output_rx) = mpsc::channel(32);

        let turn_start_tx = tx.clone();
        let active_for_start = active_turn.clone();
        tokio::spawn(async move {
            let result = client
                .turn_start(turn_params)
                .await
                .map(|response| response.turn.id);
            let start_state = match &result {
                Ok(turn_id) => Ok(turn_id.clone()),
                Err(error) => Err(error.to_string()),
            };
            active_for_start.publish_turn_start(start_state).await;
            let _ = turn_start_tx
                .send(LiveTurnEvent::TurnStartResult(result))
                .await;
        });

        let server_tx = tx.clone();
        tokio::spawn(async move {
            loop {
                let event = match server_events.recv().await {
                    Ok(event) => event,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        let _ = server_tx.send(LiveTurnEvent::TransportClosed).await;
                        break;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                };

                if server_tx.send(LiveTurnEvent::Server(event)).await.is_err() {
                    break;
                }
            }
        });
        drop(tx);

        let mut active_turn_id = None;
        let mut buffered_notifications = Vec::new();
        let stream_result: Result<()> = async {
            loop {
                let event = tokio::select! {
                    maybe_event = rx.recv() => match maybe_event {
                        Some(event) => event,
                        None => break,
                    },
                    maybe_output = tool_output_rx.recv() => match maybe_output {
                        Some(output) => LiveTurnEvent::ToolOutput(output),
                        None => continue,
                    },
                };
                match event {
                    LiveTurnEvent::TurnStartResult(result) => {
                        let turn_id = result?;
                        active_turn_id = Some(turn_id.clone());
                        self.runtime
                            .tool_router
                            .register_turn(&turn_id, tool_output_tx.clone())
                            .await;
                        let terminal = replay_buffered_notifications(
                            &buffered_notifications,
                            &thread_id,
                            &turn_id,
                            &mut text_state,
                            sink,
                        )
                        .await?;
                        buffered_notifications.clear();
                        if terminal {
                            break;
                        }
                    }
                    LiveTurnEvent::Server(ServerEvent::Notification(notification)) => {
                        if active_turn_id.is_none() {
                            match classify_notification(&notification, &thread_id) {
                                NotificationMatch::CurrentThreadBuffered => {
                                    buffered_notifications.push(notification);
                                }
                                NotificationMatch::Ignore => {}
                            }
                            continue;
                        }

                        let turn_id = active_turn_id.as_deref().expect("turn id just checked");
                        maybe_flush_on_successor_notification(&notification, &mut text_state, sink)
                            .await?;
                        if handle_live_notification(
                            notification,
                            &thread_id,
                            turn_id,
                            &mut text_state,
                            sink,
                        )
                        .await?
                        .is_terminal()
                        {
                            break;
                        }
                    }
                    LiveTurnEvent::ToolOutput(output) => {
                        flush_completed_text_item(&mut text_state, sink).await?;
                        sink.send(output).await?;
                    }
                    LiveTurnEvent::Server(ServerEvent::ServerRequest(_)) => {
                        flush_completed_text_item(&mut text_state, sink).await?;
                    }
                    LiveTurnEvent::Server(ServerEvent::TransportClosed)
                    | LiveTurnEvent::TransportClosed => {
                        return Err(anyhow!("Codex event stream closed"));
                    }
                }
            }
            Ok(())
        }
        .await;

        if let Some(turn_id) = active_turn_id.as_deref() {
            self.runtime.tool_router.unregister_turn(turn_id).await;
        }
        active_turn.mark_finished();
        self.remove_active_turn(&key, &active_turn).await;
        stream_result
    }

    pub async fn reset_chat_thread(&self, inbound: &InboundMessage) -> Result<()> {
        let key = chat_session_key(inbound);
        let session = Arc::new(Mutex::new(
            self.runtime.codex.start_thread(self.base_thread_options()),
        ));
        let mut thread = session.lock().await;
        let _ = prepare_live_thread(&mut *thread).await?;
        let thread_id = thread
            .id()
            .map(ToString::to_string)
            .ok_or_else(|| anyhow!("thread id unavailable after reset"))?;
        drop(thread);

        self.active_turns.lock().await.remove(&key);
        self.chats.lock().await.insert(key.clone(), session);

        let mut state = self.state.lock().await;
        state.chat_threads.insert(key, thread_id);
        state.save(&self.sessions_path)?;
        Ok(())
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

    async fn current_active_turn(&self, key: &str) -> Option<Arc<ActiveChatTurn>> {
        self.active_turns.lock().await.get(key).cloned()
    }

    async fn remove_active_turn(&self, key: &str, active_turn: &Arc<ActiveChatTurn>) {
        let mut active_turns = self.active_turns.lock().await;
        if active_turns
            .get(key)
            .is_some_and(|current| Arc::ptr_eq(current, active_turn))
        {
            active_turns.remove(key);
        }
    }

    async fn try_steer_active_turn(&self, key: &str, input: &Input) -> Result<bool> {
        loop {
            let Some(active) = self.current_active_turn(key).await else {
                return Ok(false);
            };
            if active.is_finished() {
                self.remove_active_turn(key, &active).await;
                continue;
            }
            self.steer_active_turn(active, input.clone()).await?;
            return Ok(true);
        }
    }

    async fn steer_active_turn(&self, active: Arc<ActiveChatTurn>, input: Input) -> Result<()> {
        let _guard = active.steer_lock.lock().await;
        if active.is_finished() {
            return Ok(());
        }
        let turn_id = active.wait_for_turn_id().await?;
        if active.is_finished() {
            return Ok(());
        }
        self.runtime
            .codex
            .client()
            .turn_steer(requests::TurnSteerParams {
                thread_id: active.thread_id.clone(),
                input: normalize_input_local(input),
                expected_turn_id: Some(turn_id),
                extra: Map::new(),
            })
            .await?;
        Ok(())
    }

    async fn persist_chat_thread_id(&self, key: &str, thread_id: &str) -> Result<()> {
        let mut state = self.state.lock().await;
        if state.chat_threads.get(key).map(String::as_str) != Some(thread_id) {
            state
                .chat_threads
                .insert(key.to_string(), thread_id.to_string());
            state.save(&self.sessions_path)?;
        }
        Ok(())
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
                transcript,
            }) => {
                let duration = duration_seconds
                    .map(|seconds| format!(" ({seconds}s)"))
                    .unwrap_or_default();
                let mut prompt = format!(
                    "Telegram user {} sent a voice message{}.\nLocal file path: `{}`.",
                    inbound.sender_name,
                    duration,
                    path.display()
                );
                if let Some(caption) = inbound.text.as_ref().filter(|text| !text.trim().is_empty())
                {
                    prompt.push_str("\nUser caption/context:\n");
                    prompt.push_str(caption.trim());
                }
                if let Some(transcript) = transcript.as_ref().filter(|text| !text.trim().is_empty())
                {
                    prompt.push_str("\n\nVoice transcript:\n");
                    prompt.push_str(transcript.trim());
                } else {
                    prompt.push_str("\n\nVoice transcript is unavailable.");
                }
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
            .dynamic_tools(vec![telegram_send_media_tool_spec()])
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

    fn build_live_turn_start_params(
        &self,
        thread_id: &str,
        input: Input,
    ) -> requests::TurnStartParams {
        let mut extra = Map::new();
        extra.insert(
            "skipGitRepoCheck".to_string(),
            Value::Bool(self.config.codex.skip_git_repo_check),
        );
        extra.insert(
            "webSearchMode".to_string(),
            Value::String("live".to_string()),
        );
        extra.insert(
            "webSearchEnabled".to_string(),
            Value::Bool(self.config.codex.web_search_enabled),
        );
        extra.insert(
            "networkAccessEnabled".to_string(),
            Value::Bool(self.config.codex.network_access_enabled),
        );
        extra.insert(
            "additionalDirectories".to_string(),
            Value::Array(
                self.additional_directories()
                    .into_iter()
                    .map(Value::String)
                    .collect(),
            ),
        );

        requests::TurnStartParams {
            thread_id: thread_id.to_string(),
            input: normalize_input_local(input),
            cwd: Some(self.config.codex.working_directory.display().to_string()),
            model: self.config.codex.model.clone(),
            model_provider: None,
            effort: None,
            summary: None,
            personality: None,
            output_schema: None,
            approval_policy: Some(
                normalized_approval_policy(&self.config.codex.approval_policy).to_string(),
            ),
            sandbox_policy: None,
            collaboration_mode: None,
            extra,
        }
    }
}

fn chat_session_key(inbound: &InboundMessage) -> String {
    match inbound.thread_id {
        Some(thread_id) => format!("{}:{thread_id}", inbound.chat_id),
        None => inbound.chat_id.to_string(),
    }
}

#[async_trait]
impl ChatTurnRunner for SessionManager {
    async fn reset_chat(&self, inbound: &InboundMessage) -> Result<()> {
        self.reset_chat_thread(inbound).await
    }

    async fn run_chat_turn(
        &self,
        inbound: InboundMessage,
        attachment: Option<DownloadedAttachment>,
        sink: &mut dyn OutputSink,
    ) -> Result<()> {
        self.stream_chat_turn(inbound, attachment, sink).await
    }
}

#[cfg(test)]
fn next_text_chunk(item: &ThreadItem, sent_text: &mut HashMap<String, String>) -> Option<String> {
    match item {
        ThreadItem::AgentMessage(message) => next_delta_chunk(
            format!("agent:{}", message.id),
            &message.text,
            should_forward_message(message),
            sent_text,
        ),
        ThreadItem::Plan(plan) => next_delta_chunk(
            format!("plan:{}", plan.id),
            &plan.text,
            !plan.text.trim().is_empty(),
            sent_text,
        ),
        _ => None,
    }
}

#[cfg(test)]
fn next_delta_chunk(
    key: String,
    text: &str,
    should_forward: bool,
    sent_text: &mut HashMap<String, String>,
) -> Option<String> {
    if !should_forward {
        return None;
    }

    let previous = sent_text.entry(key).or_default();
    if *previous == text {
        return None;
    }

    let chunk = if !previous.is_empty() && text.starts_with(previous.as_str()) {
        text[previous.len()..].to_string()
    } else {
        text.to_string()
    };
    *previous = text.to_string();

    if chunk.trim().is_empty() {
        None
    } else {
        Some(chunk)
    }
}

fn should_forward_message(message: &AgentMessageItem) -> bool {
    if message.text.trim().is_empty() {
        return false;
    }

    matches!(
        message.phase,
        Some(AgentMessagePhase::Commentary)
            | Some(AgentMessagePhase::FinalAnswer)
            | Some(AgentMessagePhase::Unknown)
            | None
    )
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

enum LiveTurnEvent {
    Server(ServerEvent),
    ToolOutput(TurnOutput),
    TurnStartResult(std::result::Result<String, codex_app_server_sdk::error::ClientError>),
    TransportClosed,
}

enum LiveNotificationOutcome {
    Continue,
    Terminal,
}

impl LiveNotificationOutcome {
    fn is_terminal(&self) -> bool {
        matches!(self, Self::Terminal)
    }
}

enum NotificationMatch {
    CurrentThreadBuffered,
    Ignore,
}

async fn prepare_live_thread(thread: &mut dyn LiveThreadHandle) -> Result<String> {
    thread.read_minimal().await?;
    thread
        .id_string()
        .ok_or_else(|| anyhow!("thread id unavailable after start/resume"))
}

fn normalize_input_local(input: Input) -> Vec<requests::TurnInputItem> {
    match input {
        Input::Text(text) => vec![requests::TurnInputItem::Text { text }],
        Input::Items(items) => {
            let mut text_parts = Vec::new();
            let mut normalized = Vec::new();

            for item in items {
                match item {
                    UserInput::Text { text } => text_parts.push(text),
                    UserInput::LocalImage { path } => {
                        normalized.push(requests::TurnInputItem::LocalImage { path });
                    }
                }
            }

            if !text_parts.is_empty() {
                normalized.insert(
                    0,
                    requests::TurnInputItem::Text {
                        text: text_parts.join("\n\n"),
                    },
                );
            }

            normalized
        }
    }
}

async fn replay_buffered_notifications(
    buffered_notifications: &[ServerNotification],
    thread_id: &str,
    turn_id: &str,
    text_state: &mut LiveTextState,
    sink: &mut dyn OutputSink,
) -> Result<bool> {
    let mut terminal = false;
    for notification in buffered_notifications.iter().cloned() {
        if handle_live_notification(notification, thread_id, turn_id, text_state, sink)
            .await?
            .is_terminal()
        {
            terminal = true;
        }
    }
    Ok(terminal)
}

async fn handle_live_notification(
    notification: ServerNotification,
    thread_id: &str,
    turn_id: &str,
    text_state: &mut LiveTextState,
    sink: &mut dyn OutputSink,
) -> Result<LiveNotificationOutcome> {
    match notification {
        ServerNotification::ItemStarted(_) => {}
        ServerNotification::ItemCompleted(payload)
            if live_matches_item(&payload, thread_id, turn_id) =>
        {
            if let Some(item) = parse_live_item(&payload) {
                log_live_completed_item(&item);
                if let Some(key) = live_text_item_key(&item) {
                    queue_completed_text_item(text_state, key, item, sink).await?;
                    flush_completed_text_item(text_state, sink).await?;
                } else {
                    flush_completed_text_item(text_state, sink).await?;
                }
            }
        }
        ServerNotification::ItemAgentMessageDelta(_) => {}
        ServerNotification::ItemPlanDelta(_) => {}
        ServerNotification::TurnStarted(payload) if payload.turn.id == turn_id => {}
        ServerNotification::TurnCompleted(payload) if payload.turn.id == turn_id => {
            if payload
                .turn
                .status
                .unwrap_or_default()
                .eq_ignore_ascii_case("failed")
            {
                let message = payload
                    .turn
                    .error
                    .map(|error| error.message)
                    .unwrap_or_else(|| "turn failed".to_string());
                return Err(anyhow!(message));
            }
            flush_completed_text_item(text_state, sink).await?;
            return Ok(LiveNotificationOutcome::Terminal);
        }
        ServerNotification::Error(payload)
            if live_matches_error(&payload.extra, thread_id, turn_id) =>
        {
            return Err(anyhow!(payload.error.message));
        }
        _ => {}
    }

    Ok(LiveNotificationOutcome::Continue)
}

async fn maybe_flush_on_successor_notification(
    notification: &ServerNotification,
    text_state: &mut LiveTextState,
    sink: &mut dyn OutputSink,
) -> Result<()> {
    let Some(pending_key) = text_state.pending_key.as_deref() else {
        return Ok(());
    };

    if notification_successor_key(notification)
        .as_deref()
        .is_some_and(|key| key == pending_key)
    {
        return Ok(());
    }

    flush_completed_text_item(text_state, sink).await
}

fn classify_notification(notification: &ServerNotification, thread_id: &str) -> NotificationMatch {
    match notification {
        ServerNotification::TurnStarted(payload) => {
            thread_buffer_match(&payload.turn.extra, thread_id)
        }
        ServerNotification::TurnCompleted(payload) => {
            thread_buffer_match(&payload.turn.extra, thread_id)
        }
        ServerNotification::ItemCompleted(payload) => {
            thread_buffer_match(&payload.extra, thread_id)
        }
        ServerNotification::Error(payload) => thread_buffer_match(&payload.extra, thread_id),
        _ => NotificationMatch::Ignore,
    }
}

fn thread_buffer_match(extra: &Map<String, Value>, thread_id: &str) -> NotificationMatch {
    if matches_thread_strict(extra, thread_id) {
        NotificationMatch::CurrentThreadBuffered
    } else {
        NotificationMatch::Ignore
    }
}

fn live_matches_item(payload: &ItemLifecycleNotification, thread_id: &str, turn_id: &str) -> bool {
    matches_target_from_extra_strict(&payload.extra, thread_id, turn_id)
}

fn live_matches_error(extra: &Map<String, Value>, thread_id: &str, turn_id: &str) -> bool {
    matches_target_from_extra_strict(extra, thread_id, turn_id)
}

fn matches_thread_strict(extra: &Map<String, Value>, thread_id: &str) -> bool {
    extra
        .get("threadId")
        .and_then(Value::as_str)
        .map(|incoming| incoming == thread_id)
        .unwrap_or(false)
}

fn matches_target_from_extra_strict(
    extra: &Map<String, Value>,
    thread_id: &str,
    turn_id: &str,
) -> bool {
    matches_thread_strict(extra, thread_id)
        && extra
            .get("turnId")
            .and_then(Value::as_str)
            .map(|incoming| incoming == turn_id)
            .unwrap_or(false)
}

fn normalized_approval_policy(value: &str) -> &'static str {
    match value {
        "on-request" => "on-request",
        "on-failure" => "on-failure",
        "untrusted" => "untrusted",
        _ => "never",
    }
}

fn telegram_send_media_tool_spec() -> DynamicToolSpec {
    DynamicToolSpec::new(
        "telegram_send_media",
        "Send a local file to the current Telegram chat as a photo, document, audio file, or voice note. Use this whenever the user should receive the actual file rather than only a text description. This is the default way to deliver screenshots, generated images, videos or screen recordings as files, audio outputs, and documents. If the artifact is a video and no dedicated video mode is available, send it as a document instead of skipping the upload.",
        json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["path", "kind"],
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Absolute or repository-relative path to an existing local file that should be delivered to the Telegram user."
                },
                "kind": {
                    "type": "string",
                    "enum": ["photo", "document", "audio", "voice"],
                    "description": "Use `photo` for images and screenshots, `document` for generic files including PDFs and videos when no dedicated video send mode exists, `audio` for audio files, and `voice` for voice-note style audio."
                },
                "caption_markdown": {
                    "type": "string",
                    "description": "Optional Markdown caption to send with the media. Keep it short and user-facing."
                },
                "file_name": {
                    "type": "string",
                    "description": "Optional Telegram-side file name override for the uploaded file."
                },
                "mime_type": {
                    "type": "string",
                    "description": "Optional MIME type override for multipart upload when the default detection is not suitable."
                },
                "disable_notification": {
                    "type": "boolean",
                    "description": "If true, send silently."
                }
            }
        }),
    )
}

fn parse_dynamic_tool_call_envelope(
    params: &server_requests::DynamicToolCallParams,
) -> std::result::Result<DynamicToolCallEnvelope, ClientError> {
    let tool = params
        .extra
        .get("tool")
        .and_then(Value::as_str)
        .ok_or_else(|| ClientError::InvalidMessage("dynamic tool call missing `tool`".to_string()))?
        .to_string();
    let arguments = params
        .extra
        .get("arguments")
        .cloned()
        .unwrap_or(Value::Null);
    let scope = params
        .extra
        .get("extra")
        .and_then(Value::as_object)
        .unwrap_or(&params.extra);
    let _thread_id = scope
        .get("threadId")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            ClientError::InvalidMessage("dynamic tool call missing `threadId`".to_string())
        })?;
    let turn_id = scope
        .get("turnId")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            ClientError::InvalidMessage("dynamic tool call missing `turnId`".to_string())
        })?
        .to_string();

    Ok(DynamicToolCallEnvelope {
        tool,
        turn_id,
        arguments,
    })
}

fn media_kind_label(kind: TelegramMediaKind) -> &'static str {
    match kind {
        TelegramMediaKind::Photo => "photo",
        TelegramMediaKind::Document => "document",
        TelegramMediaKind::Audio => "audio",
        TelegramMediaKind::Voice => "voice note",
    }
}

fn file_name_or_path(path: &str) -> &str {
    Path::new(path)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(path)
}

fn notification_successor_key(notification: &ServerNotification) -> Option<String> {
    match notification {
        ServerNotification::ItemStarted(payload) | ServerNotification::ItemCompleted(payload) => {
            live_text_item_key_from_value(&payload.item)
        }
        ServerNotification::ItemCommandExecutionOutputDelta(payload)
        | ServerNotification::ItemCommandExecutionTerminalInteraction(payload)
        | ServerNotification::ItemFileChangeOutputDelta(payload)
        | ServerNotification::ItemMcpToolCallProgress(payload)
        | ServerNotification::ItemReasoningSummaryTextDelta(payload)
        | ServerNotification::ItemReasoningSummaryPartAdded(payload)
        | ServerNotification::ItemReasoningTextDelta(payload) => payload
            .item_id
            .as_ref()
            .map(|item_id| format!("agent:{item_id}")),
        ServerNotification::ItemAgentMessageDelta(_)
        | ServerNotification::ItemPlanDelta(_)
        | ServerNotification::TurnStarted(_)
        | ServerNotification::TurnCompleted(_)
        | ServerNotification::TurnDiffUpdated(_)
        | ServerNotification::TurnPlanUpdated(_)
        | ServerNotification::Error(_)
        | ServerNotification::ThreadTokenUsageUpdated(_)
        | ServerNotification::RawResponseItemCompleted(_)
        | ServerNotification::ServerRequestResolved(_)
        | ServerNotification::Unknown { .. } => Some("__different-successor__".to_string()),
        _ => None,
    }
}

fn live_text_item_key_from_value(item: &Value) -> Option<String> {
    let object = item.as_object()?;
    let item_id = object.get("id").and_then(Value::as_str)?;
    let item_type = object.get("type").and_then(Value::as_str)?;
    match item_type {
        "agentMessage" => Some(format!("agent:{item_id}")),
        _ => None,
    }
}

fn live_text_item_key(item: &ThreadItem) -> Option<String> {
    match item {
        ThreadItem::AgentMessage(message) if should_forward_message(message) => {
            Some(format!("agent:{}", message.id))
        }
        _ => None,
    }
}

fn completed_text_from_item(item: &ThreadItem) -> Option<String> {
    match item {
        ThreadItem::AgentMessage(message) if should_forward_message(message) => {
            Some(message.text.clone())
        }
        _ => None,
    }
}

fn log_live_completed_item(item: &ThreadItem) {
    match item {
        ThreadItem::AgentMessage(message) => {
            debug!(
                item_id = %message.id,
                phase = ?message.phase,
                text_len = message.text.chars().count(),
                text = %message.text,
                "live item/completed agent message"
            );
        }
        ThreadItem::Plan(plan) => {
            debug!(
                item_id = %plan.id,
                text_len = plan.text.chars().count(),
                text = %plan.text,
                "live item/completed plan"
            );
        }
        _ => {}
    }
}

async fn queue_completed_text_item(
    state: &mut LiveTextState,
    key: String,
    item: ThreadItem,
    sink: &mut dyn OutputSink,
) -> Result<()> {
    if state.sent_keys.contains(&key) {
        return Ok(());
    }

    if state.pending_key.as_deref() == Some(key.as_str()) {
        state.pending_item = Some(item);
        return Ok(());
    }

    flush_completed_text_item(state, sink).await?;
    state.pending_key = Some(key);
    state.pending_item = Some(item);
    Ok(())
}

async fn flush_completed_text_item(
    state: &mut LiveTextState,
    sink: &mut dyn OutputSink,
) -> Result<()> {
    let Some(key) = state.pending_key.take() else {
        return Ok(());
    };
    let Some(item) = state.pending_item.take() else {
        return Ok(());
    };
    let Some(text) = completed_text_from_item(&item) else {
        return Ok(());
    };
    if state.sent_keys.insert(key) {
        sink.send(TurnOutput::Markdown {
            disable_notification: matches_agent_commentary(&item),
            text,
        })
        .await?;
    }
    Ok(())
}

fn matches_agent_commentary(item: &ThreadItem) -> bool {
    matches!(
        item,
        ThreadItem::AgentMessage(AgentMessageItem {
            phase: Some(AgentMessagePhase::Commentary),
            ..
        })
    )
}

fn parse_live_item(payload: &ItemLifecycleNotification) -> Option<ThreadItem> {
    let object = payload.item.as_object()?;
    let id = object.get("id").and_then(Value::as_str).unwrap_or_default();
    match object.get("type").and_then(Value::as_str) {
        Some("agentMessage") => Some(ThreadItem::AgentMessage(AgentMessageItem {
            id: id.to_string(),
            text: object
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            phase: object
                .get("phase")
                .and_then(Value::as_str)
                .map(parse_agent_message_phase_local),
        })),
        Some("plan") => Some(ThreadItem::Plan(codex_app_server_sdk::api::PlanItem {
            id: id.to_string(),
            text: object
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        })),
        _ => None,
    }
}

fn parse_agent_message_phase_local(value: &str) -> AgentMessagePhase {
    match value {
        "commentary" => AgentMessagePhase::Commentary,
        "final_answer" => AgentMessagePhase::FinalAnswer,
        _ => AgentMessagePhase::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{
        AppConfig, AppPaths, CodexConfig, ServerConfig, TelegramConfig, VoiceConfig,
    };
    use codex_app_server_sdk::protocol::notifications::DeltaNotification;
    use serde_json::json;

    fn message(id: &str, text: &str, phase: Option<AgentMessagePhase>) -> ThreadItem {
        ThreadItem::AgentMessage(AgentMessageItem {
            id: id.to_string(),
            text: text.to_string(),
            phase,
        })
    }

    #[test]
    fn streams_only_new_forwardable_agent_text() {
        let mut sent = HashMap::new();

        assert_eq!(
            next_text_chunk(
                &message(
                    "msg-1",
                    "Looking up flights",
                    Some(AgentMessagePhase::Commentary)
                ),
                &mut sent
            )
            .as_deref(),
            Some("Looking up flights")
        );
        assert_eq!(
            next_text_chunk(
                &message(
                    "msg-1",
                    "Looking up flights...\nOpening browser",
                    Some(AgentMessagePhase::Commentary)
                ),
                &mut sent
            )
            .as_deref(),
            Some("...\nOpening browser")
        );
        assert_eq!(
            next_text_chunk(
                &message(
                    "msg-1",
                    "Looking up flights...\nOpening browser",
                    Some(AgentMessagePhase::Commentary)
                ),
                &mut sent
            ),
            None
        );
    }

    #[test]
    fn falls_back_to_full_text_after_non_prefix_update() {
        let mut sent = HashMap::new();

        let _ = next_text_chunk(
            &message("msg-1", "Draft answer", Some(AgentMessagePhase::Commentary)),
            &mut sent,
        );

        assert_eq!(
            next_text_chunk(
                &message(
                    "msg-1",
                    "Rewritten answer",
                    Some(AgentMessagePhase::Commentary)
                ),
                &mut sent
            )
            .as_deref(),
            Some("Rewritten answer")
        );
    }

    #[test]
    fn ignores_non_forwardable_messages() {
        let mut sent = HashMap::new();

        assert_eq!(
            next_text_chunk(
                &message("msg-1", "", Some(AgentMessagePhase::Commentary)),
                &mut sent
            ),
            None
        );
        assert_eq!(
            next_text_chunk(
                &message("msg-1", "hidden", Some(AgentMessagePhase::Commentary)),
                &mut sent
            ),
            Some("hidden".to_string())
        );
        assert_eq!(
            next_text_chunk(
                &message("msg-2", "hidden", Some(AgentMessagePhase::Unknown)),
                &mut sent
            ),
            Some("hidden".to_string())
        );
        assert_eq!(
            next_text_chunk(&message("msg-3", "hidden", None), &mut sent),
            Some("hidden".to_string())
        );
    }

    #[test]
    fn ignores_empty_messages() {
        let mut sent = HashMap::new();

        assert_eq!(
            next_text_chunk(
                &message("msg-2", "", Some(AgentMessagePhase::FinalAnswer)),
                &mut sent
            ),
            None
        );
    }

    #[test]
    fn streams_plan_deltas() {
        let mut sent = HashMap::new();
        let first = ThreadItem::Plan(codex_app_server_sdk::api::PlanItem {
            id: "plan-1".to_string(),
            text: "1. Gather context".to_string(),
        });
        let second = ThreadItem::Plan(codex_app_server_sdk::api::PlanItem {
            id: "plan-1".to_string(),
            text: "1. Gather context\n2. Patch telegram bridge".to_string(),
        });

        assert_eq!(
            next_text_chunk(&first, &mut sent).as_deref(),
            Some("1. Gather context")
        );
        assert_eq!(
            next_text_chunk(&second, &mut sent).as_deref(),
            Some("\n2. Patch telegram bridge")
        );
    }

    #[derive(Default)]
    struct RecordingSink {
        outputs: Vec<TurnOutput>,
    }

    #[async_trait]
    impl OutputSink for RecordingSink {
        async fn send(&mut self, output: TurnOutput) -> Result<()> {
            self.outputs.push(output);
            Ok(())
        }
    }

    fn live_extra(thread_id: &str, turn_id: &str) -> Map<String, Value> {
        let mut extra = Map::new();
        extra.insert("threadId".to_string(), Value::String(thread_id.to_string()));
        extra.insert("turnId".to_string(), Value::String(turn_id.to_string()));
        extra
    }

    fn agent_message_delta(
        thread_id: &str,
        turn_id: &str,
        item_id: &str,
        delta: &str,
    ) -> DeltaNotification {
        DeltaNotification {
            item_id: Some(item_id.to_string()),
            delta: Some(delta.to_string()),
            text: None,
            summary_index: None,
            extra: live_extra(thread_id, turn_id),
        }
    }

    fn completed_agent_message(
        thread_id: &str,
        turn_id: &str,
        item_id: &str,
        phase: &str,
        text: &str,
    ) -> ItemLifecycleNotification {
        ItemLifecycleNotification {
            item: json!({
                "id": item_id,
                "type": "agentMessage",
                "phase": phase,
                "text": text,
            }),
            extra: live_extra(thread_id, turn_id),
        }
    }

    fn completed_plan(
        thread_id: &str,
        turn_id: &str,
        item_id: &str,
        text: &str,
    ) -> ItemLifecycleNotification {
        ItemLifecycleNotification {
            item: json!({
                "id": item_id,
                "type": "plan",
                "text": text,
            }),
            extra: live_extra(thread_id, turn_id),
        }
    }

    fn started_dynamic_tool_call(
        thread_id: &str,
        turn_id: &str,
        item_id: &str,
        tool: &str,
    ) -> ItemLifecycleNotification {
        ItemLifecycleNotification {
            item: json!({
                "id": item_id,
                "type": "dynamicToolCall",
                "tool": tool,
                "status": "in_progress",
            }),
            extra: live_extra(thread_id, turn_id),
        }
    }

    fn test_app_config(working_directory: &Path) -> AppConfig {
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
                api_base_url: "https://api.telegram.org".to_string(),
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

    #[test]
    fn parses_telegram_send_media_dynamic_tool_params() {
        let params = server_requests::DynamicToolCallParams {
            extra: serde_json::from_value(json!({
                "tool": "telegram_send_media",
                "arguments": {
                    "path": "/tmp/report.txt",
                    "kind": "document",
                    "caption_markdown": "Attached report"
                },
                "extra": {
                    "threadId": "thread-1",
                    "turnId": "turn-1"
                }
            }))
            .expect("params"),
        };

        let envelope = parse_dynamic_tool_call_envelope(&params).expect("envelope");
        assert_eq!(envelope.tool, "telegram_send_media");
        assert_eq!(envelope.turn_id, "turn-1");

        let args: TelegramSendMediaArgs =
            serde_json::from_value(envelope.arguments).expect("arguments");
        assert_eq!(
            args,
            TelegramSendMediaArgs {
                path: "/tmp/report.txt".to_string(),
                kind: TelegramMediaKind::Document,
                caption_markdown: Some("Attached report".to_string()),
                file_name: None,
                mime_type: None,
                disable_notification: None,
            }
        );
    }

    #[test]
    fn initialize_params_enable_experimental_api_from_config() {
        let mut config = test_app_config(Path::new("/tmp"));
        config.codex.experimental_api = true;

        let params = build_initialize_params(&config);
        assert_eq!(
            params
                .capabilities
                .as_ref()
                .and_then(|capabilities| capabilities.experimental_api),
            Some(true)
        );
    }

    #[test]
    fn initialize_params_leave_experimental_api_disabled_by_default() {
        let config = test_app_config(Path::new("/tmp"));

        let params = build_initialize_params(&config);
        assert!(params.capabilities.is_none());
    }

    #[test]
    fn telegram_send_media_response_uses_input_text_content_items() {
        let response = server_requests::DynamicToolCallResponse {
            extra: serde_json::from_value(json!({
                "success": true,
                "contentItems": [{
                    "type": "inputText",
                    "text": "Queued Telegram photo upload for screenshot.png."
                }]
            }))
            .expect("response"),
        };

        assert_eq!(
            response.extra.get("contentItems"),
            Some(&json!([{
                "type": "inputText",
                "text": "Queued Telegram photo upload for screenshot.png."
            }]))
        );
    }

    #[test]
    fn dynamic_tool_router_allows_standard_temp_roots() {
        let config = test_app_config(Path::new("/tmp/workspace"));
        let router = DynamicToolRouter::new(&config);

        assert!(router.allowed_roots.contains(&PathBuf::from("/tmp")));
        assert!(
            router
                .allowed_roots
                .contains(&PathBuf::from("/private/tmp"))
        );
        assert!(router.allowed_roots.contains(&PathBuf::from("/var/tmp")));
        assert!(
            router
                .allowed_roots
                .contains(&PathBuf::from("/private/var/tmp"))
        );
        assert!(router.allowed_roots.contains(&std::env::temp_dir()));

        if let Some(tmpdir) = std::env::var_os("TMPDIR") {
            assert!(router.allowed_roots.contains(&PathBuf::from(tmpdir)));
        }
    }

    #[tokio::test]
    async fn forwards_completed_intermediate_and_final_messages_but_not_deltas_or_plans()
    -> Result<()> {
        let mut text_state = LiveTextState::default();
        let mut sink = RecordingSink::default();

        handle_live_notification(
            ServerNotification::ItemAgentMessageDelta(agent_message_delta(
                "thread-1",
                "turn-1",
                "msg-1",
                "Проверяю состояние",
            )),
            "thread-1",
            "turn-1",
            &mut text_state,
            &mut sink,
        )
        .await?;

        handle_live_notification(
            ServerNotification::ItemPlanDelta(agent_message_delta(
                "thread-1",
                "turn-1",
                "plan-1",
                "1. Найти файл",
            )),
            "thread-1",
            "turn-1",
            &mut text_state,
            &mut sink,
        )
        .await?;

        assert!(
            sink.outputs.is_empty(),
            "plan deltas must stay internal too"
        );

        handle_live_notification(
            ServerNotification::ItemCompleted(completed_agent_message(
                "thread-1",
                "turn-1",
                "msg-1",
                "commentary",
                "Отправляю STEP_ONE",
            )),
            "thread-1",
            "turn-1",
            &mut text_state,
            &mut sink,
        )
        .await?;

        assert_eq!(sink.outputs.len(), 1);
        assert!(matches!(
            sink.outputs.first(),
            Some(TurnOutput::Markdown {
                text,
                disable_notification: true,
            }) if text == "Отправляю STEP_ONE"
        ));

        handle_live_notification(
            ServerNotification::ItemCompleted(completed_agent_message(
                "thread-1",
                "turn-1",
                "msg-2",
                "commentary",
                "Отправляю STEP_TWO",
            )),
            "thread-1",
            "turn-1",
            &mut text_state,
            &mut sink,
        )
        .await?;

        assert_eq!(sink.outputs.len(), 2);
        assert!(matches!(
            sink.outputs.get(1),
            Some(TurnOutput::Markdown {
                text,
                disable_notification: true,
            }) if text == "Отправляю STEP_TWO"
        ));

        handle_live_notification(
            ServerNotification::ItemCompleted(completed_plan(
                "thread-1",
                "turn-1",
                "plan-1",
                "1. Найти файл",
            )),
            "thread-1",
            "turn-1",
            &mut text_state,
            &mut sink,
        )
        .await?;

        assert_eq!(
            sink.outputs.len(),
            2,
            "non-agent completions must not emit extra Telegram messages"
        );

        handle_live_notification(
            ServerNotification::ItemCompleted(completed_agent_message(
                "thread-1",
                "turn-1",
                "msg-3",
                "final_answer",
                "Готовый финальный ответ",
            )),
            "thread-1",
            "turn-1",
            &mut text_state,
            &mut sink,
        )
        .await?;

        assert_eq!(sink.outputs.len(), 3);
        assert!(matches!(
            sink.outputs.get(2),
            Some(TurnOutput::Markdown {
                text,
                disable_notification: false,
            }) if text == "Готовый финальный ответ"
        ));

        Ok(())
    }

    #[tokio::test]
    async fn flushes_completed_commentary_on_next_tool_lifecycle_event() -> Result<()> {
        let mut text_state = LiveTextState::default();
        let mut sink = RecordingSink::default();

        handle_live_notification(
            ServerNotification::ItemCompleted(completed_agent_message(
                "thread-1",
                "turn-1",
                "msg-1",
                "commentary",
                "Checking the external source now",
            )),
            "thread-1",
            "turn-1",
            &mut text_state,
            &mut sink,
        )
        .await?;

        assert_eq!(sink.outputs.len(), 1);
        assert!(matches!(
            sink.outputs.first(),
            Some(TurnOutput::Markdown {
                text,
                disable_notification: true,
            }) if text == "Checking the external source now"
        ));

        maybe_flush_on_successor_notification(
            &ServerNotification::ItemStarted(started_dynamic_tool_call(
                "thread-1", "turn-1", "tool-1", "time",
            )),
            &mut text_state,
            &mut sink,
        )
        .await?;

        assert_eq!(
            sink.outputs.len(),
            1,
            "successor notifications should not duplicate already flushed commentary"
        );

        Ok(())
    }

    struct FakeLiveThread {
        reads: usize,
        read_result: Result<()>,
        id_after_read: Option<String>,
    }

    #[async_trait]
    impl LiveThreadHandle for FakeLiveThread {
        async fn read_minimal(&mut self) -> Result<()> {
            self.reads += 1;
            match &self.read_result {
                Ok(()) => Ok(()),
                Err(error) => Err(anyhow!(error.to_string())),
            }
        }

        fn id_string(&self) -> Option<String> {
            self.id_after_read.clone()
        }
    }

    #[tokio::test]
    async fn prepare_live_thread_forces_read_before_using_thread_id() -> Result<()> {
        let mut thread = FakeLiveThread {
            reads: 0,
            read_result: Ok(()),
            id_after_read: Some("thread-123".to_string()),
        };

        let thread_id = prepare_live_thread(&mut thread).await?;

        assert_eq!(thread.reads, 1);
        assert_eq!(thread_id, "thread-123");
        Ok(())
    }

    #[tokio::test]
    async fn prepare_live_thread_propagates_connection_not_ready_failure() {
        let mut thread = FakeLiveThread {
            reads: 0,
            read_result: Err(anyhow!(
                "connection not ready: call initialized() before invoking turn/start"
            )),
            id_after_read: None,
        };

        let error = prepare_live_thread(&mut thread)
            .await
            .expect_err("expected read failure");

        assert_eq!(thread.reads, 1);
        assert!(
            error
                .to_string()
                .contains("connection not ready: call initialized() before invoking turn/start")
        );
    }

    #[tokio::test]
    async fn active_turn_waits_until_turn_id_is_published() -> Result<()> {
        let active = Arc::new(ActiveChatTurn::new("thread-1".to_string()));
        let waiting = active.clone();
        let waiter = tokio::spawn(async move { waiting.wait_for_turn_id().await });

        tokio::task::yield_now().await;
        active.publish_turn_start(Ok("turn-1".to_string())).await;

        assert_eq!(waiter.await??, "turn-1");
        Ok(())
    }

    #[tokio::test]
    async fn active_turn_propagates_start_failure_to_steer_waiters() {
        let active = Arc::new(ActiveChatTurn::new("thread-1".to_string()));
        let waiting = active.clone();
        let waiter = tokio::spawn(async move { waiting.wait_for_turn_id().await });

        tokio::task::yield_now().await;
        active
            .publish_turn_start(Err("turn start failed".to_string()))
            .await;

        let error = waiter
            .await
            .expect("wait task must join")
            .expect_err("waiter must see start failure");
        assert!(error.to_string().contains("turn start failed"));
    }

    #[test]
    fn strict_target_matching_requires_both_thread_and_turn_ids() {
        let mut extra = Map::new();
        extra.insert(
            "threadId".to_string(),
            Value::String("thread-1".to_string()),
        );
        extra.insert("turnId".to_string(), Value::String("turn-1".to_string()));
        assert!(matches_target_from_extra_strict(
            &extra, "thread-1", "turn-1"
        ));
        assert!(!matches_target_from_extra_strict(
            &extra, "thread-2", "turn-1"
        ));
        assert!(!matches_target_from_extra_strict(
            &extra, "thread-1", "turn-2"
        ));

        let mut missing_turn = Map::new();
        missing_turn.insert(
            "threadId".to_string(),
            Value::String("thread-1".to_string()),
        );
        assert!(!matches_target_from_extra_strict(
            &missing_turn,
            "thread-1",
            "turn-1"
        ));

        let mut missing_thread = Map::new();
        missing_thread.insert("turnId".to_string(), Value::String("turn-1".to_string()));
        assert!(!matches_target_from_extra_strict(
            &missing_thread,
            "thread-1",
            "turn-1"
        ));
    }

    #[test]
    fn chat_session_key_includes_telegram_thread_context() {
        let inbound = InboundMessage {
            chat_id: 42,
            sender_id: 7,
            sender_name: "User".to_string(),
            message_id: 10,
            thread_id: Some(9001),
            text: Some("hello".to_string()),
            image_file_id: None,
            document_file_id: None,
            document_name: None,
            voice_file_id: None,
            voice_duration: None,
        };
        assert_eq!(chat_session_key(&inbound), "42:9001");

        let mut root_inbound = inbound;
        root_inbound.thread_id = None;
        assert_eq!(chat_session_key(&root_inbound), "42");
    }
}
