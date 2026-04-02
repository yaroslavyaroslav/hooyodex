use anyhow::{Context, Result, bail};
use reqwest::Client;
use serde::Deserialize;
use serde_json::json;

use crate::config::{AppConfig, WhatsAppConfig};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WhatsAppReplyTarget {
    pub account_id: String,
    pub chat_id: String,
}

impl WhatsAppReplyTarget {
    pub fn session_key(&self) -> String {
        format!("whatsapp:{}:{}", self.account_id, self.chat_id)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WhatsAppInboundEvent {
    pub account_id: String,
    pub chat_id: String,
    pub sender_id: String,
    pub sender_name: String,
    pub message_id: String,
    pub text: String,
}

impl WhatsAppInboundEvent {
    pub fn reply_target(&self) -> WhatsAppReplyTarget {
        WhatsAppReplyTarget {
            account_id: self.account_id.clone(),
            chat_id: self.chat_id.clone(),
        }
    }

    pub fn session_key(&self) -> String {
        self.reply_target().session_key()
    }
}

#[derive(Debug, Deserialize)]
pub struct WhatsAppWebhookVerificationQuery {
    #[serde(rename = "hub.mode")]
    pub mode: Option<String>,
    #[serde(rename = "hub.verify_token")]
    pub verify_token: Option<String>,
    #[serde(rename = "hub.challenge")]
    pub challenge: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct WhatsAppWebhookPayload {
    pub entry: Vec<WhatsAppEntry>,
}

#[derive(Debug, Deserialize)]
pub struct WhatsAppEntry {
    pub changes: Vec<WhatsAppChange>,
}

#[derive(Debug, Deserialize)]
pub struct WhatsAppChange {
    pub field: String,
    pub value: WhatsAppValue,
}

#[derive(Debug, Deserialize)]
pub struct WhatsAppValue {
    pub contacts: Option<Vec<WhatsAppContact>>,
    pub messages: Option<Vec<WhatsAppMessage>>,
}

#[derive(Debug, Deserialize)]
pub struct WhatsAppContact {
    pub wa_id: Option<String>,
    pub profile: Option<WhatsAppProfile>,
}

#[derive(Debug, Deserialize)]
pub struct WhatsAppProfile {
    pub name: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct WhatsAppMessage {
    pub from: String,
    pub id: String,
    #[serde(rename = "type")]
    pub message_type: String,
    pub text: Option<WhatsAppText>,
}

#[derive(Debug, Deserialize)]
pub struct WhatsAppText {
    pub body: String,
}

pub fn verify_webhook(
    config: &WhatsAppConfig,
    query: &WhatsAppWebhookVerificationQuery,
) -> Option<String> {
    (query.mode.as_deref() == Some("subscribe")
        && query.verify_token.as_deref() == Some(config.verify_token.as_str()))
    .then(|| query.challenge.clone())
    .flatten()
}

pub fn normalize_webhook_event(
    config: &WhatsAppConfig,
    payload: &WhatsAppWebhookPayload,
) -> Option<WhatsAppInboundEvent> {
    for entry in &payload.entry {
        for change in &entry.changes {
            if change.field != "messages" {
                continue;
            }
            let contacts = change.value.contacts.as_deref().unwrap_or(&[]);
            let messages = change.value.messages.as_deref().unwrap_or(&[]);
            for message in messages {
                if message.message_type != "text" {
                    continue;
                }
                let text = message
                    .text
                    .as_ref()
                    .map(|value| value.body.trim())
                    .filter(|value| !value.is_empty())?;
                if !config.allowed_senders.is_empty()
                    && !config
                        .allowed_senders
                        .iter()
                        .any(|sender| sender == &message.from)
                {
                    continue;
                }
                let sender_name = contacts
                    .iter()
                    .find(|contact| contact.wa_id.as_deref() == Some(message.from.as_str()))
                    .and_then(|contact| contact.profile.as_ref())
                    .and_then(|profile| profile.name.clone())
                    .unwrap_or_else(|| message.from.clone());

                return Some(WhatsAppInboundEvent {
                    account_id: config.account_id.clone(),
                    chat_id: message.from.clone(),
                    sender_id: message.from.clone(),
                    sender_name,
                    message_id: message.id.clone(),
                    text: text.to_string(),
                });
            }
        }
    }
    None
}

pub async fn send_whatsapp_text(
    config: &AppConfig,
    target: &WhatsAppReplyTarget,
    text: &str,
) -> Result<()> {
    let whatsapp = config
        .whatsapp
        .as_ref()
        .context("whatsapp is not configured")?;
    let url = format!(
        "{}/{}/messages",
        whatsapp.api_base_url.trim_end_matches('/'),
        whatsapp.phone_number_id
    );
    let response = Client::new()
        .post(url)
        .bearer_auth(&whatsapp.access_token)
        .json(&json!({
            "messaging_product": "whatsapp",
            "to": target.chat_id,
            "type": "text",
            "text": { "body": text }
        }))
        .send()
        .await?;
    if !response.status().is_success() {
        let body = response.text().await.unwrap_or_default();
        bail!("WhatsApp send failed: {body}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verifies_whatsapp_webhook_query() {
        let config = WhatsAppConfig {
            account_id: "default".to_string(),
            verify_token: "secret".to_string(),
            access_token: "token".to_string(),
            phone_number_id: "phone".to_string(),
            webhook_path: "/whatsapp/webhook".to_string(),
            api_base_url: "https://graph.facebook.com/v21.0".to_string(),
            allowed_senders: Vec::new(),
        };
        let query = WhatsAppWebhookVerificationQuery {
            mode: Some("subscribe".to_string()),
            verify_token: Some("secret".to_string()),
            challenge: Some("abc".to_string()),
        };
        assert_eq!(verify_webhook(&config, &query).as_deref(), Some("abc"));
    }

    #[test]
    fn normalizes_text_message_webhook() {
        let config = WhatsAppConfig {
            account_id: "default".to_string(),
            verify_token: "secret".to_string(),
            access_token: "token".to_string(),
            phone_number_id: "phone".to_string(),
            webhook_path: "/whatsapp/webhook".to_string(),
            api_base_url: "https://graph.facebook.com/v21.0".to_string(),
            allowed_senders: Vec::new(),
        };
        let payload: WhatsAppWebhookPayload = serde_json::from_value(json!({
            "entry": [{
                "changes": [{
                    "field": "messages",
                    "value": {
                        "contacts": [{
                            "wa_id": "15551230000",
                            "profile": { "name": "Client" }
                        }],
                        "messages": [{
                            "from": "15551230000",
                            "id": "wamid.1",
                            "type": "text",
                            "text": { "body": "hello" }
                        }]
                    }
                }]
            }]
        }))
        .expect("payload");

        let event = normalize_webhook_event(&config, &payload).expect("event");
        assert_eq!(event.chat_id, "15551230000");
        assert_eq!(event.sender_name, "Client");
        assert_eq!(event.text, "hello");
    }
}
