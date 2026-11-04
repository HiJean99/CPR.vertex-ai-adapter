//! Google 官方 gcloud ADC 交互登录。授权码只写入 gcloud 子进程 stdin；
//! Refresh Token 仅由 gcloud 写入独立 ADC 目录，不进入 CPR 账号或插件日志。

use std::{
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::Duration,
};

use gateway_plugin_sdk::{ErrorCode, PluginFault};
use serde::Serialize;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, Command},
    sync::{Mutex, mpsc},
    time::{Instant, timeout},
};
use uuid::Uuid;

use crate::config::Config;

const LOGIN_URL_PREFIX: &str = "https://accounts.google.com/o/oauth2/auth?";
const LOGIN_URL_TIMEOUT: Duration = Duration::from_secs(45);
const LOGIN_PROCESS_TIMEOUT: Duration = Duration::from_secs(120);
const LOGIN_SESSION_TTL: Duration = Duration::from_secs(600);

pub type SharedLogin = Arc<Mutex<Option<PendingLogin>>>;

pub struct PendingLogin {
    pub session_id: String,
    pub pending_dir: PathBuf,
    pub created_at: Instant,
    child: Child,
    stdin: Option<ChildStdin>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LoginStart {
    pub session_id: String,
    pub authorization_url: String,
    pub expires_in_seconds: u64,
}

pub struct CompletedLogin {
    pub pending_dir: PathBuf,
    pub access_token: String,
}

pub fn shared_login() -> SharedLogin {
    Arc::new(Mutex::new(None))
}

pub async fn gcloud_available(config: &Config) -> bool {
    timeout(
        Duration::from_secs(8),
        gcloud_command(config).arg("--version").output(),
    )
    .await
    .is_ok_and(|result| result.is_ok_and(|output| output.status.success()))
}

pub async fn cleanup_stale_pending(config: &Config) -> Result<(), PluginFault> {
    let root = config.pending_root();
    if tokio::fs::symlink_metadata(&root).await.is_ok() {
        let metadata = tokio::fs::symlink_metadata(&root)
            .await
            .map_err(|_| fault("Cannot inspect pending Google login state."))?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(fault(
                "Google login pending path is not a regular directory.",
            ));
        }
        tokio::fs::remove_dir_all(&root)
            .await
            .map_err(|_| fault("Cannot clean stale Google login state."))?;
    }
    Ok(())
}

pub async fn start_login(config: &Config, shared: &SharedLogin) -> Result<LoginStart, PluginFault> {
    let mut slot = shared.lock().await;
    if slot.is_some() {
        return Err(conflict("A Google sign-in is already in progress."));
    }
    if config.adc_file().exists() {
        return Err(conflict(
            "A Google account is already connected. Disconnect it before switching accounts.",
        ));
    }

    ensure_private_dir(&config.data_dir()).await?;
    ensure_private_dir(&config.home_dir()).await?;
    ensure_private_dir(&config.pending_root()).await?;
    let session_id = Uuid::new_v4().simple().to_string();
    let pending_dir = config.pending_dir(&session_id);
    ensure_private_dir(&pending_dir).await?;

    let mut child = gcloud_command(config)
        .args([
            "auth",
            "application-default",
            "login",
            "--no-launch-browser",
            "--scopes=https://www.googleapis.com/auth/cloud-platform,https://www.googleapis.com/auth/userinfo.email,openid",
        ])
        .env("CLOUDSDK_CONFIG", &pending_dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|_| {
            fault(
                "Google Cloud CLI is unavailable. Install the bundled Google Cloud CLI and retry.",
            )
        })?;

    let stdin = child.stdin.take().ok_or_else(login_process_error)?;
    let stdout = child.stdout.take().ok_or_else(login_process_error)?;
    let stderr = child.stderr.take().ok_or_else(login_process_error)?;
    let (sender, mut receiver) = mpsc::unbounded_channel::<String>();
    tokio::spawn(read_lines(stdout, sender.clone()));
    tokio::spawn(read_lines(stderr, sender));

    let authorization_url = match timeout(LOGIN_URL_TIMEOUT, async {
        while let Some(line) = receiver.recv().await {
            if let Some(url) = extract_authorization_url(&line) {
                return Some(url);
            }
        }
        None
    })
    .await
    {
        Ok(Some(url)) => url,
        _ => {
            let _ = child.kill().await;
            let _ = tokio::fs::remove_dir_all(&pending_dir).await;
            return Err(fault(
                "gcloud did not produce a Google authorization URL. Check the CLI installation and network.",
            ));
        }
    };

    let result = LoginStart {
        session_id: session_id.clone(),
        authorization_url: authorization_url.clone(),
        expires_in_seconds: LOGIN_SESSION_TTL.as_secs(),
    };
    *slot = Some(PendingLogin {
        session_id,
        pending_dir,
        created_at: Instant::now(),
        child,
        stdin: Some(stdin),
    });
    Ok(result)
}

