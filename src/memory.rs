//! The local mirror of one Claude's subjective Daycare memories.
//!
//! The server remains the source used by the hub. At homecoming the companion
//! replaces this snapshot atomically, so an ordinary local Claude can answer
//! questions about Daycare without a network request and a failed refresh can
//! never erase the last good copy.

use crate::paths::{write_atomic, Layout};
use crate::platform::Memory;
use crate::{Error, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalMemoryMirror {
    pub identity_id: String,
    pub identity_name: String,
    pub synced_at: String,
    pub memories: Vec<Memory>,
}

impl LocalMemoryMirror {
    pub fn save(&self, layout: &Layout) -> Result<PathBuf> {
        layout.ensure_root()?;
        let path = layout.memory_file(&self.identity_id);
        let bytes = serde_json::to_vec_pretty(self)?;
        write_atomic(&path, &bytes, 0o600)?;
        Ok(path)
    }

    pub fn load(layout: &Layout, identity_id: &str) -> Result<Self> {
        let path = layout.memory_file(identity_id);
        let bytes = fs::read(&path).map_err(|error| {
            Error::new(format!(
                "no local Daycare memories for this Claude at {} ({error}); complete a visit while this computer is online first",
                path.display()
            ))
        })?;
        let mirror: LocalMemoryMirror = serde_json::from_slice(&bytes).map_err(|error| {
            Error::new(format!(
                "{} is not a memory mirror: {error}",
                path.display()
            ))
        })?;
        if mirror.identity_id != identity_id {
            return Err(Error::new(format!(
                "{} belongs to identity {}, not {}; refusing a misleading local memory result",
                path.display(),
                mirror.identity_id,
                identity_id
            )));
        }
        Ok(mirror)
    }

    pub fn path(&self, layout: &Layout) -> PathBuf {
        layout.memory_file(&self.identity_id)
    }
}

pub fn sync(
    layout: &Layout,
    identity_id: &str,
    identity_name: &str,
    synced_at: &str,
    memories: Vec<Memory>,
) -> Result<LocalMemoryMirror> {
    let mirror = LocalMemoryMirror {
        identity_id: identity_id.to_string(),
        identity_name: identity_name.to_string(),
        synced_at: synced_at.to_string(),
        memories,
    };
    mirror.save(layout)?;
    Ok(mirror)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn memory(id: &str, text: &str) -> Memory {
        Memory {
            id: id.to_string(),
            text: text.to_string(),
            created_at: "2026-08-07T06:00:00Z".to_string(),
        }
    }

    #[test]
    fn a_sync_is_a_complete_owner_only_snapshot() {
        let layout = Layout::at(crate::testdir::unique_path("daycare-memory"));
        let mirror = sync(
            &layout,
            "actor-1",
            "Pip",
            "2026-08-07T07:00:00Z",
            vec![memory("m-1", "I found the chalk.")],
        )
        .unwrap();

        assert_eq!(LocalMemoryMirror::load(&layout, "actor-1").unwrap(), mirror);
        assert_eq!(mirror.memories.len(), 1);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(layout.memory_file("actor-1"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600);
        }
    }

    #[test]
    fn a_later_sync_replaces_deleted_server_memories_instead_of_accumulating() {
        let layout = Layout::at(crate::testdir::unique_path("daycare-memory-replace"));
        sync(
            &layout,
            "actor-1",
            "Pip",
            "2026-08-07T07:00:00Z",
            vec![
                memory("m-old", "Forget this"),
                memory("m-keep", "Keep this"),
            ],
        )
        .unwrap();
        sync(
            &layout,
            "actor-1",
            "Pip",
            "2026-08-07T08:00:00Z",
            vec![memory("m-keep", "Keep this")],
        )
        .unwrap();

        let loaded = LocalMemoryMirror::load(&layout, "actor-1").unwrap();
        assert_eq!(loaded.memories, vec![memory("m-keep", "Keep this")]);
        assert_eq!(loaded.synced_at, "2026-08-07T08:00:00Z");
    }

    #[test]
    fn a_mirror_cannot_claim_to_belong_to_a_different_identity() {
        let layout = Layout::at(crate::testdir::unique_path("daycare-memory-owner"));
        let mirror = LocalMemoryMirror {
            identity_id: "actor-2".into(),
            identity_name: "Stranger".into(),
            synced_at: "2026-08-07T08:00:00Z".into(),
            memories: Vec::new(),
        };
        layout.ensure_root().unwrap();
        let bytes = serde_json::to_vec(&mirror).unwrap();
        write_atomic(&layout.memory_file("actor-1"), &bytes, 0o600).unwrap();

        let error = LocalMemoryMirror::load(&layout, "actor-1").unwrap_err();
        assert!(error.message().contains("refusing a misleading"), "{error}");
    }
}
