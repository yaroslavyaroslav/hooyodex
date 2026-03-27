use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PersistentState {
    pub orchestrator_thread_id: Option<String>,
}

impl PersistentState {
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let raw = fs::read_to_string(path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        let state = serde_json::from_str(&raw)
            .with_context(|| format!("failed to parse {}", path.display()))?;
        Ok(state)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        let tmp = path.with_extension("json.tmp");
        let raw = serde_json::to_vec_pretty(self)?;
        fs::write(&tmp, raw).with_context(|| format!("failed to write {}", tmp.display()))?;
        fs::rename(&tmp, path)
            .with_context(|| format!("failed to move {} to {}", tmp.display(), path.display()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saves_and_loads_state() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("sessions.json");
        let state = PersistentState {
            orchestrator_thread_id: Some("orch".to_string()),
        };
        state.save(&path).expect("save");
        let loaded = PersistentState::load(&path).expect("load");
        assert_eq!(loaded.orchestrator_thread_id.as_deref(), Some("orch"));
    }
}
