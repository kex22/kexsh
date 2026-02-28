use std::collections::HashMap;

use serde_json::Value;

use crate::error::{KexshError, Result};
use crate::ipc::message::ViewInfo;

pub struct View {
    pub id: String,
    pub name: Option<String>,
    pub terminal_ids: Vec<String>,
    pub layout: Value,
    pub focused: String,
    pub created_at: String,
}

pub struct ViewManager {
    views: HashMap<String, View>,
}

impl Default for ViewManager {
    fn default() -> Self {
        Self::new()
    }
}

impl ViewManager {
    pub fn new() -> Self {
        Self {
            views: HashMap::new(),
        }
    }

    pub fn create(&mut self, name: Option<String>, terminal_id: String) -> String {
        let mut id = uuid::Uuid::new_v4().to_string()[..8].to_string();
        while self.views.contains_key(&id) {
            id = uuid::Uuid::new_v4().to_string()[..8].to_string();
        }
        let view = View {
            id: id.clone(),
            name,
            terminal_ids: vec![terminal_id.clone()],
            layout: Value::Null,
            focused: terminal_id,
            created_at: chrono_now(),
        };
        self.views.insert(id.clone(), view);
        id
    }

    pub fn delete(&mut self, id_or_name: &str) -> Result<()> {
        let id = self
            .resolve_id(id_or_name)
            .ok_or_else(|| KexshError::Server(format!("view not found: {id_or_name}")))?;
        self.views.remove(&id);
        Ok(())
    }

    pub fn list(&self) -> Vec<ViewInfo> {
        let mut views: Vec<ViewInfo> = self
            .views
            .values()
            .map(|v| ViewInfo {
                id: v.id.clone(),
                name: v.name.clone(),
                terminal_ids: v.terminal_ids.clone(),
                created_at: v.created_at.clone(),
            })
            .collect();
        views.sort_by(|a, b| a.created_at.cmp(&b.created_at));
        views
    }

    pub fn list_full(&self) -> Vec<&View> {
        self.views.values().collect()
    }

    pub fn get(&self, id_or_name: &str) -> Option<&View> {
        self.resolve_id(id_or_name)
            .and_then(|id| self.views.get(&id))
    }

    pub fn resolve_id(&self, id_or_name: &str) -> Option<String> {
        if self.views.contains_key(id_or_name) {
            return Some(id_or_name.to_string());
        }
        self.views
            .values()
            .find(|v| v.name.as_deref() == Some(id_or_name))
            .map(|v| v.id.clone())
    }

    pub fn update_layout(&mut self, id: &str, layout: Value, focused: String) {
        if let Some(v) = self.views.get_mut(id) {
            v.layout = layout;
            v.focused = focused;
        }
    }

    pub fn add_terminal(&mut self, view_id: &str, terminal_id: &str) {
        if let Some(v) = self.views.get_mut(view_id)
            && !v.terminal_ids.contains(&terminal_id.to_string())
        {
            v.terminal_ids.push(terminal_id.to_string());
        }
    }

    pub fn remove_terminal_from_view(&mut self, view_id: &str, terminal_id: &str) {
        if let Some(v) = self.views.get_mut(view_id) {
            v.terminal_ids.retain(|t| t != terminal_id);
        }
    }

    pub fn remove_terminal(&mut self, terminal_id: &str) {
        for v in self.views.values_mut() {
            v.terminal_ids.retain(|t| t != terminal_id);
        }
    }
}

fn chrono_now() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as libc::time_t;
    // SAFETY: `localtime_r` is reentrant and writes to our stack-allocated `tm`.
    // `secs` is a valid timestamp derived from SystemTime.
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
    fn create_returns_8char_id() {
        let mut mgr = ViewManager::new();
        let id = mgr.create(None, "t1".into());
        assert_eq!(id.len(), 8);
    }

    #[test]
    fn create_with_name_and_resolve() {
        let mut mgr = ViewManager::new();
        let id = mgr.create(Some("dev".into()), "t1".into());
        assert_eq!(mgr.resolve_id("dev"), Some(id.clone()));
        assert_eq!(mgr.resolve_id(&id), Some(id));
    }

    #[test]
    fn list_and_delete() {
        let mut mgr = ViewManager::new();
        let id = mgr.create(None, "t1".into());
        assert_eq!(mgr.list().len(), 1);
        mgr.delete(&id).unwrap();
        assert_eq!(mgr.list().len(), 0);
    }

    #[test]
    fn delete_nonexistent_returns_error() {
        let mut mgr = ViewManager::new();
        assert!(mgr.delete("nonexistent").is_err());
    }

    #[test]
    fn get_returns_view_with_terminal() {
        let mut mgr = ViewManager::new();
        let id = mgr.create(Some("work".into()), "t1".into());
        let view = mgr.get("work").unwrap();
        assert_eq!(view.id, id);
        assert_eq!(view.terminal_ids, vec!["t1"]);
    }

    #[test]
    fn add_terminal_deduplicates() {
        let mut mgr = ViewManager::new();
        let id = mgr.create(None, "t1".into());
        mgr.add_terminal(&id, "t2");
        mgr.add_terminal(&id, "t2");
        assert_eq!(mgr.get(&id).unwrap().terminal_ids, vec!["t1", "t2"]);
    }

    #[test]
    fn remove_terminal_from_all_views() {
        let mut mgr = ViewManager::new();
        let v1 = mgr.create(None, "t1".into());
        let v2 = mgr.create(None, "t1".into());
        mgr.remove_terminal("t1");
        assert!(mgr.get(&v1).unwrap().terminal_ids.is_empty());
        assert!(mgr.get(&v2).unwrap().terminal_ids.is_empty());
    }

    #[test]
    fn shared_terminal_survives_view_delete() {
        let mut mgr = ViewManager::new();
        let v1 = mgr.create(Some("dev".into()), "t1".into());
        let v2 = mgr.create(Some("ops".into()), "t1".into());
        mgr.add_terminal(&v2, "t2");
        // Delete v1 — t1 should still be in v2
        mgr.delete(&v1).unwrap();
        let view2 = mgr.get(&v2).unwrap();
        assert_eq!(view2.terminal_ids, vec!["t1", "t2"]);
    }

    #[test]
    fn update_layout_stores_value() {
        let mut mgr = ViewManager::new();
        let id = mgr.create(None, "t1".into());
        let layout = serde_json::json!({"type": "leaf", "terminal_id": "t1"});
        mgr.update_layout(&id, layout.clone(), "t1".into());
        assert_eq!(mgr.get(&id).unwrap().layout, layout);
        assert_eq!(mgr.get(&id).unwrap().focused, "t1");
    }

    #[test]
    fn remove_terminal_from_specific_view() {
        let mut mgr = ViewManager::new();
        let v1 = mgr.create(None, "t1".into());
        let v2 = mgr.create(None, "t1".into());
        mgr.add_terminal(&v1, "t2");
        mgr.remove_terminal_from_view(&v1, "t2");
        assert_eq!(mgr.get(&v1).unwrap().terminal_ids, vec!["t1"]);
        // v2 unaffected
        assert_eq!(mgr.get(&v2).unwrap().terminal_ids, vec!["t1"]);
    }
}
