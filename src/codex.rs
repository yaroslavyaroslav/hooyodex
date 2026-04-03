use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
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
use tokio::task::JoinHandle;
use tokio::time::{Instant, MissedTickBehavior, interval_at};
use tracing::{debug, info, warn};

use crate::approval::{ApprovalManager, ApprovalOutcome, OperatorTarget};
use crate::channels::{ChannelKind, EventRoute, MediaKind, TurnOutput};
use crate::config::AppConfig;
use crate::state::PersistentState;
use crate::telegram::{DownloadedAttachment, InboundMessage};

pub struct CodexRuntime {
    pub codex: Codex,
    pub _server: WsServerHandle,
    tool_router: Arc<DynamicToolRouter>,
}

const PROACTIVE_AUTH_REFRESH_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);

pub struct SessionManager {
    runtime: tokio::sync::RwLock<Arc<CodexRuntime>>,
    approvals: Arc<ApprovalManager>,
    config: RwLock<AppConfig>,
    sessions_path: PathBuf,
    state: Mutex<PersistentState>,
    chats: Mutex<HashMap<String, Arc<Mutex<Thread>>>>,
    active_turns: Mutex<HashMap<String, Arc<ActiveChatTurn>>>,
    orchestrator: Mutex<Option<Arc<Mutex<Thread>>>>,
    auth_refresh_task: Mutex<Option<JoinHandle<()>>>,
    reconnect_lock: Mutex<()>,
}

#[derive(Debug, Clone)]
pub struct ChatRequest {
    pub session_key: String,
    pub sender_name: String,
    pub text: Option<String>,
    pub reply_to_text: Option<String>,
    pub quoted_text: Option<String>,
    pub operator_target: OperatorTarget,
}

