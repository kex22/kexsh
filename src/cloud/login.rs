use crate::credential::{self, Credential};
use crate::error::{KexshError, Result};
use serde::Deserialize;

const CLIENT_ID: &str = "kexsh-cli";

#[derive(Deserialize)]
struct DeviceCodeResponse {
    device_code: String,
    user_code: String,
    verification_uri: String,
    interval: u64,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
}

#[derive(Deserialize)]
struct ErrorResponse {
    error: Option<String>,
}

pub async fn login(server_url: &str) -> Result<()> {
    let client = reqwest::Client::new();

    // Request device code
    let res: DeviceCodeResponse = client
        .post(format!("{server_url}/api/auth/device/code"))
        .json(&serde_json::json!({ "client_id": CLIENT_ID }))
        .send()
        .await
        .map_err(|e| KexshError::Server(format!("request failed: {e}")))?
        .json()
        .await
        .map_err(|e| KexshError::Server(format!("invalid response: {e}")))?;

    // Show user code and open browser
    println!("Open in browser: {}", res.verification_uri);
    println!("Enter code: {}", res.user_code);
    let _ = open::that(&res.verification_uri);

    // Poll for token
    let mut interval = std::time::Duration::from_secs(res.interval);
    let token = loop {
        tokio::time::sleep(interval).await;
        let poll = client
            .post(format!("{server_url}/api/auth/device/token"))
            .json(&serde_json::json!({
                "grant_type": "urn:ietf:params:oauth:grant-type:device_code",
                "device_code": res.device_code,
                "client_id": CLIENT_ID,
            }))
            .send()
            .await
            .map_err(|e| KexshError::Server(format!("poll failed: {e}")))?;

        if poll.status().is_success() {
            let t: TokenResponse = poll
                .json()
                .await
                .map_err(|e| KexshError::Server(format!("invalid token response: {e}")))?;
            break t.access_token;
        }
        let err: ErrorResponse = poll.json().await.unwrap_or(ErrorResponse { error: None });
        match err.error.as_deref() {
            Some("expired_token") => {
                return Err(KexshError::Server(
                    "device code expired — run `kexsh login` again".into(),
                ));
            }
            Some("access_denied") => {
                return Err(KexshError::Server("authorization denied".into()));
            }
            Some("slow_down") => interval += std::time::Duration::from_secs(5),
            _ => {} // authorization_pending — keep polling
        }
    };

    // Save credential
    credential::save(&Credential {
        token: token.clone(),
        server_url: server_url.to_string(),
    })?;

    // Register device
    let hostname = hostname::get()
        .map(|h| h.to_string_lossy().to_string())
        .unwrap_or_else(|_| "unknown".into());
    let fingerprint = machine_uid::get().unwrap_or_else(|_| "unknown".into());

    let _ = client
        .post(format!("{server_url}/api/devices"))
        .header("Authorization", format!("Bearer {token}"))
        .json(&serde_json::json!({
            "name": hostname,
            "fingerprint": fingerprint,
        }))
        .send()
        .await;

    println!("Logged in successfully.");
    Ok(())
}

pub async fn logout() -> Result<()> {
    let cred = credential::load()?;
    let client = reqwest::Client::new();
    let _ = client
        .post(format!("{}/api/auth/sign-out", cred.server_url))
        .header("Authorization", format!("Bearer {}", cred.token))
        .send()
        .await;
    credential::remove()?;
    println!("Logged out.");
    Ok(())
}
