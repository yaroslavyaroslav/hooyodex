use std::path::PathBuf;

use anyhow::{Result, bail};

use crate::config::AppConfig;
use crate::markdown::markdown_to_telegram_html;
use crate::telegram::{
    OutboundTelegramMedia, send_audio, send_document, send_markdown_message, send_photo, send_voice,
};

pub mod whatsapp;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum ChannelKind {
    Telegram,
    WhatsApp,
    Mattermost,
    RocketChat,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TelegramReplyTarget {
    pub chat_id: i64,
    pub thread_id: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(dead_code)]
pub enum ReplyTarget {
    Telegram(TelegramReplyTarget),
    WhatsApp(whatsapp::WhatsAppReplyTarget),
    Mattermost {
        account_id: String,
        conversation_id: String,
    },
    RocketChat {
        account_id: String,
        conversation_id: String,
    },
}

impl ReplyTarget {
    pub fn telegram(chat_id: i64, thread_id: Option<i64>) -> Self {
        Self::Telegram(TelegramReplyTarget { chat_id, thread_id })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(dead_code)]
pub struct EventRoute {
    pub channel: ChannelKind,
    pub account_id: String,
    pub conversation_id: String,
    pub thread_id: Option<String>,
}

impl EventRoute {
    #[allow(dead_code)]
    pub fn session_key(&self) -> String {
        match &self.thread_id {
            Some(thread_id) => format!(
                "{}:{}:{}:{}",
                self.channel.as_str(),
                self.account_id,
                self.conversation_id,
                thread_id
            ),
            None => format!(
                "{}:{}:{}",
                self.channel.as_str(),
                self.account_id,
                self.conversation_id
            ),
        }
    }
}

impl ChannelKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ChannelKind::Telegram => "telegram",
            ChannelKind::WhatsApp => "whatsapp",
            ChannelKind::Mattermost => "mattermost",
            ChannelKind::RocketChat => "rocketchat",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaKind {
    Photo,
    Document,
    Audio,
    Voice,
}

#[derive(Debug, Clone)]
pub enum TurnOutput {
    Markdown {
        text: String,
        disable_notification: bool,
    },
    Media {
        kind: MediaKind,
        path: PathBuf,
        caption_markdown: Option<String>,
        file_name: Option<String>,
        mime_type: Option<String>,
        disable_notification: bool,
    },
}

pub async fn send_turn_output_to_target(
    config: &AppConfig,
    target: &ReplyTarget,
    output: TurnOutput,
) -> Result<()> {
    match (target, output) {
        (
            ReplyTarget::Telegram(target),
            TurnOutput::Markdown {
                text,
                disable_notification,
            },
        ) => {
            send_markdown_message(
                config,
                target.chat_id,
                target.thread_id,
                disable_notification,
                &text,
            )
            .await
        }
        (
            ReplyTarget::Telegram(target),
            TurnOutput::Media {
                kind,
                path,
                caption_markdown,
                file_name,
                mime_type,
                disable_notification,
            },
        ) => {
            let caption_html = caption_markdown.as_deref().map(markdown_to_telegram_html);
            let media = OutboundTelegramMedia {
                chat_id: target.chat_id,
                thread_id: target.thread_id,
                path: &path,
                disable_notification,
                caption_html: caption_html.as_deref(),
                file_name_override: file_name.as_deref(),
                mime_type_override: mime_type.as_deref(),
            };
            match kind {
                MediaKind::Photo => send_photo(config, media).await,
                MediaKind::Document => send_document(config, media).await,
                MediaKind::Audio => send_audio(config, media).await,
                MediaKind::Voice => send_voice(config, media).await,
            }
        }
        (
            ReplyTarget::WhatsApp(target),
            TurnOutput::Markdown {
                text,
                disable_notification: _,
            },
        ) => whatsapp::send_whatsapp_text(config, target, &text).await,
        (ReplyTarget::WhatsApp(_), TurnOutput::Media { .. }) => {
            bail!("WhatsApp media delivery is not implemented yet")
        }
        (ReplyTarget::Mattermost { .. }, _) => {
            bail!("Mattermost reply delivery is not wired yet")
        }
        (ReplyTarget::RocketChat { .. }, _) => {
            bail!("Rocket.Chat reply delivery is not wired yet")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_route_builds_channel_agnostic_session_keys() {
        let route = EventRoute {
            channel: ChannelKind::WhatsApp,
            account_id: "default".to_string(),
            conversation_id: "client123".to_string(),
            thread_id: None,
        };
        assert_eq!(route.session_key(), "whatsapp:default:client123");

        let threaded = EventRoute {
            channel: ChannelKind::Mattermost,
            account_id: "ops".to_string(),
            conversation_id: "chan-1".to_string(),
            thread_id: Some("root-post".to_string()),
        };
        assert_eq!(threaded.session_key(), "mattermost:ops:chan-1:root-post");
    }
}