impl ChatRequest {
    pub fn from_telegram(inbound: InboundMessage, operator_target: OperatorTarget) -> Self {
        let session_key = EventRoute {
            channel: ChannelKind::Telegram,
            account_id: "default".to_string(),
            conversation_id: inbound.chat_id.to_string(),
            thread_id: inbound.thread_id.map(|value| value.to_string()),
        }
        .session_key();
        Self {
            session_key,
            sender_name: inbound.sender_name,
            text: inbound.text,
            reply_to_text: inbound.reply_to_text,
            quoted_text: inbound.quoted_text,
            operator_target,
        }
    }
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
struct TelegramSendMediaArgs {
    path: String,
    kind: MediaKind,
    #[serde(default)]
    caption_markdown: Option<String>,
    #[serde(default)]
    file_name: Option<String>,
    #[serde(default)]
    mime_type: Option<String>,
    #[serde(default)]
    disable_notification: Option<bool>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
struct TelegramRequestApprovalArgs {
    summary_markdown: String,
    proposed_reply: String,
    #[serde(default)]
    operator_chat_id: Option<i64>,
    #[serde(default)]
    operator_thread_id: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct SessionIndexEntry {
    id: String,
    thread_name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DynamicToolCallEnvelope {
    tool: String,
    turn_id: String,
    arguments: Value,
}

#[derive(Clone)]
struct ActiveTurnContext {
    output_sender: mpsc::Sender<TurnOutput>,
    operator_target: OperatorTarget,
}

struct DynamicToolRouter {
    working_directory: PathBuf,
    config: AppConfig,
    approvals: Arc<ApprovalManager>,
    turn_contexts: Mutex<HashMap<String, ActiveTurnContext>>,
}

impl DynamicToolRouter {
    fn new(config: &AppConfig, approvals: Arc<ApprovalManager>) -> Self {
        Self {
            working_directory: config.codex.working_directory.clone(),
            config: config.clone(),
            approvals,
            turn_contexts: Mutex::new(HashMap::new()),
        }
    }

    async fn register_turn(
        &self,
        turn_id: &str,
        sender: mpsc::Sender<TurnOutput>,
        operator_target: OperatorTarget,
    ) {
        self.turn_contexts.lock().await.insert(
            turn_id.to_string(),
            ActiveTurnContext {
                output_sender: sender,
                operator_target,
            },
        );
    }

    async fn unregister_turn(&self, turn_id: &str) {
        self.turn_contexts.lock().await.remove(turn_id);
    }

    async fn handle_call(
        &self,
        params: server_requests::DynamicToolCallParams,
    ) -> std::result::Result<server_requests::DynamicToolCallResponse, ClientError> {
        let envelope = parse_dynamic_tool_call_envelope(&params).map_err(|error| {
            warn!(error = %error, extra = ?params.extra, "dynamic tool call rejected");
            error
        })?;
        match envelope.tool.as_str() {
            "telegram_send_media" => self.handle_send_media(envelope).await,
            "telegram_request_approval" => self.handle_request_approval(envelope).await,
            _ => {
                let error = ClientError::InvalidMessage(format!(
                    "unsupported dynamic tool `{}`",
                    envelope.tool
                ));
                warn!(
                    error = %error,
                    tool = %envelope.tool,
                    turn_id = %envelope.turn_id,
                    "dynamic tool call rejected"
                );
                Err(error)
            }
        }
    }

    async fn handle_send_media(
        &self,
        envelope: DynamicToolCallEnvelope,
    ) -> std::result::Result<server_requests::DynamicToolCallResponse, ClientError> {
        let arguments: TelegramSendMediaArgs = serde_json::from_value(envelope.arguments.clone())
            .map_err(|error| {
            warn!(
                error = %error,
                tool = %envelope.tool,
                turn_id = %envelope.turn_id,
                raw_arguments = ?envelope.arguments,
                "dynamic tool arguments failed to parse"
            );
            ClientError::from(error)
        })?;
        let display_name = arguments
            .file_name
            .as_deref()
            .unwrap_or_else(|| file_name_or_path(&arguments.path))
            .to_string();
        let path = self.resolve_path(&arguments.path).await.map_err(|error| {
            warn!(
                error = %error,
                tool = %envelope.tool,
                turn_id = %envelope.turn_id,
                path = %arguments.path,
                "dynamic tool path resolution failed"
            );
            error
        })?;
        let output = TurnOutput::Media {
            kind: arguments.kind,
            path,
            caption_markdown: arguments.caption_markdown,
            file_name: arguments.file_name,
            mime_type: arguments.mime_type,
            disable_notification: arguments.disable_notification.unwrap_or(false),
        };

        let sender = self
            .turn_contexts
            .lock()
            .await
            .get(&envelope.turn_id)
            .map(|context| context.output_sender.clone())
            .ok_or_else(|| {
                let error = ClientError::InvalidMessage(format!(
                    "no active Telegram sink for turn {}",
                    envelope.turn_id
                ));
                warn!(
                    error = %error,
                    tool = %envelope.tool,
                    turn_id = %envelope.turn_id,
                    "dynamic tool sink lookup failed"
                );
                error
            })?;

        sender.send(output).await.map_err(|_| {
            warn!(
                tool = %envelope.tool,
                turn_id = %envelope.turn_id,
                path = %arguments.path,
                "dynamic tool output channel closed"
            );
            ClientError::TransportClosed
        })?;

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

    async fn handle_request_approval(
        &self,
        envelope: DynamicToolCallEnvelope,
    ) -> std::result::Result<server_requests::DynamicToolCallResponse, ClientError> {
        let arguments: TelegramRequestApprovalArgs =
            serde_json::from_value(envelope.arguments.clone()).map_err(|error| {
                warn!(
                    error = %error,
                    tool = %envelope.tool,
                    turn_id = %envelope.turn_id,
                    raw_arguments = ?envelope.arguments,
                    "dynamic tool arguments failed to parse"
                );
                ClientError::from(error)
            })?;

        let default_target = self
            .turn_contexts
            .lock()
            .await
            .get(&envelope.turn_id)
            .map(|context| context.operator_target)
            .ok_or_else(|| {
                ClientError::InvalidMessage(format!(
                    "no operator target for turn {}",
                    envelope.turn_id
                ))
            })?;
        let target = OperatorTarget {
            chat_id: arguments.operator_chat_id.unwrap_or(default_target.chat_id),
            thread_id: arguments.operator_thread_id.or(default_target.thread_id),
        };

        let decision = self
            .approvals
            .request(
                &self.config,
                target,
                &arguments.summary_markdown,
                &arguments.proposed_reply,
            )
            .await
            .map_err(|error| ClientError::InvalidMessage(error.to_string()))?;

        let extra = match decision {
            ApprovalOutcome::Approved => json!({
                "success": true,
                "decision": "approve",
                "contentItems": [{
                    "type": "inputText",
                    "text": "Operator approved the proposed action."
                }]
            }),
            ApprovalOutcome::Denied => json!({
                "success": true,
                "decision": "deny",
                "contentItems": [{
                    "type": "inputText",
                    "text": "Operator denied the proposed action. Do not perform it unless they later change their mind."
                }]
            }),
            ApprovalOutcome::Steer(message) => json!({
                "success": true,
                "decision": "steer",
                "contentItems": [{
                    "type": "inputText",
                    "text": format!(
                        "Operator requested changes before approval. Do not send the previous draft. Revise it with these instructions:\n{}",
                        message
                    )
                }]
            }),
        };

        Ok(server_requests::DynamicToolCallResponse {
            extra: serde_json::from_value(extra)?,
        })
    }

    async fn resolve_path(&self, raw_path: &str) -> std::result::Result<PathBuf, ClientError> {
        let candidate = PathBuf::from(raw_path);
        Ok(match tokio::fs::canonicalize(&candidate).await {
            Ok(path) => path,
            Err(first_error) if candidate.is_absolute() => {
                return Err(ClientError::Io(first_error));
            }
            Err(_) => {
                let joined = self.working_directory.join(&candidate);
                tokio::fs::canonicalize(joined).await?
            }
        })
    }
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
    async fn reset_chat(&self, request: &ChatRequest) -> Result<()>;

    async fn run_chat_turn(
        &self,
        request: ChatRequest,
        attachment: Option<DownloadedAttachment>,
        sink: &mut dyn OutputSink,
    ) -> Result<()>;

    async fn ready(&self) -> Result<()> {
        Ok(())
    }

    async fn reload_config(&self, _config: AppConfig) -> Result<()> {
        Ok(())
    }
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
    pub async fn start(config: &AppConfig, approvals: Arc<ApprovalManager>) -> Result<Self> {
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
        let tool_router = Arc::new(DynamicToolRouter::new(config, approvals));
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

fn spawn_proactive_auth_refresh_task(codex: Codex) -> JoinHandle<()> {
    tokio::spawn(async move {
        match refresh_managed_chatgpt_auth(&codex).await {
            Ok(()) => {
                info!("completed startup proactive Codex auth refresh");
            }
            Err(error) => {
                warn!("startup proactive Codex auth refresh failed: {error}");
            }
        }

        let mut interval = interval_at(
            Instant::now() + PROACTIVE_AUTH_REFRESH_INTERVAL,
            PROACTIVE_AUTH_REFRESH_INTERVAL,
        );
        interval.set_missed_tick_behavior(MissedTickBehavior::Skip);

        info!(
            "scheduled proactive Codex auth refresh every {} hours",
            PROACTIVE_AUTH_REFRESH_INTERVAL.as_secs() / 3600
        );

        loop {
            interval.tick().await;
            match refresh_managed_chatgpt_auth(&codex).await {
                Ok(()) => {
                    info!("completed scheduled proactive Codex auth refresh");
                }
                Err(error) => {
                    warn!("scheduled proactive Codex auth refresh failed: {error}");
                }
            }
        }
    })
}

async fn refresh_managed_chatgpt_auth(codex: &Codex) -> Result<()> {
    codex
        .account_read(requests::GetAccountParams {
            refresh_token: Some(true),
            ..Default::default()
        })
        .await?;
    Ok(())
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
    fn current_config(&self) -> AppConfig {
        self.config
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    async fn replace_runtime(
        &self,
        new_runtime: Arc<CodexRuntime>,
        new_config: AppConfig,
    ) -> Result<()> {
        let new_auth_refresh_task = spawn_proactive_auth_refresh_task(new_runtime.codex.clone());

        {
            let mut runtime = self.runtime.write().await;
            *runtime = new_runtime;
        }

        {
            let mut config = self
                .config
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            *config = new_config;
        }

        if let Some(handle) = self
            .auth_refresh_task
            .lock()
            .await
            .replace(new_auth_refresh_task)
        {
            handle.abort();
        }

        let active_turns = {
            let mut active_turns = self.active_turns.lock().await;
            std::mem::take(&mut *active_turns)
        };
        for active in active_turns.into_values() {
            active.mark_finished();
        }

        self.chats.lock().await.clear();
        *self.orchestrator.lock().await = None;

        Ok(())
    }

    async fn reload_config_and_restart_runtime(&self, new_config: AppConfig) -> Result<()> {
        let _guard = self.reconnect_lock.lock().await;
        info!(
            working_directory = %new_config.codex.working_directory.display(),
            "reloading SessionManager config and restarting Codex runtime"
        );

        let new_runtime = Arc::new(CodexRuntime::start(&new_config, self.approvals.clone()).await?);
        self.replace_runtime(new_runtime, new_config).await
    }

    pub async fn new(config: AppConfig, approvals: Arc<ApprovalManager>) -> Result<Self> {
        let runtime = Arc::new(CodexRuntime::start(&config, approvals.clone()).await?);
        let auth_refresh_task = spawn_proactive_auth_refresh_task(runtime.codex.clone());
        let state = PersistentState::load(&config.sessions_path())?;
        Ok(Self {
            runtime: tokio::sync::RwLock::new(runtime),
            approvals,
            sessions_path: config.sessions_path(),
            config: RwLock::new(config),
            state: Mutex::new(state),
            chats: Mutex::new(HashMap::new()),
            active_turns: Mutex::new(HashMap::new()),
            orchestrator: Mutex::new(None),
            auth_refresh_task: Mutex::new(Some(auth_refresh_task)),
            reconnect_lock: Mutex::new(()),
        })
    }

    pub async fn ensure_orchestrator(&self) -> Result<()> {
        match self.ensure_orchestrator_once().await {
            Ok(()) => Ok(()),
            Err(error) if is_codex_transport_failure(&error) => {
                self.reconnect_runtime("ensure_orchestrator", &error)
                    .await?;
                self.ensure_orchestrator_once().await
            }
            Err(error) => Err(error),
        }
    }

    async fn ensure_orchestrator_once(&self) -> Result<()> {
        let mut orchestrator = self.orchestrator.lock().await;
        if orchestrator.is_some() {
            return Ok(());
        }

        let runtime = self.runtime().await;
        let thread_id = self.state.lock().await.orchestrator_thread_id.clone();
        let thread = if let Some(thread_id) = thread_id {
            runtime
                .codex
                .resume_thread_by_id(thread_id, self.base_thread_options())
        } else {
            runtime.codex.start_thread(self.base_thread_options())
        };

        let arc = Arc::new(Mutex::new(thread));
        if self.state.lock().await.orchestrator_thread_id.is_none() {
            let mut thread = arc.lock().await;
            let _ = thread
                .run(
                    "You are the persistent top-level orchestrator for this Hooyodex service. Maintain long-lived orchestration context. Only create automations or cron-style tasks when explicitly requested.",
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
        request: ChatRequest,
        attachment: Option<DownloadedAttachment>,
        sink: &mut dyn OutputSink,
    ) -> Result<()> {
        match self
            .stream_chat_turn_once(request.clone(), attachment.clone(), sink)
            .await
        {
            Ok(()) => Ok(()),
            Err(error) if is_codex_transport_failure(&error) => {
                self.reconnect_runtime("stream_chat_turn", &error).await?;
                self.stream_chat_turn_once(request, attachment, sink).await
            }
            Err(error) => Err(error),
        }
    }

    async fn stream_chat_turn_once(
        &self,
        request: ChatRequest,
        attachment: Option<DownloadedAttachment>,
        sink: &mut dyn OutputSink,
    ) -> Result<()> {
        let key = request.session_key.clone();
        let input = self.build_input(&request, attachment);

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

        let thread_id = self.ensure_chat_thread_identity(&mut thread, &key).await?;
        let active_turn = Arc::new(ActiveChatTurn::new(thread_id.clone()));
        self.active_turns
            .lock()
            .await
            .insert(key.clone(), active_turn.clone());
        drop(thread);

        let turn_params = self.build_live_turn_start_params(&thread_id, input);
        let runtime = self.runtime().await;
        let client = runtime.codex.client();
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
                        runtime
                            .tool_router
                            .register_turn(
                                &turn_id,
                                tool_output_tx.clone(),
                                request.operator_target,
                            )
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
            runtime.tool_router.unregister_turn(turn_id).await;
        }
        active_turn.mark_finished();
        self.remove_active_turn(&key, &active_turn).await;
        stream_result
    }

    pub async fn reset_chat_thread(&self, request: &ChatRequest) -> Result<()> {
        match self.reset_chat_thread_once(request).await {
            Ok(()) => Ok(()),
            Err(error) if is_codex_transport_failure(&error) => {
                self.reconnect_runtime("reset_chat_thread", &error).await?;
                self.reset_chat_thread_once(request).await
            }
            Err(error) => Err(error),
        }
    }

    async fn reset_chat_thread_once(&self, request: &ChatRequest) -> Result<()> {
        let key = request.session_key.clone();
        let runtime = self.runtime().await;
        let mut thread = runtime.codex.start_thread(self.base_thread_options());
        self.assign_chat_thread_name(&mut thread, &key).await?;
        let session = Arc::new(Mutex::new(thread));

        self.active_turns.lock().await.remove(&key);
        self.chats.lock().await.insert(key.clone(), session);
        Ok(())
    }

    async fn codex_ready(&self) -> Result<()> {
        match self.codex_ready_once().await {
            Ok(()) => Ok(()),
            Err(error) if is_codex_transport_failure(&error) => {
                self.reconnect_runtime("readyz", &error).await?;
                self.codex_ready_once().await
            }
            Err(error) => Err(error),
        }
    }

    async fn codex_ready_once(&self) -> Result<()> {
        let runtime = self.runtime().await;
        runtime
            .codex
            .account_read(requests::GetAccountParams {
                refresh_token: Some(false),
                ..Default::default()
            })
            .await?;
        Ok(())
    }

    async fn get_or_create_chat_thread(&self, key: &str) -> Result<Arc<Mutex<Thread>>> {
        if let Some(existing) = self.chats.lock().await.get(key).cloned() {
            return Ok(existing);
        }

        let runtime = self.runtime().await;
        let thread = if let Some(thread_id) = self.find_chat_thread_id_by_name(key).await? {
            runtime
                .codex
                .resume_thread_by_id(thread_id, self.base_thread_options())
        } else {
            let mut thread = runtime.codex.start_thread(self.base_thread_options());
            self.assign_chat_thread_name(&mut thread, key).await?;
            thread
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
        let runtime = self.runtime().await;
        runtime
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

    async fn find_chat_thread_id_by_name(&self, key: &str) -> Result<Option<String>> {
        let path = codex_session_index_path()?;
        let thread_name = chat_thread_name(key);
        if let Some(found) = find_thread_id_in_session_index(&path, &thread_name)? {
            return Ok(Some(found));
        }
        if let Some(legacy) = legacy_telegram_thread_name(key) {
            return find_thread_id_in_session_index(&path, &legacy);
        }
        Ok(None)
    }

    async fn assign_chat_thread_name(&self, thread: &mut Thread, key: &str) -> Result<String> {
        thread.set_name(chat_thread_name(key)).await?;
        thread
            .id()
            .map(ToString::to_string)
            .ok_or_else(|| anyhow!("thread id unavailable after naming"))
    }

    async fn ensure_chat_thread_identity(&self, thread: &mut Thread, key: &str) -> Result<String> {
        if thread.id().is_none() {
            return self.assign_chat_thread_name(thread, key).await;
        }
        prepare_live_thread(thread).await
    }

    fn build_input(
        &self,
        request: &ChatRequest,
        attachment: Option<DownloadedAttachment>,
    ) -> Input {
        let text_with_context = compose_message_context(request);
        match attachment {
            Some(DownloadedAttachment::Image(path)) => {
                let mut items = Vec::new();
                if let Some(text) = &text_with_context {
                    items.push(UserInput::Text { text: text.clone() });
                } else {
                    items.push(UserInput::Text {
                        text: format!(
                            "User {} attached an image. Reply to them in the same chat.",
                            request.sender_name
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
                    "User {} sent a voice message{}.\nLocal file path: `{}`.",
                    request.sender_name,
                    duration,
                    path.display()
                );
                if let Some(caption) = text_with_context
                    .as_ref()
                    .filter(|text| !text.trim().is_empty())
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
                    "User {} sent a file.\nLocal file path: `{}`.\nUse that file directly if needed.\n\n{}",
                    request.sender_name,
                    path.display(),
                    text_with_context.clone().unwrap_or_default()
                );
                Input::text(prompt)
            }
            None => Input::text(text_with_context.unwrap_or_default()),
        }
    }

    fn base_thread_options(&self) -> ThreadOptions {
        let config = self.current_config();
        let mut builder = ThreadOptions::builder()
            .working_directory(config.codex.working_directory.display().to_string())
            .skip_git_repo_check(config.codex.skip_git_repo_check)
            .network_access_enabled(config.codex.network_access_enabled)
            .web_search_enabled(config.codex.web_search_enabled)
            .web_search_mode(WebSearchMode::Live)
            .dynamic_tools(vec![
                telegram_send_media_tool_spec(),
                telegram_request_approval_tool_spec(),
            ])
            .additional_directories(self.additional_directories());

        if let Some(model) = &config.codex.model {
            builder = builder.model(model.clone());
        }

        builder = match config.codex.approval_policy.as_str() {
            "on-request" => builder.approval_policy(ApprovalMode::OnRequest),
            "on-failure" => builder.approval_policy(ApprovalMode::OnFailure),
            "untrusted" => builder.approval_policy(ApprovalMode::Untrusted),
            _ => builder.approval_policy(ApprovalMode::Never),
        };

        builder = match config.codex.sandbox_mode.as_str() {
            "read-only" => builder.sandbox_mode(SandboxMode::ReadOnly),
            "workspace-write" => builder.sandbox_mode(SandboxMode::WorkspaceWrite),
            _ => builder.sandbox_mode(SandboxMode::DangerFullAccess),
        };

        builder.build()
    }

    fn additional_directories(&self) -> Vec<String> {
        let config = self.current_config();
        let mut directories = Vec::new();
        directories.push(config.attachment_root().display().to_string());
        directories.extend(
            config
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
        let config = self.current_config();
        let mut extra = Map::new();
        extra.insert(
            "skipGitRepoCheck".to_string(),
            Value::Bool(config.codex.skip_git_repo_check),
        );
        extra.insert(
            "webSearchMode".to_string(),
            Value::String("live".to_string()),
        );
        extra.insert(
            "webSearchEnabled".to_string(),
            Value::Bool(config.codex.web_search_enabled),
        );
        extra.insert(
            "networkAccessEnabled".to_string(),
            Value::Bool(config.codex.network_access_enabled),
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
            cwd: Some(config.codex.working_directory.display().to_string()),
            model: config.codex.model.clone(),
            model_provider: None,
            effort: None,
            summary: None,
            personality: None,
            output_schema: None,
            approval_policy: Some(
                normalized_approval_policy(&config.codex.approval_policy).to_string(),
            ),
            sandbox_policy: None,
            collaboration_mode: None,
            extra,
        }
    }

    async fn runtime(&self) -> Arc<CodexRuntime> {
        self.runtime.read().await.clone()
    }

    async fn reconnect_runtime(&self, operation: &str, error: &anyhow::Error) -> Result<()> {
        let _guard = self.reconnect_lock.lock().await;
        warn!(
            operation,
            error = %error,
            "reinitializing Codex runtime after transport failure"
        );

        let config = self.current_config();
        let new_runtime = Arc::new(CodexRuntime::start(&config, self.approvals.clone()).await?);
        self.replace_runtime(new_runtime, config).await
    }
}

fn chat_thread_name(key: &str) -> String {
    format!("session:{key}")
}

fn compose_message_context(request: &ChatRequest) -> Option<String> {
    let user_text = request
        .text
        .as_deref()
        .map(str::trim)
        .filter(|text| !text.is_empty());
    let reply_to_text = request
        .reply_to_text
        .as_deref()
        .map(str::trim)
        .filter(|text| !text.is_empty());
    let quoted_text = request
        .quoted_text
        .as_deref()
        .map(str::trim)
        .filter(|text| !text.is_empty());

    if reply_to_text.is_none() && quoted_text.is_none() {
        return user_text.map(ToOwned::to_owned);
    }

    let mut parts = Vec::new();
    if let Some(reply_to_text) = reply_to_text {
        parts.push(format!("Quoted message:\n{reply_to_text}"));
    }
    if let Some(quoted_text) = quoted_text {
        parts.push(format!("Quoted fragment:\n{quoted_text}"));
    }
    if let Some(user_text) = user_text {
        parts.push(format!("User reply/comment:\n{user_text}"));
    }

    if parts.is_empty() {
        None
    } else {
        Some(parts.join("\n\n"))
    }
}

fn legacy_telegram_thread_name(key: &str) -> Option<String> {
    let remainder = key.strip_prefix("telegram:default:")?;
    Some(format!("telegram:{remainder}"))
}

#[cfg(test)]
fn chat_session_key(inbound: &InboundMessage) -> String {
    EventRoute {
        channel: ChannelKind::Telegram,
        account_id: "default".to_string(),
        conversation_id: inbound.chat_id.to_string(),
        thread_id: inbound.thread_id.map(|value| value.to_string()),
    }
    .session_key()
}

fn codex_session_index_path() -> Result<PathBuf> {
    if let Some(codex_home) = std::env::var_os("CODEX_HOME") {
        return Ok(PathBuf::from(codex_home).join("session_index.jsonl"));
    }

    let home = dirs::home_dir().ok_or_else(|| anyhow!("failed to resolve home directory"))?;
    Ok(home.join(".codex").join("session_index.jsonl"))
}

fn find_thread_id_in_session_index(path: &Path, thread_name: &str) -> Result<Option<String>> {
    if thread_name.trim().is_empty() || !path.exists() {
        return Ok(None);
    }

    let raw =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    let mut found = None;

    for line in raw.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(entry) = serde_json::from_str::<SessionIndexEntry>(trimmed) else {
            continue;
        };
        if entry.thread_name == thread_name {
            found = Some(entry.id);
        }
    }

    Ok(found)
}

fn is_codex_transport_failure(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        let message = cause.to_string();
        message.contains("Codex event stream closed")
            || message.contains("transport send failed")
            || message.contains("channel closed")
            || message.contains("request timed out")
            || message.contains("connection not ready")
    })
}

#[async_trait]
impl ChatTurnRunner for SessionManager {
    async fn reset_chat(&self, request: &ChatRequest) -> Result<()> {
        self.reset_chat_thread(request).await
    }

    async fn run_chat_turn(
        &self,
        request: ChatRequest,
        attachment: Option<DownloadedAttachment>,
        sink: &mut dyn OutputSink,
    ) -> Result<()> {
        self.stream_chat_turn(request, attachment, sink).await
    }

    async fn ready(&self) -> Result<()> {
        self.codex_ready().await
    }

    async fn reload_config(&self, config: AppConfig) -> Result<()> {
        self.reload_config_and_restart_runtime(config).await?;
        self.ensure_orchestrator().await
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
        if let Ok(mut auth_refresh_task) = self.auth_refresh_task.try_lock()
            && let Some(handle) = auth_refresh_task.take()
        {
            handle.abort();
        }
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

fn telegram_request_approval_tool_spec() -> DynamicToolSpec {
    DynamicToolSpec::new(
        "telegram_request_approval",
        "Ask the configured Telegram operator to approve, deny, or steer a proposed action. Use this when a draft reply or sensitive action needs human approval before continuing. This tool blocks until the operator chooses Approve, Deny, or Steer and sends steering text.",
        json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["summary_markdown", "proposed_reply"],
            "properties": {
                "summary_markdown": {
                    "type": "string",
                    "description": "Short Markdown summary explaining what needs approval."
                },
                "proposed_reply": {
                    "type": "string",
                    "description": "Proposed outbound message text to show the operator."
                },
                "operator_chat_id": {
                    "type": "integer",
                    "description": "Optional Telegram chat id override for where to send the approval request."
                },
                "operator_thread_id": {
                    "type": "integer",
                    "description": "Optional Telegram message_thread_id override for where to send the approval request."
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

fn media_kind_label(kind: MediaKind) -> &'static str {
    match kind {
        MediaKind::Photo => "photo",
        MediaKind::Document => "document",
        MediaKind::Audio => "audio",
        MediaKind::Voice => "voice note",
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
        AppConfig, AppPaths, CodexConfig, OperatorConfig, ServerConfig, TelegramConfig, VoiceConfig,
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
            whatsapp: None,
            operator: OperatorConfig::default(),
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
                kind: MediaKind::Document,
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

    #[tokio::test]
    async fn dynamic_tool_router_resolves_relative_paths_from_working_directory() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let docs_dir = tempdir.path().join("Documents");
        std::fs::create_dir_all(&docs_dir).expect("create docs dir");
        let file_path = docs_dir.join("example.pdf");
        std::fs::write(&file_path, b"pdf").expect("write file");

        let config = test_app_config(tempdir.path());
        let router = DynamicToolRouter::new(&config, ApprovalManager::new());

        let resolved = router
            .resolve_path("Documents/example.pdf")
            .await
            .expect("resolve path");
        assert_eq!(resolved, file_path.canonicalize().expect("canonical path"));
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
            reply_to_text: None,
            quoted_text: None,
            image_file_id: None,
            document_file_id: None,
            document_name: None,
            voice_file_id: None,
            voice_duration: None,
        };
        assert_eq!(chat_session_key(&inbound), "telegram:default:42:9001");

        let mut root_inbound = inbound;
        root_inbound.thread_id = None;
        assert_eq!(chat_session_key(&root_inbound), "telegram:default:42");
    }

    #[test]
    fn chat_thread_name_uses_generic_session_prefix() {
        assert_eq!(
            chat_thread_name("telegram:default:42"),
            "session:telegram:default:42"
        );
        assert_eq!(
            chat_thread_name("telegram:default:42:9001"),
            "session:telegram:default:42:9001"
        );
    }

    #[test]
    fn legacy_telegram_thread_name_supports_existing_sessions() {
        assert_eq!(
            legacy_telegram_thread_name("telegram:default:42:9001").as_deref(),
            Some("telegram:42:9001")
        );
        assert_eq!(
            legacy_telegram_thread_name("telegram:default:42").as_deref(),
            Some("telegram:42")
        );
    }

    #[test]
    fn session_index_lookup_uses_newest_matching_thread_name() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("session_index.jsonl");
        std::fs::write(
            &path,
            concat!(
                "{\"id\":\"thread-1\",\"thread_name\":\"session:telegram:default:42\",\"updated_at\":\"2026-03-27T10:00:00Z\"}\n",
                "{\"id\":\"thread-2\",\"thread_name\":\"session:telegram:default:9\",\"updated_at\":\"2026-03-27T10:01:00Z\"}\n",
                "{\"id\":\"thread-3\",\"thread_name\":\"session:telegram:default:42\",\"updated_at\":\"2026-03-27T10:02:00Z\"}\n",
                "{\"id\":\"ignored\",\"thread_name\":\"session:telegram:default:42:extra\",\"updated_at\":\"2026-03-27T10:03:00Z\"}\n",
            ),
        )
        .expect("write session index");

        let found =
            find_thread_id_in_session_index(&path, "session:telegram:default:42").expect("lookup");
        assert_eq!(found.as_deref(), Some("thread-3"));
    }

    #[test]
    fn compose_message_context_includes_reply_and_quote() {
        let request = ChatRequest {
            session_key: "telegram:default:42".to_string(),
            sender_name: "User".to_string(),
            text: Some("new reply".to_string()),
            reply_to_text: Some("full original".to_string()),
            quoted_text: Some("selected bit".to_string()),
            operator_target: OperatorTarget {
                chat_id: 1,
                thread_id: None,
            },
        };

        let context = compose_message_context(&request).expect("context");
        assert!(context.contains("Quoted message:\nfull original"));
        assert!(context.contains("Quoted fragment:\nselected bit"));
        assert!(context.contains("User reply/comment:\nnew reply"));
    }

    #[test]
    fn detects_codex_transport_failures_from_error_text() {
        assert!(is_codex_transport_failure(&anyhow!(
            "Codex event stream closed"
        )));
        assert!(is_codex_transport_failure(&anyhow!(
            "transport send failed: failed to send outbound frame: channel closed"
        )));
        assert!(!is_codex_transport_failure(&anyhow!(
            "{}",
            r#"rpc error RpcError { code: -32600, message: "no active turn to steer", data: None }"#
        )));
    }
}
