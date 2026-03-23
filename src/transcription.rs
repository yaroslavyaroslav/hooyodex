use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use tokio::fs;
use tokio::process::Command;
use uuid::Uuid;

use crate::config::AppConfig;

pub async fn transcribe_voice_message(config: &AppConfig, audio_path: &Path) -> Result<String> {
    let output_dir = transcript_job_dir(config);
    fs::create_dir_all(&output_dir)
        .await
        .with_context(|| format!("failed to create {}", output_dir.display()))?;

    let transcript_template = "transcript";
    let command_path = resolve_transcriber_command(config);
    let output = Command::new(&command_path)
        .arg(audio_path)
        .arg("--model")
        .arg(&config.voice.model)
        .arg("--output-dir")
        .arg(&output_dir)
        .arg("--output-format")
        .arg("txt")
        .arg("--output-template")
        .arg(transcript_template)
        .output()
        .await
        .with_context(|| {
            format!(
                "failed to start voice transcriber `{}`",
                command_path.display()
            )
        })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        bail!(
            "voice transcription command failed with status {}.\nstdout:\n{}\nstderr:\n{}",
            output.status,
            stdout.trim(),
            stderr.trim()
        );
    }

    let transcript_path = output_dir.join(format!("{transcript_template}.txt"));
    let transcript = fs::read_to_string(&transcript_path)
        .await
        .with_context(|| format!("failed to read {}", transcript_path.display()))?;
    let transcript = transcript.trim().to_string();
    if transcript.is_empty() {
        bail!(
            "voice transcription produced an empty transcript at {}",
            transcript_path.display()
        );
    }
    Ok(transcript)
}

fn transcript_job_dir(config: &AppConfig) -> PathBuf {
    config
        .paths
        .cache_dir
        .join("voice-transcripts")
        .join(Uuid::new_v4().to_string())
}

fn resolve_transcriber_command(config: &AppConfig) -> PathBuf {
    let configured = PathBuf::from(&config.voice.transcriber_command);
    if configured.is_absolute() {
        return configured;
    }

    let home_local = dirs::home_dir().map(|home| {
        home.join(".local")
            .join("bin")
            .join(&config.voice.transcriber_command)
    });
    match home_local {
        Some(path) if path.exists() => path,
        _ => configured,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use tempfile::TempDir;

    use crate::config::{AppPaths, CodexConfig, ServerConfig, TelegramConfig, VoiceConfig};

    fn test_config(tempdir: &TempDir) -> AppConfig {
        AppConfig {
            paths: AppPaths {
                config_path: tempdir.path().join("config.toml"),
                state_dir: tempdir.path().join("state"),
                cache_dir: tempdir.path().join("cache"),
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
                working_directory: tempdir.path().to_path_buf(),
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
    fn prefers_home_local_binary_when_present() {
        let tempdir = TempDir::new().expect("tempdir");
        let config = test_config(&tempdir);
        let resolved = resolve_transcriber_command(&config);
        let expected = dirs::home_dir()
            .expect("home dir")
            .join(".local")
            .join("bin")
            .join("parakeet-mlx");
        assert_eq!(resolved, expected);
    }
}
