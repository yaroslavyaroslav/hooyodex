use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::{ServiceCommand, config::AppPaths};

const LINUX_SERVICE_NAME: &str = "hooyodex.service";
const MAC_LABEL: &str = "com.hooyodex.agent";
const MAC_ALT_LABEL: &str = "dev.hooyodex.agent";

pub fn handle(command: ServiceCommand, paths: &AppPaths) -> Result<()> {
    match command {
        ServiceCommand::Install => install(paths),
        ServiceCommand::Uninstall => uninstall(paths),
        ServiceCommand::Start => run_platform_action(paths, PlatformAction::Start),
        ServiceCommand::Stop => run_platform_action(paths, PlatformAction::Stop),
        ServiceCommand::Restart => run_platform_action(paths, PlatformAction::Restart),
        ServiceCommand::Reload => run_platform_action(paths, PlatformAction::Reload),
        ServiceCommand::Status => run_platform_action(paths, PlatformAction::Status),
        ServiceCommand::Print => print_units(paths),
    }
}

enum PlatformAction {
    Start,
    Stop,
    Restart,
    Reload,
    Status,
}

fn install(paths: &AppPaths) -> Result<()> {
    let exe = current_exe()?;
    match env::consts::OS {
        "linux" => {
            let unit_path = linux_unit_path()?;
            if let Some(parent) = unit_path.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(&unit_path, render_linux_unit(&exe, &paths.config_path))?;
            run(Command::new("systemctl").args(["--user", "daemon-reload"]))?;
            run(Command::new("systemctl").args(["--user", "enable", "--now", LINUX_SERVICE_NAME]))?;
            println!("{}", unit_path.display());
        }
        "macos" => {
            let plist_path = mac_plist_path()?;
            if let Some(parent) = plist_path.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(&plist_path, render_macos_plist(&exe, &paths.config_path))?;
            let domain = launchctl_domain();
            let _ = Command::new("launchctl")
                .args(["bootout", &domain, &plist_path.display().to_string()])
                .status();
            run(Command::new("launchctl").args([
                "bootstrap",
                &domain,
                &plist_path.display().to_string(),
            ]))?;
            run(Command::new("launchctl").args(["enable", &format!("{}/{}", domain, MAC_LABEL)]))?;
            run(Command::new("launchctl").args([
                "kickstart",
                "-k",
                &format!("{}/{}", domain, MAC_LABEL),
            ]))?;
            println!("{}", plist_path.display());
        }
        other => bail!("unsupported OS for service install: {other}"),
    }
    Ok(())
}

fn uninstall(paths: &AppPaths) -> Result<()> {
    match env::consts::OS {
        "linux" => {
            let unit_path = linux_unit_path()?;
            let _ = Command::new("systemctl")
                .args(["--user", "disable", "--now", LINUX_SERVICE_NAME])
                .status();
            if unit_path.exists() {
                fs::remove_file(&unit_path)?;
            }
            let _ = run(Command::new("systemctl").args(["--user", "daemon-reload"]));
        }
        "macos" => {
            let plist_path = mac_plist_path()?;
            let domain = launchctl_domain();
            let _ = Command::new("launchctl")
                .args(["bootout", &domain, &plist_path.display().to_string()])
                .status();
            if plist_path.exists() {
                fs::remove_file(plist_path)?;
            }
        }
        other => bail!("unsupported OS for service uninstall: {other}"),
    }
    let _ = paths;
    Ok(())
}

fn run_platform_action(paths: &AppPaths, action: PlatformAction) -> Result<()> {
    let _ = paths;
    match env::consts::OS {
        "linux" => {
            let mut command = Command::new("systemctl");
            command.arg("--user");
            match action {
                PlatformAction::Start => {
                    command.args(["start", LINUX_SERVICE_NAME]);
                }
                PlatformAction::Stop => {
                    command.args(["stop", LINUX_SERVICE_NAME]);
                }
                PlatformAction::Restart => {
                    command.args(["restart", LINUX_SERVICE_NAME]);
                }
                PlatformAction::Reload => {
                    command.args(["reload", LINUX_SERVICE_NAME]);
                }
                PlatformAction::Status => {
                    command.args(["status", "--no-pager", LINUX_SERVICE_NAME]);
                }
            }
            run(&mut command)
        }
        "macos" => {
            let domain = launchctl_domain();
            let active_label = match action {
                PlatformAction::Start => MAC_LABEL.to_string(),
                _ => detect_active_macos_label().unwrap_or_else(|| MAC_LABEL.to_string()),
            };
            let label = format!("{}/{}", domain, active_label);
            let mut command = Command::new("launchctl");
            match action {
                PlatformAction::Start => {
                    command.args(["kickstart", "-k", &label]);
                }
                PlatformAction::Stop => {
                    command.args(["bootout", &label]);
                }
                PlatformAction::Restart => {
                    command.args(["kickstart", "-k", &label]);
                }
                PlatformAction::Reload => {
                    command.args(["kill", "HUP", &label]);
                }
                PlatformAction::Status => {
                    command.args(["print", &label]);
                }
            }
            run(&mut command)
        }
        other => bail!("unsupported OS for service commands: {other}"),
    }
}