pub async fn complete_login(
    config: &Config,
    shared: &SharedLogin,
    session_id: &str,
    authorization_code: &str,
) -> Result<CompletedLogin, PluginFault> {
    if !(8..=4096).contains(&authorization_code.len())
        || !authorization_code.is_ascii()
        || authorization_code
            .bytes()
            .any(|byte| byte.is_ascii_whitespace() || byte.is_ascii_control())
    {
        return Err(PluginFault::new(
            ErrorCode::InvalidInput,
            "The Google authorization code is invalid.",
        ));
    }

    let mut pending = {
        let mut slot = shared.lock().await;
        let Some(pending) = slot.take() else {
            return Err(conflict(
                "No Google sign-in is waiting for an authorization code.",
            ));
        };
        if pending.session_id != session_id {
            *slot = Some(pending);
            return Err(conflict("The Google sign-in session does not match."));
        }
        pending
    };
    if pending.created_at.elapsed() > LOGIN_SESSION_TTL {
        let _ = pending.child.kill().await;
        let _ = tokio::fs::remove_dir_all(&pending.pending_dir).await;
        return Err(fault("Google sign-in expired. Start a new sign-in."));
    }
    let Some(mut stdin) = pending.stdin.take() else {
        let _ = pending.child.kill().await;
        return Err(login_process_error());
    };
    if stdin
        .write_all(authorization_code.as_bytes())
        .await
        .is_err()
        || stdin.write_all(b"\n").await.is_err()
        || stdin.flush().await.is_err()
    {
        let _ = pending.child.kill().await;
        let _ = tokio::fs::remove_dir_all(&pending.pending_dir).await;
        return Err(fault(
            "gcloud could not accept the authorization code. Start a new sign-in.",
        ));
    }
    drop(stdin);

    let status = match timeout(LOGIN_PROCESS_TIMEOUT, pending.child.wait()).await {
        Ok(Ok(status)) => status,
        _ => {
            let _ = pending.child.kill().await;
            let _ = tokio::fs::remove_dir_all(&pending.pending_dir).await;
            return Err(fault(
                "Google authorization did not finish. Start a new sign-in.",
            ));
        }
    };
    let adc_file = pending
        .pending_dir
        .join("application_default_credentials.json");
    if !status.success() || !valid_adc_file(&adc_file).await {
        let _ = tokio::fs::remove_dir_all(&pending.pending_dir).await;
        return Err(fault(
            "Google authorization failed or did not create Application Default Credentials.",
        ));
    }
    set_private_file(&adc_file).await?;
    let access_token = match access_token_from_dir(config, &pending.pending_dir).await {
        Ok(token) => token,
        Err(error) => {
            let _ = tokio::fs::remove_dir_all(&pending.pending_dir).await;
            return Err(error);
        }
    };
    Ok(CompletedLogin {
        pending_dir: pending.pending_dir,
        access_token,
    })
}

