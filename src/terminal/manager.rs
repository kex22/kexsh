use std::collections::HashMap;

use crate::error::{KexshError, Result};
use crate::ipc::message::TerminalInfo;
use crate::terminal::pty::Pty;

pub struct Terminal {
    pub id: String,
    pub name: Option<String>,
    pub pty: Pty,
    pub created_at: String,
}

pub struct TerminalManager {
    terminals: HashMap<String, Terminal>,
}

impl Default for TerminalManager {
    fn default() -> Self {
        Self::new()
    }
}

impl TerminalManager {
    pub fn new() -> Self {
        Self {
            terminals: HashMap::new(),
        }
    }

    pub fn create(&mut self, name: Option<String>) -> Result<String> {
        let mut id = uuid::Uuid::new_v4().to_string()[..8].to_string();
        while self.terminals.contains_key(&id) {
            id = uuid::Uuid::new_v4().to_string()[..8].to_string();
        }
        let pty = Pty::spawn()?;
        let terminal = Terminal {
            id: id.clone(),
            name,
            pty,
            created_at: chrono_now(),
        };
        self.terminals.insert(id.clone(), terminal);
        Ok(id)
    }

    pub fn kill(&mut self, id_or_name: &str) -> Result<()> {
        let id = self
            .resolve_id(id_or_name)
            .ok_or_else(|| KexshError::Server(format!("terminal not found: {id_or_name}")))?;
        if let Some(mut t) = self.terminals.remove(&id) {
            let _ = t.pty.kill();
        }
        Ok(())
    }

    pub fn list(&self) -> Vec<TerminalInfo> {
        self.terminals
            .values()
            .map(|t| TerminalInfo {
                id: t.id.clone(),
                name: t.name.clone(),
                created_at: t.created_at.clone(),
            })
            .collect()
    }

    pub fn get(&self, id_or_name: &str) -> Option<&Terminal> {
        self.resolve_id(id_or_name)
            .and_then(|id| self.terminals.get(&id))
    }

    pub fn resolve_id(&self, id_or_name: &str) -> Option<String> {
        if self.terminals.contains_key(id_or_name) {
            return Some(id_or_name.to_string());
        }
        self.terminals
            .values()
            .find(|t| t.name.as_deref() == Some(id_or_name))
            .map(|t| t.id.clone())
    }
}

fn chrono_now() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as libc::time_t;
    unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&secs, &mut tm);
        format!(
            "{:04}-{:02}-{:02} {:02}:{:02}",
            tm.tm_year + 1900,
            tm.tm_mon + 1,
            tm.tm_mday,
            tm.tm_hour,
            tm.tm_min
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_returns_8char_hex_id() {
        let mut mgr = TerminalManager::new();
        let id = mgr.create(None).unwrap();
        assert_eq!(id.len(), 8);
        assert!(id.chars().all(|c| c.is_ascii_hexdigit() || c == '-'));
    }

    #[test]
    fn create_with_name_and_resolve() {
        let mut mgr = TerminalManager::new();
        let id = mgr.create(Some("dev".into())).unwrap();
        assert_eq!(mgr.resolve_id("dev"), Some(id.clone()));
        assert_eq!(mgr.resolve_id(&id), Some(id));
    }

    #[test]
    fn list_and_kill() {
        let mut mgr = TerminalManager::new();
        let id = mgr.create(None).unwrap();
        assert_eq!(mgr.list().len(), 1);
        mgr.kill(&id).unwrap();
        assert_eq!(mgr.list().len(), 0);
    }

    #[test]
    fn kill_nonexistent_returns_error() {
        let mut mgr = TerminalManager::new();
        assert!(mgr.kill("nonexistent").is_err());
    }
}
