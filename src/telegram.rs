use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use reqwest::Client;
use reqwest::multipart::{Form, Part};
use serde::Deserialize;
use serde_json::json;
use tokio::fs;

use crate::config::AppConfig;
use crate::markdown::{markdown_to_telegram_html, sanitize_telegram_html, split_telegram_message};

#[derive(Debug, Deserialize)]
pub struct TelegramUpdate {
    pub update_id: i64,
    pub message: Option<TelegramMessage>,
    pub edited_message: Option<TelegramMessage>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct TelegramMessage {
    pub message_id: i64,
    pub chat: TelegramChat,
    pub from: Option<TelegramUser>,
    pub sender_chat: Option<TelegramSenderChat>,
    pub text: Option<String>,
    pub caption: Option<String>,
    pub photo: Option<Vec<TelegramPhotoSize>>,
    pub document: Option<TelegramDocument>,
    pub voice: Option<TelegramVoice>,
    pub message_thread_id: Option<i64>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct TelegramChat {
    pub id: i64,
    #[serde(rename = "type")]
    pub kind: String,
}

#[derive(Debug, Deserialize, Clone)]
pub struct TelegramUser {
    pub id: i64,
    pub first_name: Option<String>,
    pub last_name: Option<String>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct TelegramSenderChat {
    pub id: i64,
    pub title: Option<String>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct TelegramPhotoSize {
    pub file_id: String,
}

#[derive(Debug, Deserialize, Clone)]
pub struct TelegramVoice {
    pub file_id: String,
    pub duration: Option<u32>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct TelegramDocument {
    pub file_id: String,
    pub file_name: Option<String>,
}

#[derive(Debug, Clone)]
pub struct InboundMessage {
    pub chat_id: i64,
    pub sender_id: i64,
    pub sender_name: String,
    pub message_id: i64,
    pub thread_id: Option<i64>,
    pub text: Option<String>,
    pub image_file_id: Option<String>,
    pub document_file_id: Option<String>,
    pub document_name: Option<String>,
    pub voice_file_id: Option<String>,
    pub voice_duration: Option<u32>,
}

#[derive(Debug, Clone)]
pub enum DownloadedAttachment {
    Image(PathBuf),
    File(PathBuf),
    Voice {
        path: PathBuf,
        duration_seconds: Option<u32>,
        transcript: Option<String>,
    },
}

impl InboundMessage {
    pub fn requests_new_chat(&self) -> bool {
        matches!(
            self.text.as_deref().map(str::trim),
            Some(text) if text.eq_ignore_ascii_case("/new")
        )
    }
}

pub fn normalize_update(update: &TelegramUpdate, allowed_users: &[i64]) -> Option<InboundMessage> {
    let message = update.message.as_ref().or(update.edited_message.as_ref())?;

    let (sender_id, sender_name) = if let Some(from) = &message.from {
        let first = from
            .first_name
            .clone()
            .unwrap_or_else(|| "Unknown".to_string());
        let last = from.last_name.clone().unwrap_or_default();
        let name = if last.is_empty() {
            first
        } else {
            format!("{first} {last}")
        };
        (from.id, name)
    } else if let Some(sender_chat) = &message.sender_chat {
        (
            sender_chat.id,
            sender_chat
                .title
                .clone()
                .unwrap_or_else(|| "Unknown".to_string()),
        )
    } else {
        return None;
    };

    if !allowed_users.is_empty() && !allowed_users.contains(&sender_id) {
        return None;
    }

    let image_file_id = message
        .photo
        .as_ref()
        .and_then(|photos| photos.last())
        .map(|photo| photo.file_id.clone());
    let document_file_id = message
        .document
        .as_ref()
        .map(|document| document.file_id.clone());
    let voice_file_id = message.voice.as_ref().map(|voice| voice.file_id.clone());

    if message.text.is_none()
        && image_file_id.is_none()
        && document_file_id.is_none()
        && voice_file_id.is_none()
    {
        return None;
    }

    Some(InboundMessage {
        chat_id: message.chat.id,
        sender_id,
        sender_name,
        message_id: message.message_id,
        thread_id: if message.chat.kind == "private" {
            None
        } else {
            message.message_thread_id
        },
        text: message.text.clone().or_else(|| message.caption.clone()),
        image_file_id,
        document_file_id,
        document_name: message
            .document
            .as_ref()
            .and_then(|document| document.file_name.clone()),
        voice_file_id,
        voice_duration: message.voice.as_ref().and_then(|voice| voice.duration),
    })
}

pub async fn set_webhook(config: &AppConfig) -> Result<()> {
    let client = Client::new();
    let url = format!(
        "{}/bot{}/setWebhook",
        config.telegram.api_base_url.trim_end_matches('/'),
        config.telegram.bot_token
    );
    let body = json!({
        "url": config.webhook_url(),
        "drop_pending_updates": false,
    });
    let response: serde_json::Value = client.post(url).json(&body).send().await?.json().await?;
    if response["ok"].as_bool() != Some(true) {
        bail!("Telegram setWebhook failed: {}", response);
    }
    println!("{}", config.webhook_url());
    Ok(())
}

pub async fn delete_webhook(config: &AppConfig) -> Result<()> {
    let client = Client::new();
    let url = format!(
        "{}/bot{}/deleteWebhook",
        config.telegram.api_base_url.trim_end_matches('/'),
        config.telegram.bot_token
    );
    let response: serde_json::Value = client
        .post(url)
        .json(&json!({}))
        .send()
        .await?
        .json()
        .await?;
    if response["ok"].as_bool() != Some(true) {
        bail!("Telegram deleteWebhook failed: {}", response);
    }
    Ok(())
}

pub async fn send_markdown_message(
    config: &AppConfig,
    chat_id: i64,
    thread_id: Option<i64>,
    markdown: &str,
) -> Result<()> {
    let client = Client::new();
    let url = format!(
        "{}/bot{}/sendMessage",
        config.telegram.api_base_url.trim_end_matches('/'),
        config.telegram.bot_token
    );
    let html = sanitize_telegram_html(&markdown_to_telegram_html(markdown));
    for chunk in split_telegram_message(&html, 4096) {
        let mut body = json!({
            "chat_id": chat_id,
            "text": chunk,
            "parse_mode": "HTML",
        });
        if let Some(thread_id) = thread_id {
            body["message_thread_id"] = json!(thread_id);
        }
        let response = client.post(&url).json(&body).send().await?;
        if !response.status().is_success() {
            let text = response.text().await.unwrap_or_default();
            bail!("Telegram sendMessage failed: {}", text);
        }
    }
    Ok(())
}

pub async fn send_photo(
    config: &AppConfig,
    chat_id: i64,
    thread_id: Option<i64>,
    path: &Path,
    caption_html: Option<&str>,
) -> Result<()> {
    let client = Client::new();
    let url = format!(
        "{}/bot{}/sendPhoto",
        config.telegram.api_base_url.trim_end_matches('/'),
        config.telegram.bot_token
    );

    let bytes = fs::read(path)
        .await
        .with_context(|| format!("failed to read image {}", path.display()))?;
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("image.png")
        .to_string();

    let mut photo = Part::bytes(bytes).file_name(file_name);
    if let Some(mime) = image_mime_type(path) {
        photo = photo.mime_str(mime)?;
    }

    let mut form = Form::new()
        .text("chat_id", chat_id.to_string())
        .part("photo", photo);
    if let Some(thread_id) = thread_id {
        form = form.text("message_thread_id", thread_id.to_string());
    }
    if let Some(caption_html) = caption_html.filter(|caption| !caption.trim().is_empty()) {
        form = form
            .text("caption", sanitize_telegram_html(caption_html))
            .text("parse_mode", "HTML");
    }

    let response = client.post(&url).multipart(form).send().await?;
    if !response.status().is_success() {
        let text = response.text().await.unwrap_or_default();
        bail!("Telegram sendPhoto failed: {}", text);
    }
    Ok(())
}

pub async fn download_attachment(
    config: &AppConfig,
    inbound: &InboundMessage,
    attachment_root: &Path,
) -> Result<Option<DownloadedAttachment>> {
    let client = Client::new();
    fs::create_dir_all(attachment_root)
        .await
        .with_context(|| format!("failed to create {}", attachment_root.display()))?;

    if let Some(file_id) = &inbound.image_file_id {
        let path = download_file(
            &client,
            config,
            file_id,
            attachment_root,
            inbound.chat_id,
            "image",
        )
        .await?;
        return Ok(Some(DownloadedAttachment::Image(path)));
    }

    if let Some(file_id) = &inbound.voice_file_id {
        let path = download_file(
            &client,
            config,
            file_id,
            attachment_root,
            inbound.chat_id,
            "voice",
        )
        .await?;
        return Ok(Some(DownloadedAttachment::Voice {
            path,
            duration_seconds: inbound.voice_duration,
            transcript: None,
        }));
    }

    if let Some(file_id) = &inbound.document_file_id {
        let path = download_file(
            &client,
            config,
            file_id,
            attachment_root,
            inbound.chat_id,
            inbound.document_name.as_deref().unwrap_or("file"),
        )
        .await?;
        return Ok(Some(DownloadedAttachment::File(path)));
    }

    Ok(None)
}

async fn download_file(
    client: &Client,
    config: &AppConfig,
    file_id: &str,
    attachment_root: &Path,
    chat_id: i64,
    kind: &str,
) -> Result<PathBuf> {
    let api_base = config.telegram.api_base_url.trim_end_matches('/');
    let get_file_url = format!("{api_base}/bot{}/getFile", config.telegram.bot_token);
    let response: serde_json::Value = client
        .post(&get_file_url)
        .json(&json!({ "file_id": file_id }))
        .send()
        .await?
        .json()
        .await?;

    let file_path = response["result"]["file_path"]
        .as_str()
        .context("Telegram getFile did not return result.file_path")?;
    let download_url = format!(
        "{api_base}/file/bot{}/{}",
        config.telegram.bot_token, file_path
    );
    let bytes = client.get(download_url).send().await?.bytes().await?;

    let extension = Path::new(file_path)
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or("bin");
    let chat_dir = attachment_root.join(chat_id.to_string());
    fs::create_dir_all(&chat_dir)
        .await
        .with_context(|| format!("failed to create {}", chat_dir.display()))?;

    let filename = format!("{}-{}.{}", kind, uuid::Uuid::new_v4(), extension);
    let full_path = chat_dir.join(filename);
    fs::write(&full_path, &bytes)
        .await
        .with_context(|| format!("failed to write {}", full_path.display()))?;
    Ok(full_path)
}

fn image_mime_type(path: &Path) -> Option<&'static str> {
    match path.extension().and_then(|ext| ext.to_str()) {
        Some("png") => Some("image/png"),
        Some("jpg") | Some("jpeg") => Some("image/jpeg"),
        Some("webp") => Some("image/webp"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_message() -> TelegramMessage {
        TelegramMessage {
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
            text: Some("hello".to_string()),
            caption: None,
            photo: None,
            document: None,
            voice: None,
            message_thread_id: None,
        }
    }

    #[test]
    fn normalize_update_filters_unlisted_user() {
        let update = TelegramUpdate {
            update_id: 1,
            message: Some(sample_message()),
            edited_message: None,
        };
        assert!(normalize_update(&update, &[999]).is_none());
    }

    #[test]
    fn normalize_update_accepts_document() {
        let mut message = sample_message();
        message.text = None;
        message.document = Some(TelegramDocument {
            file_id: "doc-1".to_string(),
            file_name: Some("note.txt".to_string()),
        });
        let update = TelegramUpdate {
            update_id: 1,
            message: Some(message),
            edited_message: None,
        };
        let inbound = normalize_update(&update, &[42]).expect("inbound");
        assert_eq!(inbound.document_file_id.as_deref(), Some("doc-1"));
        assert_eq!(inbound.document_name.as_deref(), Some("note.txt"));
    }

    #[test]
    fn detects_common_image_mime_types() {
        assert_eq!(
            image_mime_type(Path::new("/tmp/file.png")),
            Some("image/png")
        );
        assert_eq!(
            image_mime_type(Path::new("/tmp/file.jpeg")),
            Some("image/jpeg")
        );
        assert_eq!(image_mime_type(Path::new("/tmp/file.bin")), None);
    }

    #[test]
    fn detects_new_chat_command() {
        let inbound = normalize_update(
            &TelegramUpdate {
                update_id: 1,
                message: Some(sample_message()),
                edited_message: None,
            },
            &[42],
        )
        .expect("inbound");
        assert!(!inbound.requests_new_chat());

        let mut command_message = sample_message();
        command_message.text = Some("/new".to_string());
        let command_inbound = normalize_update(
            &TelegramUpdate {
                update_id: 2,
                message: Some(command_message),
                edited_message: None,
            },
            &[42],
        )
        .expect("command inbound");
        assert!(command_inbound.requests_new_chat());
    }
}