pub async fn commit_login(config: &Config, pending_dir: &Path) -> Result<(), PluginFault> {
    let active = config.gcloud_config_dir();
    if active.exists() {
        return Err(conflict(
            "Google ADC storage already exists. Disconnect the current account first.",
        ));
    }
    tokio::fs::rename(pending_dir, &active)
        .await
        .map_err(|_| fault("Google ADC could not be saved."))?;
    ensure_private_dir(&active).await?;
    set_private_file(&config.adc_file()).await?;
    Ok(())
}

pub async fn cancel_login(
    shared: &SharedLogin,
    session_id: Option<&str>,
) -> Result<(), PluginFault> {
    let mut slot = shared.lock().await;
    let Some(mut pending) = slot.take() else {
        return Ok(());
    };
    if session_id.is_some_and(|expected| expected != pending.session_id) {
        *slot = Some(pending);
        return Err(conflict("The Google sign-in session does not match."));
    }
    let _ = pending.child.kill().await;
    let _ = tokio::fs::remove_dir_all(&pending.pending_dir).await;
    Ok(())
}

pub async fn access_token(config: &Config) -> Result<String, PluginFault> {
    access_token_from_dir(config, &config.gcloud_config_dir()).await
}

async fn access_token_from_dir(config: &Config, directory: &Path) -> Result<String, PluginFault> {
    let output = timeout(
        Duration::from_secs(30),
        gcloud_command(config)
            .args(["auth", "application-default", "print-access-token"])
            .env("CLOUDSDK_CONFIG", directory)
            .output(),
    )
    .await
    .map_err(|_| fault("Google ADC token refresh timed out."))?
    .map_err(|_| fault("Google Cloud CLI is unavailable in the CPR runtime."))?;
    if !output.status.success() {
        return Err(fault(
            "Google ADC token refresh failed. Reconnect the Google account.",
        ));
    }
    let token = String::from_utf8(output.stdout)
        .map_err(|_| fault("Google ADC returned an invalid access token."))?;
    let token = token.trim();
    if token.len() < 20
        || token.len() > 8192
        || token.bytes().any(|byte| byte.is_ascii_whitespace())
    {
        return Err(fault("Google ADC returned an invalid access token."));
    }
    Ok(token.to_owned())
}

pub async fn disconnect(config: &Config, shared: &SharedLogin) -> Result<(), PluginFault> {
    let _ = cancel_login(shared, None).await;
    let active = config.gcloud_config_dir();
    if tokio::fs::symlink_metadata(&active).await.is_ok() {
        let metadata = tokio::fs::symlink_metadata(&active)
            .await
            .map_err(|_| fault("Google ADC storage could not be inspected."))?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(fault("Google ADC storage is not a regular directory."));
        }
        tokio::fs::remove_dir_all(&active)
            .await
            .map_err(|_| fault("Google ADC credentials could not be removed."))?;
    }
    Ok(())
}

async fn valid_adc_file(path: &Path) -> bool {
    let Ok(metadata) = tokio::fs::metadata(path).await else {
        return false;
    };
    if !metadata.is_file() || metadata.len() == 0 || metadata.len() > 64 * 1024 {
        return false;
    }
    let Ok(bytes) = tokio::fs::read(path).await else {
        return false;
    };
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        return false;
    };
    value.get("type").and_then(serde_json::Value::as_str) == Some("authorized_user")
        && value
            .get("refresh_token")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|token| !token.is_empty())
}

fn gcloud_command(config: &Config) -> Command {
    let mut command = Command::new(&config.gcloud_path);
    command
        .env("HOME", config.home_dir())
        .env("CLOUDSDK_COMPONENT_MANAGER_DISABLE_UPDATE_CHECK", "true")
        .env("CLOUDSDK_CORE_DISABLE_USAGE_REPORTING", "true")
        .env("CLOUDSDK_PYTHON_SITEPACKAGES", "0");
    command
}

async fn ensure_private_dir(path: &Path) -> Result<(), PluginFault> {
    tokio::fs::create_dir_all(path)
        .await
        .map_err(|_| fault("Vertex AI persistent storage is not writable."))?;
    let metadata = tokio::fs::symlink_metadata(path)
        .await
        .map_err(|_| fault("Vertex AI data directory is unavailable."))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(fault("Vertex AI data path must be a real directory."));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
            .await
            .map_err(|_| fault("Vertex AI data directory permissions could not be set."))?;
    }
    Ok(())
}

