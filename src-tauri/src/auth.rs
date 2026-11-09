use std::time::{Duration, Instant};

use reqwest::{Client, StatusCode, Url};
use serde::Deserialize;

pub const KEYCHAIN_SERVICE: &str = "com.bryan.ncsync";
const LOGIN_TIMEOUT: Duration = Duration::from_secs(20 * 60);

/// Validates and normalizes a server URL: https only (http for localhost), no trailing slash.
pub fn normalize_server_url(input: &str) -> Result<String, String> {
    let s = input.trim().trim_end_matches('/');
    let url = Url::parse(s).map_err(|_| "Invalid server URL".to_string())?;
    let host = url.host_str().ok_or("Invalid server URL")?;
    match url.scheme() {
        "https" => {}
        "http" if host == "localhost" || host == "127.0.0.1" => {}
        _ => return Err("Server URL must start with https://".into()),
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err("Invalid server URL".into());
    }
    Ok(s.to_string())
}

pub fn keychain_account(login_name: &str, server_url: &str) -> String {
    let host = Url::parse(server_url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_string))
        .unwrap_or_default();
    format!("{login_name}@{host}")
}

fn entry(login_name: &str, server_url: &str) -> keyring::Result<keyring::Entry> {
    keyring::Entry::new(KEYCHAIN_SERVICE, &keychain_account(login_name, server_url))
}

pub fn get_password(login_name: &str, server_url: &str) -> keyring::Result<String> {
    entry(login_name, server_url)?.get_password()
}

pub fn set_password(login_name: &str, server_url: &str, pw: &str) -> keyring::Result<()> {
    entry(login_name, server_url)?.set_password(pw)
}

pub fn delete_password(login_name: &str, server_url: &str) -> keyring::Result<()> {
    match entry(login_name, server_url)?.delete_credential() {
        Err(keyring::Error::NoEntry) => Ok(()),
        r => r,
    }
}

#[derive(Deserialize)]
struct FlowStart {
    poll: FlowPoll,
    login: String,
}

#[derive(Deserialize)]
struct FlowPoll {
    token: String,
    endpoint: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct FlowResult {
    login_name: String,
    app_password: String,
}

pub struct Credentials {
    pub login_name: String,
    pub app_password: String,
    pub user_id: String,
}

/// Runs Login Flow v2. `open` is called with the browser URL. Cancel by aborting the task.
pub async fn login_flow(
    client: &Client,
    server: &str,
    open: impl Fn(&str) -> Result<(), String>,
) -> Result<Credentials, String> {
    let start: FlowStart = client
        .post(format!("{server}/index.php/login/v2"))
        .send()
        .await
        .map_err(|e| format!("Cannot reach server: {e}"))?
        .error_for_status()
        .map_err(|e| format!("Login not available: {e}"))?
        .json()
        .await
        .map_err(|_| "Not a Nextcloud server?".to_string())?;
    open(&start.login)?;

    let began = Instant::now();
    let res: FlowResult = loop {
        tokio::time::sleep(Duration::from_secs(2)).await;
        if began.elapsed() > LOGIN_TIMEOUT {
            return Err("Login timed out".into());
        }
        let resp = match client
            .post(&start.poll.endpoint)
            .form(&[("token", &start.poll.token)])
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!("login poll failed: {e}");
                continue;
            }
        };
        if resp.status() == StatusCode::OK {
            break resp
                .json()
                .await
                .map_err(|e| format!("Bad login response: {e}"))?;
        }
    };

    let user_id = fetch_user_id(client, server, &res.login_name, &res.app_password).await?;
    Ok(Credentials {
        login_name: res.login_name,
        app_password: res.app_password,
        user_id,
    })
}

async fn fetch_user_id(
    client: &Client,
    server: &str,
    login: &str,
    pw: &str,
) -> Result<String, String> {
    let v: serde_json::Value = client
        .get(format!("{server}/ocs/v2.php/cloud/user?format=json"))
        .basic_auth(login, Some(pw))
        .header("OCS-APIRequest", "true")
        .send()
        .await
        .map_err(|e| format!("Cannot fetch user: {e}"))?
        .error_for_status()
        .map_err(|e| format!("Cannot fetch user: {e}"))?
        .json()
        .await
        .map_err(|e| format!("Cannot fetch user: {e}"))?;
    v["ocs"]["data"]["id"]
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| "User id missing in server response".into())
}

/// Revokes the app password on the server. Errors are ignored.
pub async fn revoke(client: &Client, server: &str, login: &str, pw: &str) {
    let r = client
        .delete(format!("{server}/ocs/v2.php/core/apppassword"))
        .basic_auth(login, Some(pw))
        .header("OCS-APIRequest", "true")
        .send()
        .await;
    if let Err(e) = r {
        tracing::warn!("app password revoke failed: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_urls() {
        assert_eq!(
            normalize_server_url("https://cloud.x.com/").unwrap(),
            "https://cloud.x.com"
        );
        assert_eq!(
            normalize_server_url(" https://x.com/nc// ").unwrap(),
            "https://x.com/nc"
        );
        assert_eq!(
            normalize_server_url("http://localhost:8080/").unwrap(),
            "http://localhost:8080"
        );
        assert!(normalize_server_url("http://127.0.0.1").is_ok());
        assert!(normalize_server_url("http://cloud.x.com").is_err());
        assert!(normalize_server_url("cloud.x.com").is_err());
        assert!(normalize_server_url("ftp://x.com").is_err());
    }

    #[test]
    fn account() {
        assert_eq!(
            keychain_account("bob", "https://x.com:8443/nc"),
            "bob@x.com"
        );
    }
}
