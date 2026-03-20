use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, anyhow};
use async_trait::async_trait;
use codex_app_server_sdk::api::{
    AgentMessageItem, AgentMessagePhase, ApprovalMode, Codex, Input, SandboxMode, Thread,
    ThreadItem, ThreadOptions, TurnOptions, UserInput, WebSearchMode,
};
use codex_app_server_sdk::events::{ServerEvent, ServerNotification};
use codex_app_server_sdk::protocol::notifications::{
    DeltaNotification, ItemLifecycleNotification,
};
use codex_app_server_sdk::protocol::requests;
use codex_app_server_sdk::{WsConfig, WsServerHandle, WsStartConfig};
use serde_json::{Map, Value};
use tokio::sync::{Mutex, mpsc};
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
pub enum TurnOutput {
    Markdown(String),
    Image(PathBuf),
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

    pub async fn stream_chat_turn(
        &self,
        inbound: InboundMessage,
        attachment: Option<DownloadedAttachment>,
        sink: &mut dyn OutputSink,
    ) -> Result<()> {
        let key = inbound.chat_id.to_string();
        let session = self.get_or_create_chat_thread(&key).await?;
        let input = self.build_input(&inbound, attachment);

        let mut sent_text = HashMap::new();
        let mut sent_images = HashSet::new();
        let mut delta_state = DeltaState::default();

        let mut thread = session.lock().await;
        let thread_id = prepare_live_thread(&mut *thread).await?;
        let turn_params = self.build_live_turn_start_params(&thread_id, input);
        let client = self.runtime.codex.client();
        let mut server_events = client.subscribe();
        let (tx, mut rx) = mpsc::channel(512);

        let turn_start_tx = tx.clone();
        tokio::spawn(async move {
            let result = client
                .turn_start(turn_params)
                .await
                .map(|response| response.turn.id);
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
        let mut turn_completed = false;
        loop {
            let recv = tokio::time::timeout(Duration::from_millis(250), rx.recv()).await;
            match recv {
                Ok(Some(event)) => match event {
                    LiveTurnEvent::TurnStartResult(result) => {
                        let turn_id = result?;
                        active_turn_id = Some(turn_id.clone());
                        if replay_buffered_notifications(
                            &buffered_notifications,
                            &thread_id,
                            &turn_id,
                            &mut delta_state,
                            &mut sent_text,
                            &mut sent_images,
                            sink,
                        )
                        .await?
                        {
                            turn_completed = true;
                        }
                        buffered_notifications.clear();
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
                        if handle_live_notification(
                            notification,
                            &thread_id,
                            turn_id,
                            &mut delta_state,
                            &mut sent_text,
                            &mut sent_images,
                            sink,
                        )
                        .await?
                        .is_terminal()
                        {
                            turn_completed = true;
                        }
                    }
                    LiveTurnEvent::Server(ServerEvent::TransportClosed)
                    | LiveTurnEvent::TransportClosed => {
                        return Err(anyhow!("Codex event stream closed"));
                    }
                    LiveTurnEvent::Server(ServerEvent::ServerRequest(_)) => {}
                },
                Ok(None) => break,
                Err(_) => {
                    flush_ready_delta_chunks(&mut delta_state, &mut sent_text, sink).await?;
                    if turn_completed {
                        flush_all_delta_chunks(&mut delta_state, &mut sent_text, sink).await?;
                        break;
                    }
                }
            }
        }

        if let Some(id) = thread.id().map(ToString::to_string) {
            let mut state = self.state.lock().await;
            if state.chat_threads.get(&key) != Some(&id) {
                state.chat_threads.insert(key, id);
                state.save(&self.sessions_path)?;
            }
        }

        Ok(())
    }

    pub async fn reset_chat_thread(&self, inbound: &InboundMessage) -> Result<()> {
        let key = inbound.chat_id.to_string();
        let session = Arc::new(Mutex::new(self.runtime.codex.start_thread(
            self.base_thread_options(),
        )));
        let mut thread = session.lock().await;
        let _ = prepare_live_thread(&mut *thread).await?;
        let thread_id = thread
            .id()
            .map(ToString::to_string)
            .ok_or_else(|| anyhow!("thread id unavailable after reset"))?;
        drop(thread);

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

fn next_image_path(item: &ThreadItem, sent_images: &mut HashSet<String>) -> Option<PathBuf> {
    let ThreadItem::ImageView(image) = item else {
        return None;
    };

    if image.path.trim().is_empty() || !sent_images.insert(image.id.clone()) {
        return None;
    }

    Some(PathBuf::from(&image.path))
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
    TurnStartResult(std::result::Result<String, codex_app_server_sdk::error::ClientError>),
    TransportClosed,
}

#[derive(Default)]
struct DeltaState {
    item_text: HashMap<String, String>,
    ready_items: HashSet<String>,
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
    delta_state: &mut DeltaState,
    sent_text: &mut HashMap<String, String>,
    sent_images: &mut HashSet<String>,
    sink: &mut dyn OutputSink,
) -> Result<bool> {
    let mut terminal = false;
    for notification in buffered_notifications.iter().cloned() {
        if handle_live_notification(
            notification,
            thread_id,
            turn_id,
            delta_state,
            sent_text,
            sent_images,
            sink,
        )
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
    delta_state: &mut DeltaState,
    sent_text: &mut HashMap<String, String>,
    sent_images: &mut HashSet<String>,
    sink: &mut dyn OutputSink,
) -> Result<LiveNotificationOutcome> {
    match notification {
        ServerNotification::ItemStarted(_) => {}
        ServerNotification::ItemCompleted(payload)
            if live_matches_item(&payload, thread_id, turn_id) =>
        {
            if let Some(item) = parse_live_item(&payload) {
                clear_delta_state_for_completed_item(delta_state, &item);
                if let Some(chunk) = next_text_chunk(&item, sent_text) {
                    sink.send(TurnOutput::Markdown(chunk)).await?;
                }
                if let Some(image_path) = next_image_path(&item, sent_images) {
                    sink.send(TurnOutput::Image(image_path)).await?;
                }
            }
        }
        ServerNotification::ItemAgentMessageDelta(delta)
            if live_matches_delta(&delta, thread_id, turn_id) =>
        {
            if let Some(item) = append_delta_item(delta_state, "agent", &delta, false)
                && let Some(chunk) = next_text_chunk(&item, sent_text)
            {
                sink.send(TurnOutput::Markdown(chunk)).await?;
            }
        }
        ServerNotification::ItemPlanDelta(delta)
            if live_matches_delta(&delta, thread_id, turn_id) =>
        {
            if let Some(item) = append_delta_item(delta_state, "plan", &delta, true)
                && let Some(chunk) = next_text_chunk(&item, sent_text)
            {
                sink.send(TurnOutput::Markdown(chunk)).await?;
            }
        }
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

fn classify_notification(notification: &ServerNotification, thread_id: &str) -> NotificationMatch {
    match notification {
        ServerNotification::TurnStarted(payload) => thread_buffer_match(&payload.turn.extra, thread_id),
        ServerNotification::TurnCompleted(payload) => thread_buffer_match(&payload.turn.extra, thread_id),
        ServerNotification::ItemCompleted(payload) => {
            thread_buffer_match(&payload.extra, thread_id)
        }
        ServerNotification::ItemAgentMessageDelta(delta)
        | ServerNotification::ItemPlanDelta(delta) => {
            thread_buffer_match(&delta.extra, thread_id)
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

fn live_matches_delta(payload: &DeltaNotification, thread_id: &str, turn_id: &str) -> bool {
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

fn append_delta_item(
    delta_state: &mut DeltaState,
    prefix: &str,
    payload: &DeltaNotification,
    flush_on_newline: bool,
) -> Option<ThreadItem> {
    let delta = payload
        .delta
        .as_deref()
        .or(payload.text.as_deref())
        .unwrap_or_default();
    if delta.is_empty() {
        return None;
    }

    let item_id = payload.item_id.clone().unwrap_or_default();
    let key = format!("{prefix}:{item_id}");
    let full_text = delta_state.item_text.entry(key.clone()).or_default();
    full_text.push_str(delta);

    let should_flush = flush_on_newline
        || delta.ends_with('\n')
        || delta.ends_with('.')
        || delta.ends_with('!')
        || delta.ends_with('?')
        || delta.starts_with(' ')
        || delta.len() >= 24;

    if !should_flush {
        return None;
    }

    delta_state.ready_items.insert(key);
    build_delta_item(prefix, item_id, full_text.clone())
}

async fn flush_ready_delta_chunks(
    delta_state: &mut DeltaState,
    sent_text: &mut HashMap<String, String>,
    sink: &mut dyn OutputSink,
) -> Result<()> {
    if delta_state.ready_items.is_empty() {
        return Ok(());
    }

    let ready_keys: Vec<String> = delta_state.ready_items.drain().collect();
    for key in ready_keys {
        let Some((prefix, item_id)) = key.split_once(':') else {
            continue;
        };
        let Some(full_text) = delta_state.item_text.get(&key).cloned() else {
            continue;
        };
        if let Some(item) = build_delta_item(prefix, item_id.to_string(), full_text)
            && let Some(chunk) = next_text_chunk(&item, sent_text)
        {
            sink.send(TurnOutput::Markdown(chunk)).await?;
        }
    }
    Ok(())
}

async fn flush_all_delta_chunks(
    delta_state: &mut DeltaState,
    sent_text: &mut HashMap<String, String>,
    sink: &mut dyn OutputSink,
) -> Result<()> {
    delta_state
        .ready_items
        .extend(delta_state.item_text.keys().cloned());
    flush_ready_delta_chunks(delta_state, sent_text, sink).await
}

fn clear_delta_state_for_completed_item(delta_state: &mut DeltaState, item: &ThreadItem) {
    let key = match item {
        ThreadItem::AgentMessage(message) => Some(format!("agent:{}", message.id)),
        ThreadItem::Plan(plan) => Some(format!("plan:{}", plan.id)),
        _ => None,
    };

    if let Some(key) = key {
        delta_state.item_text.remove(&key);
        delta_state.ready_items.remove(&key);
    }
}

fn build_delta_item(prefix: &str, item_id: String, text: String) -> Option<ThreadItem> {
    if text.trim().is_empty() {
        return None;
    }

    match prefix {
        "agent" => Some(ThreadItem::AgentMessage(AgentMessageItem {
            id: item_id,
            text,
            phase: None,
        })),
        "plan" => Some(ThreadItem::Plan(codex_app_server_sdk::api::PlanItem {
            id: item_id,
            text,
        })),
        _ => None,
    }
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
        Some("imageView") => Some(ThreadItem::ImageView(
            codex_app_server_sdk::api::ImageViewItem {
                id: id.to_string(),
                path: object
                    .get("path")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
            },
        )),
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

    fn message(id: &str, text: &str, phase: Option<AgentMessagePhase>) -> ThreadItem {
        ThreadItem::AgentMessage(AgentMessageItem {
            id: id.to_string(),
            text: text.to_string(),
            phase,
        })
    }

    #[test]
    fn streams_only_new_agent_text() {
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
                &message("msg-1", "hidden", Some(AgentMessagePhase::Unknown)),
                &mut sent
            )
            .as_deref(),
            Some("hidden")
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

    #[test]
    fn emits_each_image_once() {
        let mut sent = HashSet::new();
        let image = ThreadItem::ImageView(codex_app_server_sdk::api::ImageViewItem {
            id: "image-1".to_string(),
            path: "/tmp/output.png".to_string(),
        });

        assert_eq!(
            next_image_path(&image, &mut sent),
            Some(PathBuf::from("/tmp/output.png"))
        );
        assert_eq!(next_image_path(&image, &mut sent), None);
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
}