async fn set_private_file(path: &Path) -> Result<(), PluginFault> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .await
            .map_err(|_| fault("Google ADC file permissions could not be set."))?;
    }
    Ok(())
}

async fn read_lines<R>(reader: R, sender: mpsc::UnboundedSender<String>)
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    let mut reader = BufReader::new(reader);
    loop {
        let mut line = String::new();
        match reader.read_line(&mut line).await {
            Ok(0) | Err(_) => break,
            Ok(_) => {
                // Keep draining stdout/stderr after the URL consumer returns;
                // otherwise gcloud can block when its pipe buffer fills.
                let _ = sender.send(line);
            }
        }
    }
}

fn extract_authorization_url(line: &str) -> Option<String> {
    let start = line.find(LOGIN_URL_PREFIX)?;
    let candidate = line[start..].split_whitespace().next()?;
    if candidate.len() > 8192 || !candidate.starts_with(LOGIN_URL_PREFIX) {
        return None;
    }
    Some(candidate.trim_end_matches(['.', ',', ')']).to_owned())
}

fn login_process_error() -> PluginFault {
    fault("Google Cloud CLI could not start the authorization flow.")
}
fn conflict(message: &'static str) -> PluginFault {
    PluginFault::new(ErrorCode::Conflict, message)
}
fn fault(message: &'static str) -> PluginFault {
    PluginFault::new(ErrorCode::Rejected, message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_only_google_authorization_url() {
        let line = format!("Visit {LOGIN_URL_PREFIX}client_id=example&scope=cloud-platform\n");
        assert_eq!(
            extract_authorization_url(&line).as_deref(),
            Some(
                "https://accounts.google.com/o/oauth2/auth?client_id=example&scope=cloud-platform"
            )
        );
        assert!(extract_authorization_url("https://evil.example/login").is_none());
    }

    #[tokio::test]
    async fn fake_gcloud_roundtrip_keeps_code_out_of_adc_and_returns_token() {
        use std::os::unix::fs::PermissionsExt;
        let root =
            std::env::temp_dir().join(format!("vertex-auth-test-{}", Uuid::new_v4().simple()));
        tokio::fs::create_dir_all(&root).await.unwrap();
        let script = root.join("gcloud-test.sh");
        let content = r##"#!/bin/sh
if [ "$1" = "--version" ]; then echo test; exit 0; fi
if [ "$3" = "login" ]; then
  echo 'https://accounts.google.com/o/oauth2/auth?client_id=test&scope=cloud-platform' >&2
  IFS= read -r code
  mkdir -p "$CLOUDSDK_CONFIG"
  printf '{"type":"authorized_user","refresh_token":"fake-refresh-token"}' > "$CLOUDSDK_CONFIG/application_default_credentials.json"
  exit 0
fi
if [ "$3" = "print-access-token" ]; then echo fake-access-token-123456789012345; exit 0; fi
exit 1
"##;
        tokio::fs::write(&script, content).await.unwrap();
        let mut permissions = tokio::fs::metadata(&script).await.unwrap().permissions();
        permissions.set_mode(0o700);
        tokio::fs::set_permissions(&script, permissions)
            .await
            .unwrap();
        let config = Config {
            gcloud_path: script.display().to_string(),
            data_dir: root.join("data").display().to_string(),
        };
        let shared = shared_login();
        let start = start_login(&config, &shared).await.unwrap();
        assert!(start.authorization_url.starts_with(LOGIN_URL_PREFIX));
        let completed = complete_login(&config, &shared, &start.session_id, "fake-code-123456789")
            .await
            .unwrap();
        assert!(completed.access_token.starts_with("fake-access-token"));
        commit_login(&config, &completed.pending_dir).await.unwrap();
        assert!(config.adc_file().exists());
        disconnect(&config, &shared).await.unwrap();
        tokio::fs::remove_dir_all(root).await.unwrap();
    }
}
