use crate::launch::validate_session_id;
use crate::paths::{shell_quote, shell_quote_path, write_atomic, Layout};
use crate::{Error, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

/// Everything the companion needs to run a turn except the credential. The
/// device token lives in the OS keychain; if it ever appears in this struct the
/// enrollment is wrong.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Config {
    pub platform_url: String,
    pub device_id: String,
    pub actor_id: String,
    pub actor_name: String,
    pub workspace_dir: PathBuf,
    /// Absolute URL of the Daycare MCP endpoint, derived at enrollment from the
    /// platform's `mcp_path`. Public routing information, not a secret.
    pub mcp_url: String,
    #[serde(default)]
    pub device_name: Option<String>,
}

impl Config {
    pub fn load(layout: &Layout) -> Result<Self> {
        let path = layout.config_file();
        let bytes = fs::read(&path).map_err(|error| {
            Error::new(format!(
                "no enrollment found at {} ({error}); run `daycare-runner enroll` first",
                path.display()
            ))
        })?;
        let config: Config = serde_json::from_slice(&bytes).map_err(|error| {
            Error::new(format!("{} is not valid config: {error}", path.display()))
        })?;
        config.validate()?;
        Ok(config)
    }

    pub fn save(&self, layout: &Layout) -> Result<()> {
        self.validate()?;
        layout.ensure_root()?;
        let bytes = serde_json::to_vec_pretty(self)?;
        write_atomic(&layout.config_file(), &bytes, 0o600)
    }

    fn validate(&self) -> Result<()> {
        if self.platform_url.trim().is_empty() {
            return Err(Error::new("config.platform_url is empty"));
        }
        if self.actor_id.trim().is_empty() {
            return Err(Error::new("config.actor_id is empty"));
        }
        if self.device_id.trim().is_empty() {
            return Err(Error::new("config.device_id is empty"));
        }
        Ok(())
    }

    /// The command the user runs to talk to the same Claude interactively.
    pub fn attach_command(&self, session_id: Option<&str>) -> String {
        match session_id {
            Some(id) => format!(
                "cd {} && claude --resume {}",
                shell_quote_path(&self.workspace_dir),
                shell_quote(id)
            ),
            None => format!(
                "cd {} && claude   # no daycare session yet; run a turn first",
                shell_quote_path(&self.workspace_dir)
            ),
        }
    }
}

/// `actor_id -> claude_session_id`. This map is how the same Claude — with its
/// memory of previous turns — comes back on the next turn via `--resume`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Sessions(pub BTreeMap<String, String>);

impl Sessions {
    pub fn load(layout: &Layout) -> Result<Self> {
        let path = layout.sessions_file();
        match fs::read(&path) {
            Ok(bytes) => {
                let sessions: Sessions = serde_json::from_slice(&bytes).map_err(|error| {
                    Error::new(format!(
                        "{} is not valid sessions map: {error}",
                        path.display()
                    ))
                })?;
                sessions.validate()?;
                Ok(sessions)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Sessions::default()),
            Err(error) => Err(error.into()),
        }
    }

    pub fn get(&self, actor_id: &str) -> Option<&str> {
        self.0.get(actor_id).map(String::as_str)
    }

    pub fn set(&mut self, actor_id: &str, session_id: &str) {
        self.0.insert(actor_id.to_string(), session_id.to_string());
    }

    pub fn save(&self, layout: &Layout) -> Result<()> {
        self.validate()?;
        layout.ensure_root()?;
        let bytes = serde_json::to_vec_pretty(self)?;
        write_atomic(&layout.sessions_file(), &bytes, 0o600)
    }

    fn validate(&self) -> Result<()> {
        for (actor_id, session_id) in &self.0 {
            validate_session_id(session_id).map_err(|_| {
                Error::new(format!(
                    "session map contains an invalid Claude session id for {actor_id}"
                ))
            })?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> Layout {
        Layout::at(crate::testdir::unique_path(&format!(
            "daycare-config-{name}"
        )))
    }

    fn sample(layout: &Layout) -> Config {
        Config {
            platform_url: "https://example.test".into(),
            device_id: "device-1".into(),
            actor_id: "actor-1".into(),
            actor_name: "Pip".into(),
            workspace_dir: layout.workspace_dir("actor-1"),
            mcp_url: "https://example.test/api/daycare/mcp".into(),
            device_name: Some("josh-mbp".into()),
        }
    }

    #[test]
    fn config_round_trips_and_never_stores_a_token() {
        let layout = scratch("round-trip");
        let config = sample(&layout);
        config.save(&layout).unwrap();

        let raw = fs::read_to_string(layout.config_file()).unwrap();
        assert!(
            !raw.to_lowercase().contains("token"),
            "config leaked a token field: {raw}"
        );

        assert_eq!(Config::load(&layout).unwrap(), config);
        fs::remove_dir_all(layout.root()).ok();
    }

    #[test]
    fn sessions_round_trip_and_default_to_empty() {
        let layout = scratch("sessions");
        assert_eq!(Sessions::load(&layout).unwrap(), Sessions::default());

        let mut sessions = Sessions::default();
        sessions.set("actor-1", "550e8400-e29b-41d4-a716-446655440000");
        sessions.save(&layout).unwrap();

        let reloaded = Sessions::load(&layout).unwrap();
        assert_eq!(
            reloaded.get("actor-1"),
            Some("550e8400-e29b-41d4-a716-446655440000")
        );
        assert_eq!(reloaded.get("actor-2"), None);
        fs::remove_dir_all(layout.root()).ok();
    }

    #[test]
    fn invalid_session_ids_are_never_loaded_or_saved() {
        let layout = scratch("invalid-sessions");
        let mut sessions = Sessions::default();
        sessions.set("actor-1", "bad; touch /tmp/daycare-owned");
        let error = sessions.save(&layout).unwrap_err();
        assert!(error.message().contains("invalid Claude session id"));

        layout.ensure_root().unwrap();
        fs::write(
            layout.sessions_file(),
            r#"{"actor-1":"bad; touch /tmp/daycare-owned"}"#,
        )
        .unwrap();
        let error = Sessions::load(&layout).unwrap_err();
        assert!(error.message().contains("invalid Claude session id"));
        fs::remove_dir_all(layout.root()).ok();
    }

    #[test]
    fn missing_config_explains_how_to_enroll() {
        let layout = scratch("missing");
        let error = Config::load(&layout).unwrap_err();
        assert!(error.message().contains("enroll"), "{error}");
    }

    #[test]
    fn attach_command_points_at_the_owned_workspace() {
        let layout = scratch("attach");
        let config = sample(&layout);
        let command = config.attach_command(Some("550e8400-e29b-41d4-a716-446655440000"));
        assert!(command.contains("claude --resume '550e8400-e29b-41d4-a716-446655440000'"));
        assert!(command.contains("workspaces/actor-1"));

        let hostile = config.attach_command(Some("bad; touch /tmp/daycare-owned"));
        assert!(hostile.contains("--resume 'bad; touch /tmp/daycare-owned'"));
    }
}
