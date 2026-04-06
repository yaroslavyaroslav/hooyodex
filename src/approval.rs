use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use serde_json::json;
use tokio::sync::{Mutex, oneshot};
use uuid::Uuid;

use crate::config::AppConfig;
use crate::telegram::{
    TelegramCallbackQuery, TelegramUpdate, answer_callback_query, edit_message_reply_markup_remove,
    send_markdown_message, send_markdown_message_with_inline_keyboard,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OperatorTarget {
    pub chat_id: i64,
    pub thread_id: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApprovalOutcome {
    Approved,
    Denied,
    Steer(String),
}

#[derive(Debug)]
struct PendingApproval {
    operator_target: OperatorTarget,
    operator_message_id: i64,
    resolution_tx: oneshot::Sender<ApprovalOutcome>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct SteerKey {
    chat_id: i64,
    thread_id: Option<i64>,
    user_id: i64,
}

#[derive(Default)]
pub struct ApprovalManager {
    pending: Mutex<HashMap<String, PendingApproval>>,
    awaiting_steer: Mutex<HashMap<SteerKey, String>>,
}

impl ApprovalManager {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub async fn request(
        &self,
        config: &AppConfig,
        target: OperatorTarget,
        summary_markdown: &str,
        proposed_reply: &str,
    ) -> Result<ApprovalOutcome> {
        let approval_id = Uuid::new_v4().simple().to_string();
        let prompt = format!(
            "{summary_markdown}\n\n---\n\n**Proposed reply**\n\n{}",
            proposed_reply.trim()
        );
        let keyboard = json!({
            "inline_keyboard": [[
                { "text": "Approve", "callback_data": format!("apr:approve:{approval_id}") },
                { "text": "Deny", "callback_data": format!("apr:deny:{approval_id}") },
                { "text": "Steer", "callback_data": format!("apr:steer:{approval_id}") }
            ]]
        });
        let (resolution_tx, resolution_rx) = oneshot::channel();
        let operator_message_id = send_markdown_message_with_inline_keyboard(
            config,
            target.chat_id,
            target.thread_id,
            false,
            &prompt,
            keyboard,
        )
        .await?;

        self.pending.lock().await.insert(
            approval_id,
            PendingApproval {
                operator_target: target,
                operator_message_id,
                resolution_tx,
            },
        );

        Ok(resolution_rx.await.unwrap_or(ApprovalOutcome::Denied))
    }

    pub async fn handle_update(
        &self,
        config: &AppConfig,
        update: &TelegramUpdate,
        allowed_users: &[i64],
    ) -> Result<bool> {
        if let Some(callback) = update.callback_query.as_ref() {
            return self
                .handle_callback_query(config, callback, allowed_users)
                .await;
        }
        self.handle_steer_message(config, update, allowed_users)
            .await
    }

    async fn handle_callback_query(
        &self,
        config: &AppConfig,
        callback: &TelegramCallbackQuery,
        allowed_users: &[i64],
    ) -> Result<bool> {
        let Some(data) = callback.data.as_deref() else {
            return Ok(false);
        };
        if !data.starts_with("apr:") {
            return Ok(false);
        }

        if !allowed_users.is_empty() && !allowed_users.contains(&callback.from.id) {
            answer_callback_query(config, &callback.id, Some("Not allowed."), false).await?;
            return Ok(true);
        }

        let mut parts = data.splitn(3, ':');
        let _prefix = parts.next();
        let action = parts.next().unwrap_or_default();
        let approval_id = parts.next().unwrap_or_default();
        if approval_id.is_empty() {
            answer_callback_query(config, &callback.id, Some("Malformed approval."), false).await?;
            return Ok(true);
        }

        match action {
            "approve" => {
                let Some(pending) = self.pending.lock().await.remove(approval_id) else {
                    answer_callback_query(config, &callback.id, Some("Approval expired."), false)
                        .await?;
                    return Ok(true);
                };
                let _ = edit_message_reply_markup_remove(
                    config,
                    pending.operator_target.chat_id,
                    pending.operator_message_id,
                )
                .await;
                let _ = pending.resolution_tx.send(ApprovalOutcome::Approved);
                answer_callback_query(config, &callback.id, Some("Approved."), false).await?;
                Ok(true)
            }
            "deny" => {
                let Some(pending) = self.pending.lock().await.remove(approval_id) else {
                    answer_callback_query(config, &callback.id, Some("Approval expired."), false)
                        .await?;
                    return Ok(true);
                };
                let _ = edit_message_reply_markup_remove(
                    config,
                    pending.operator_target.chat_id,
                    pending.operator_message_id,
                )
                .await;
                let _ = pending.resolution_tx.send(ApprovalOutcome::Denied);
                answer_callback_query(config, &callback.id, Some("Denied."), false).await?;
                Ok(true)
            }
            "steer" => {
                let pending = self.pending.lock().await;
                let Some(entry) = pending.get(approval_id) else {
                    answer_callback_query(config, &callback.id, Some("Approval expired."), false)
                        .await?;
                    return Ok(true);
                };
                let key = callback.message.as_ref().map(|message| SteerKey {
                    chat_id: message.chat.id,
                    thread_id: message.message_thread_id,
                    user_id: callback.from.id,
                });
                let Some(key) = key else {
                    answer_callback_query(
                        config,
                        &callback.id,
                        Some("Steer is unavailable here."),
                        false,
                    )
                    .await?;
                    return Ok(true);
                };
                self.awaiting_steer
                    .lock()
                    .await
                    .insert(key, approval_id.to_string());
                let _ = edit_message_reply_markup_remove(
                    config,
                    entry.operator_target.chat_id,
                    entry.operator_message_id,
                )
                .await;
                drop(pending);
                answer_callback_query(
                    config,
                    &callback.id,
                    Some("Send your steering message next."),
                    false,
                )
                .await?;
                send_markdown_message(
                    config,
                    key.chat_id,
                    key.thread_id,
                    false,
                    "Send your steering feedback as the next text message in this chat.",
                )
                .await?;
                Ok(true)
            }
            _ => {
                answer_callback_query(config, &callback.id, Some("Unknown action."), false).await?;
                Ok(true)
            }
        }
    }

    async fn handle_steer_message(
        &self,
        config: &AppConfig,
        update: &TelegramUpdate,
        allowed_users: &[i64],
    ) -> Result<bool> {
        let Some(message) = update.any_message() else {
            return Ok(false);
        };
        let Some(from) = message.from.as_ref() else {
            return Ok(false);
        };
        if !allowed_users.is_empty() && !allowed_users.contains(&from.id) {
            return Ok(false);
        }

        let key = SteerKey {
            chat_id: message.chat.id,
            thread_id: message.message_thread_id,
            user_id: from.id,
        };
        let Some(approval_id) = self.awaiting_steer.lock().await.remove(&key) else {
            return Ok(false);
        };

        let Some(steer_text) = message
            .text
            .as_deref()
            .map(str::trim)
            .filter(|text| !text.is_empty())
        else {
            self.awaiting_steer.lock().await.insert(key, approval_id);
            send_markdown_message(
                config,
                message.chat.id,
                message.message_thread_id,
                false,
                "Please send a text steering message.",
            )
            .await?;
            return Ok(true);
        };

        if let Some(pending) = self.pending.lock().await.remove(&approval_id) {
            let _ = pending
                .resolution_tx
                .send(ApprovalOutcome::Steer(steer_text.to_string()));
        }

        send_markdown_message(
            config,
            message.chat.id,
            message.message_thread_id,
            false,
            "Got it. Revising with your steering feedback.",
        )
        .await?;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use anyhow::Result;
    use axum::Router;
    use axum::body::Bytes;
    use axum::extract::State;
    use axum::routing::post;
    use serde_json::Value;
    use tokio::net::TcpListener;

    use crate::config::{
        AppConfig, AppPaths, CodexConfig, OperatorConfig, OperatorTelegramConfig, ServerConfig,
        TelegramConfig, VoiceConfig,
    };
    use crate::telegram::{TelegramChat, TelegramMessage, TelegramUpdate, TelegramUser};

    type RecordedRequests = Arc<Mutex<Vec<(String, Vec<u8>)>>>;

    #[derive(Clone, Default)]
    struct MockTelegramState {
        requests: RecordedRequests,
    }

    async fn start_mock_telegram_api() -> Result<(String, MockTelegramState)> {
        async fn record_request(
            State(state): State<MockTelegramState>,
            uri: axum::http::Uri,
            body: Bytes,
        ) -> axum::Json<Value> {
            state
                .requests
                .lock()
                .await
                .push((uri.path().to_string(), body.to_vec()));
            axum::Json(serde_json::json!({
                "ok": true,
                "result": { "message_id": 55 }
            }))
        }

        let state = MockTelegramState::default();
        let app = Router::new()
            .route("/{*path}", post(record_request))
            .with_state(state.clone());
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        Ok((format!("http://{}", address), state))
    }

    fn test_config(api_base_url: &str) -> AppConfig {
        AppConfig {
            paths: AppPaths {
                config_path: "/tmp/config.toml".into(),
                state_dir: "/tmp/state".into(),
                cache_dir: "/tmp/cache".into(),
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
            voice: VoiceConfig::default(),
            codex: CodexConfig {
                connect_url: "ws://127.0.0.1:4222".to_string(),
                listen_url: "ws://127.0.0.1:4222".to_string(),
                reuse_existing_server: true,
                experimental_api: false,
                working_directory: "/tmp".into(),
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

    fn callback_update(data: &str) -> TelegramUpdate {
        TelegramUpdate {
            update_id: 1,
            message: None,
            edited_message: None,
            business_message: None,
            edited_business_message: None,
            callback_query: Some(TelegramCallbackQuery {
                id: "cb-1".to_string(),
                from: TelegramUser {
                    id: 42,
                    first_name: Some("Op".to_string()),
                    last_name: None,
                },
                message: Some(TelegramMessage {
                    message_id: 55,
                    chat: TelegramChat { id: 100 },
                    from: Some(TelegramUser {
                        id: 999,
                        first_name: Some("Bot".to_string()),
                        last_name: None,
                    }),
                    sender_chat: None,
                    business_connection_id: None,
                    text: Some("approval".to_string()),
                    caption: None,
                    photo: None,
                    document: None,
                    voice: None,
                    message_thread_id: Some(777),
                    reply_to_message: None,
                    quote: None,
                }),
                data: Some(data.to_string()),
            }),
        }
    }

    fn steer_message_update(text: &str) -> TelegramUpdate {
        TelegramUpdate {
            update_id: 2,
            message: Some(TelegramMessage {
                message_id: 56,
                chat: TelegramChat { id: 100 },
                from: Some(TelegramUser {
                    id: 42,
                    first_name: Some("Op".to_string()),
                    last_name: None,
                }),
                sender_chat: None,
                business_connection_id: None,
                text: Some(text.to_string()),
                caption: None,
                photo: None,
                document: None,
                voice: None,
                message_thread_id: Some(777),
                reply_to_message: None,
                quote: None,
            }),
            edited_message: None,
            business_message: None,
            edited_business_message: None,
            callback_query: None,
        }
    }

    #[test]
    fn operator_target_is_copyable() {
        let target = OperatorTarget {
            chat_id: 1,
            thread_id: Some(2),
        };
        assert_eq!(target.chat_id, 1);
        assert_eq!(target.thread_id, Some(2));
    }

    #[tokio::test]
    async fn approval_manager_resolves_approve_callback() -> Result<()> {
        let (api_base_url, _state) = start_mock_telegram_api().await?;
        let config = test_config(&api_base_url);
        let approvals = ApprovalManager::new();
        let approvals_for_request = approvals.clone();

        let request_task = tokio::spawn(async move {
            approvals_for_request
                .request(
                    &config,
                    OperatorTarget {
                        chat_id: 100,
                        thread_id: Some(777),
                    },
                    "Please approve this reply.",
                    "hello there",
                )
                .await
        });

        wait_for_pending_approval(&approvals).await;
        let handled = approvals
            .handle_update(
                &test_config(&api_base_url),
                &callback_update("apr:approve:00000000000000000000000000000000"),
                &[42],
            )
            .await?;
        assert!(
            handled,
            "unknown approval id should still be consumed as expired"
        );

        let approval_id = approvals
            .pending
            .lock()
            .await
            .keys()
            .next()
            .cloned()
            .expect("approval");
        let handled = approvals
            .handle_update(
                &test_config(&api_base_url),
                &callback_update(&format!("apr:approve:{approval_id}")),
                &[42],
            )
            .await?;
        assert!(handled);
        let result = request_task.await??;
        assert_eq!(result, ApprovalOutcome::Approved);
        Ok(())
    }

    #[tokio::test]
    async fn approval_manager_resolves_steer_follow_up_message() -> Result<()> {
        let (api_base_url, _state) = start_mock_telegram_api().await?;
        let config = test_config(&api_base_url);
        let approvals = ApprovalManager::new();
        let approvals_for_request = approvals.clone();

        let request_task = tokio::spawn(async move {
            approvals_for_request
                .request(
                    &config,
                    OperatorTarget {
                        chat_id: 100,
                        thread_id: Some(777),
                    },
                    "Need steering",
                    "draft reply",
                )
                .await
        });

        wait_for_pending_approval(&approvals).await;
        let approval_id = approvals
            .pending
            .lock()
            .await
            .keys()
            .next()
            .cloned()
            .expect("approval");
        let config = test_config(&api_base_url);
        assert!(
            approvals
                .handle_update(
                    &config,
                    &callback_update(&format!("apr:steer:{approval_id}")),
                    &[42]
                )
                .await?
        );
        assert!(
            approvals
                .handle_update(&config, &steer_message_update("make it shorter"), &[42])
                .await?
        );
        let result = request_task.await??;
        assert_eq!(
            result,
            ApprovalOutcome::Steer("make it shorter".to_string())
        );
        Ok(())
    }

    async fn wait_for_pending_approval(approvals: &Arc<ApprovalManager>) {
        for _ in 0..20 {
            if !approvals.pending.lock().await.is_empty() {
                return;
            }
            tokio::task::yield_now().await;
        }
        panic!("approval");
    }
}
