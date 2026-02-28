use crate::error::{KexshError, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Serialize, Deserialize)]
pub struct Credential {
    pub token: String,
    pub server_url: String,
}

pub fn credential_path() -> PathBuf {
    std::env::var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_else(|_| "~".into());
            PathBuf::from(home).join(".config")
        })
        .join("kexsh/credentials.json")
}

pub fn load() -> Result<Credential> {
    let path = credential_path();
    let content = std::fs::read_to_string(&path)
        .map_err(|_| KexshError::Config("not logged in — run `kexsh login`".into()))?;
    serde_json::from_str(&content)
        .map_err(|e| KexshError::Config(format!("invalid credentials: {e}")))
}

pub fn save(cred: &Credential) -> Result<()> {
    let path = credential_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| KexshError::Config(format!("cannot create config dir: {e}")))?;
    }
    let json = serde_json::to_string_pretty(cred)
        .map_err(|e| KexshError::Config(format!("serialize error: {e}")))?;
    std::fs::write(&path, &json)
        .map_err(|e| KexshError::Config(format!("cannot write credentials: {e}")))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
            .map_err(|e| KexshError::Config(format!("cannot set permissions: {e}")))?;
    }
    Ok(())
}

pub fn remove() -> Result<()> {
    let path = credential_path();
    if path.exists() {
        std::fs::remove_file(&path)
            .map_err(|e| KexshError::Config(format!("cannot remove credentials: {e}")))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;

    #[test]
    fn save_load_remove_cycle() {
        let dir = tempfile::tempdir().unwrap();
        // SAFETY: test runs serially, no other threads reading env
        unsafe { env::set_var("XDG_CONFIG_HOME", dir.path()) };

        let cred = Credential {
            token: "test-token".into(),
            server_url: "https://app.kex.sh".into(),
        };
        save(&cred).unwrap();
        let loaded = load().unwrap();
        assert_eq!(loaded.token, "test-token");

        remove().unwrap();
        assert!(load().is_err());
    }
}