fn print_units(paths: &AppPaths) -> Result<()> {
    let exe = current_exe()?;
    match env::consts::OS {
        "linux" => {
            println!("{}", render_linux_unit(&exe, &paths.config_path));
        }
        "macos" => {
            println!("{}", render_macos_plist(&exe, &paths.config_path));
        }
        other => bail!("unsupported OS for service print: {other}"),
    }
    Ok(())
}

fn render_linux_unit(exe: &Path, config: &Path) -> String {
    format!(
        "[Unit]\nDescription=Hooyodex Telegram Gateway\nAfter=network-online.target\nWants=network-online.target\n\n[Service]\nType=simple\nExecStart={} --config {} run\nExecReload=/bin/kill -HUP $MAINPID\nRestart=on-failure\nRestartSec=3\nTimeoutStopSec=20\nEnvironment=RUST_LOG=hooyodex=info\nEnvironment=PATH={}\nWorkingDirectory={}\n\n[Install]\nWantedBy=default.target\n",
        shell_escape(exe),
        shell_escape(config),
        service_path_env(),
        shell_escape(
            &env::var_os("HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("."))
        ),
    )
}

fn render_macos_plist(exe: &Path, config: &Path) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\">\n<dict>\n  <key>Label</key>\n  <string>{}</string>\n  <key>ProgramArguments</key>\n  <array>\n    <string>{}</string>\n    <string>--config</string>\n    <string>{}</string>\n    <string>run</string>\n  </array>\n  <key>RunAtLoad</key>\n  <true/>\n  <key>KeepAlive</key>\n  <true/>\n  <key>WorkingDirectory</key>\n  <string>{}</string>\n  <key>EnvironmentVariables</key>\n  <dict>\n    <key>RUST_LOG</key>\n    <string>hooyodex=info</string>\n    <key>PATH</key>\n    <string>{}</string>\n  </dict>\n  <key>StandardOutPath</key>\n  <string>{}</string>\n  <key>StandardErrorPath</key>\n  <string>{}</string>\n</dict>\n</plist>\n",
        MAC_LABEL,
        xml_escape(exe),
        xml_escape(config),
        xml_escape(
            &env::var_os("HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("."))
        ),
        service_path_env(),
        xml_escape(&default_log_dir().join("hooyodex.out.log")),
        xml_escape(&default_log_dir().join("hooyodex.err.log")),
    )
}

fn current_exe() -> Result<PathBuf> {
    env::current_exe().context("failed to determine current executable path")
}

fn linux_unit_path() -> Result<PathBuf> {
    dirs::config_dir()
        .map(|dir| dir.join("systemd").join("user").join(LINUX_SERVICE_NAME))
        .context("failed to determine ~/.config for systemd user unit")
}

fn mac_plist_path() -> Result<PathBuf> {
    dirs::home_dir()
        .map(|dir| {
            dir.join("Library")
                .join("LaunchAgents")
                .join(format!("{MAC_LABEL}.plist"))
        })
        .context("failed to determine ~/Library/LaunchAgents")
}

fn launchctl_domain() -> String {
    let uid = nix_like_uid();
    format!("gui/{uid}")
}

fn detect_active_macos_label() -> Option<String> {
    let domain = launchctl_domain();
    for label in [MAC_LABEL, MAC_ALT_LABEL] {
        let Ok(output) = Command::new("launchctl")
            .args(["print", &format!("{domain}/{label}")])
            .output()
        else {
            continue;
        };
        if output.status.success() {
            return Some(label.to_string());
        }
    }
    None
}

fn nix_like_uid() -> String {
    if let Ok(output) = Command::new("id").arg("-u").output() {
        return String::from_utf8_lossy(&output.stdout).trim().to_string();
    }
    "0".to_string()
}

fn default_log_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("Library")
        .join("Logs")
}

fn service_path_env() -> &'static str {
    "/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin"
}

fn run(command: &mut Command) -> Result<()> {
    let status = command
        .status()
        .with_context(|| format!("failed to run {:?}", command))?;
    if !status.success() {
        bail!("command {:?} failed with status {}", command, status);
    }
    Ok(())
}

fn shell_escape(path: &Path) -> String {
    path.display().to_string()
}

fn xml_escape(path: &Path) -> String {
    path.display()
        .to_string()
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}
