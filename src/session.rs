//! Picking which Claude is about to act, and getting its credential.
//!
//! Slice 1 had one answer to both questions, because a device had exactly one
//! actor. With several identities on a machine, "run a turn" needs to name one,
//! and every path that names one has to end at that identity's own token —
//! never the device's, and never a sibling's.
//!
//! The device token lists and mints. For every identity this build creates, it
//! cannot act — and that is the whole security argument for per-identity
//! tokens: if acting required a device token plus an identity argument, a
//! sibling identity's id would have to exist on a surface the local Claude can
//! reach. Acting authority would then come from an untrusted argument instead
//! of the credential.
//!
//! One identity is exempt, and the exemption is deliberate. A slice-1 install
//! adopted its single actor in place (`migrate_legacy`), and that actor's
//! credential IS the device token — the same secret, stored under two names.
//! So for that one actor, and only while the platform has not minted it a real
//! per-identity token, `read_identity_token` returns the device token and it
//! does reach the child's environment. The test
//! `the_adopted_actor_can_still_act_on_the_device_token_it_was_born_with`
//! pins that behaviour, because breaking it would strand a paired user's
//! memories rather than protect anything: on the server the two names denote
//! the same Claude. The exemption cannot widen — it is gated on
//! `identity_id == config.actor_id`, and a machine has one of those.

use crate::config::Config;
use crate::identity::{
    device_token_account, token_account, Identities, Identity, IdentityKind, LegacyIdentity,
    Resolution, Selector,
};
use crate::keychain::TokenStore;
use crate::paths::Layout;
use crate::workspace::Workspace;
use crate::{Error, Result};
use std::path::Path;

/// One resolved Claude, ready to run a turn.
#[derive(Debug, Clone)]
pub struct Active {
    pub identity: Identity,
    pub workspace: Workspace,
    pub platform_url: String,
    token: String,
}

impl Active {
    /// Deliberately a method rather than a public field: the token is handed to
    /// the child process and to the `Authorization` header, and nowhere else.
    pub fn token(&self) -> &str {
        &self.token
    }
}

/// Bring a slice-1 install forward, once, on first read.
///
/// The user paired before identities existed. Telling them to re-enroll would
/// abandon their Claude's memories and its session lineage, so the actor is
/// adopted where it stands: same id, same workspace directory, same
/// `sessions.json` row. Only the credential moves, from the device-keyed
/// keychain entry to an identity-keyed one.
///
/// Returns true when something was migrated, so the caller can say so.
pub fn migrate_legacy(
    layout: &Layout,
    store: &dyn TokenStore,
    config: &Config,
    identities: &mut Identities,
    now: &str,
) -> Result<bool> {
    if identities.get(&config.actor_id).is_some() {
        return Ok(false);
    }
    let adopted = identities.adopt_legacy(
        LegacyIdentity {
            identity_id: config.actor_id.clone(),
            name: config.actor_name.clone(),
            mcp_url: config.mcp_url.clone(),
        },
        now,
    );
    if !adopted {
        return Ok(false);
    }

    // Copy, and deliberately do not delete the legacy entry.
    //
    // It holds the same secret under the name the slice-1 binary looks for, so
    // leaving it makes a rollback free: an older build keeps working, and this
    // migration stops being one-way on a machine that is already paired and
    // running real turns. There is no second credential here to leak — one
    // token, reachable under the name each build knows it by.
    if let Some(token) = store.read(&config.device_id)? {
        store.store(&token_account(&config.actor_id), &token)?;
        store.store(&device_token_account(&config.device_id), &token)?;
    }

    identities.save(layout)?;
    Ok(true)
}

/// The device's own credential, used for listing and minting identities.
pub fn device_token(store: &dyn TokenStore, config: &Config) -> Result<String> {
    if let Some(token) = store.read(&device_token_account(&config.device_id))? {
        return Ok(token);
    }
    // A pre-migration install still has it under the bare device id.
    store.read(&config.device_id)?.ok_or_else(|| {
        Error::new(format!(
            "no device token in the keychain for {}; run `daycare-runner enroll` again",
            config.device_id
        ))
    })
}

/// Resolve a selector to a Claude that can act right now.
///
/// Returns `Err` when the selector names an identity that would have to be
/// created: minting needs the network, and this function deliberately does not
/// have it. The caller decides whether creating one is appropriate — `visit
/// start` will, `status` will not.
pub fn resolve(
    layout: &Layout,
    store: &dyn TokenStore,
    config: &Config,
    selector: &Selector,
    cwd: &Path,
) -> Result<Active> {
    let identities = Identities::load(layout)?;
    match identities.resolve(selector, cwd)? {
        Resolution::Use(identity_id) => activate(layout, store, config, &identities, &identity_id),
        Resolution::Create { name, kind, .. } => Err(Error::new(match kind {
            IdentityKind::General => format!(
                "no general Claude on this machine yet; run `daycare-runner identity create --name {name} --general`"
            ),
            IdentityKind::Workspace => format!(
                "no Claude for this project yet; run `daycare-runner identity create --name {name}`"
            ),
        })),
    }
}

pub fn activate(
    layout: &Layout,
    store: &dyn TokenStore,
    config: &Config,
    identities: &Identities,
    identity_id: &str,
) -> Result<Active> {
    let identity = identities
        .get(identity_id)
        .ok_or_else(|| Error::new(format!("no identity {identity_id} on this machine")))?
        .clone();

    let token = read_identity_token(store, config, &identity)?;
    let workspace = Workspace::new(layout.workspace_dir(&identity.identity_id));
    if !workspace.is_scaffolded() {
        // scaffold creates the directory, validates its physical ancestry, and
        // only then writes. Do not move the guard after its writes.
        workspace.scaffold(&identity.name, &identity.mcp_url)?;
    }
    // Keep the inspected physical path for the whole active session. Status,
    // `open`, visit turns, and homecoming must never return to a lexical parent
    // symlink after activation.
    let workspace = Workspace::new(workspace.guard_ancestors()?);
    workspace.guard_scaffold_files()?;

    Ok(Active {
        identity,
        workspace,
        platform_url: config.platform_url.clone(),
        token,
    })
}

/// The identity's token, or the device's if this identity is the adopted
/// slice-1 actor whose per-identity token has not been minted yet.
///
/// That fallback is not a loophole: the credential was originally minted with
/// authority to act for the adopted actor. It disappears when the platform mints a real
/// per-identity token for it.
fn read_identity_token(
    store: &dyn TokenStore,
    config: &Config,
    identity: &Identity,
) -> Result<String> {
    if let Some(token) = store.read(&token_account(&identity.identity_id))? {
        return Ok(token);
    }
    if identity.identity_id == config.actor_id {
        if let Some(token) = store.read(&config.device_id)? {
            return Ok(token);
        }
        if let Some(token) = store.read(&device_token_account(&config.device_id))? {
            return Ok(token);
        }
    }
    Err(Error::new(format!(
        "no credential for {} in the keychain; run `daycare-runner identity create --name {}` to mint one",
        identity.name, identity.name
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keychain::MemoryTokenStore;
    use std::path::PathBuf;

    fn legacy_config(layout: &Layout) -> Config {
        Config {
            platform_url: "https://example.test".into(),
            device_id: "device-1".into(),
            actor_id: "actor-1".into(),
            actor_name: "Patch".into(),
            workspace_dir: layout.workspace_dir("actor-1"),
            mcp_url: "https://example.test/api/daycare/mcp/mcp".into(),
            device_name: Some("laptop".into()),
        }
    }

    #[test]
    fn a_slice_one_install_migrates_without_losing_its_credential() {
        let layout = Layout::at(crate::testdir::unique_path("daycare-migrate"));
        let config = legacy_config(&layout);
        let store = MemoryTokenStore::default();
        store.store("device-1", "dck_legacy").unwrap();

        let mut identities = Identities::default();
        assert!(migrate_legacy(
            &layout,
            &store,
            &config,
            &mut identities,
            "2026-08-06T00:00:00Z"
        )
        .unwrap());

        // The Claude kept its id, so its workspace and session lineage still
        // point at the same place.
        let patch = identities.get("actor-1").unwrap();
        assert_eq!(patch.name, "Patch");
        assert_eq!(patch.kind, IdentityKind::General);

        // The credential is reachable under both new keys.
        assert_eq!(
            store.read(&token_account("actor-1")).unwrap().as_deref(),
            Some("dck_legacy")
        );
        assert_eq!(
            store
                .read(&device_token_account("device-1"))
                .unwrap()
                .as_deref(),
            Some("dck_legacy")
        );
        // And still under the old one, so downgrading the binary on a machine
        // that is already running real turns costs nothing.
        assert_eq!(
            store.read("device-1").unwrap().as_deref(),
            Some("dck_legacy")
        );

        // Running it twice must not undo the first run.
        assert!(!migrate_legacy(
            &layout,
            &store,
            &config,
            &mut identities,
            "2026-08-07T00:00:00Z"
        )
        .unwrap());
        assert_eq!(identities.all().len(), 1);
    }

    #[test]
    fn migration_is_a_no_op_for_an_install_that_never_had_a_token() {
        let layout = Layout::at(crate::testdir::unique_path("daycare-migrate-empty"));
        let config = legacy_config(&layout);
        let store = MemoryTokenStore::default();
        let mut identities = Identities::default();
        // The identity is still adopted — its memories are on the server and
        // matter more than the missing credential, which `enroll` can replace.
        assert!(migrate_legacy(
            &layout,
            &store,
            &config,
            &mut identities,
            "2026-08-06T00:00:00Z"
        )
        .unwrap());
        assert!(identities.get("actor-1").is_some());
    }

    #[test]
    fn acting_uses_the_identitys_own_token_never_a_siblings() {
        let layout = Layout::at(crate::testdir::unique_path("daycare-activate"));
        let config = legacy_config(&layout);
        let store = MemoryTokenStore::default();
        store.store(&token_account("actor-1"), "dck_patch").unwrap();
        store.store(&token_account("actor-2"), "dck_scout").unwrap();

        let mut identities = Identities::default();
        identities.insert(Identity {
            identity_id: "actor-1".into(),
            name: "Patch".into(),
            kind: IdentityKind::General,
            bound_workspace: None,
            workspace_label: None,
            mcp_url: config.mcp_url.clone(),
            created_at: "2026-08-06T00:00:00Z".into(),
        });
        identities.insert(Identity {
            identity_id: "actor-2".into(),
            name: "Scout".into(),
            kind: IdentityKind::Workspace,
            bound_workspace: Some(PathBuf::from("/Users/x/dev/voh")),
            workspace_label: Some("voh".into()),
            mcp_url: config.mcp_url.clone(),
            created_at: "2026-08-06T00:00:00Z".into(),
        });

        let patch = activate(&layout, &store, &config, &identities, "actor-1").unwrap();
        assert_eq!(patch.token(), "dck_patch");
        let scout = activate(&layout, &store, &config, &identities, "actor-2").unwrap();
        assert_eq!(scout.token(), "dck_scout");

        // Each Claude gets its own workspace directory — one CLAUDE.md, one
        // session lineage, no shared state between two characters.
        assert_ne!(patch.workspace.dir, scout.workspace.dir);
        assert!(patch.workspace.is_scaffolded());
        assert!(scout.workspace.is_scaffolded());
        assert!(std::fs::read_to_string(scout.workspace.claude_md())
            .unwrap()
            .contains("Scout"));

        std::fs::remove_dir_all(layout.root()).ok();
    }

    #[test]
    fn an_identity_with_no_credential_says_how_to_get_one() {
        let layout = Layout::at(crate::testdir::unique_path("daycare-nocred"));
        let config = legacy_config(&layout);
        let store = MemoryTokenStore::default();
        let mut identities = Identities::default();
        identities.insert(Identity {
            identity_id: "actor-9".into(),
            name: "Scout".into(),
            kind: IdentityKind::Workspace,
            bound_workspace: None,
            workspace_label: Some("voh".into()),
            mcp_url: config.mcp_url.clone(),
            created_at: "2026-08-06T00:00:00Z".into(),
        });
        let error = activate(&layout, &store, &config, &identities, "actor-9").unwrap_err();
        assert!(
            error.message().contains("no credential for Scout"),
            "{error}"
        );
        assert!(error.message().contains("identity create"), "{error}");
    }

    #[test]
    fn the_adopted_actor_can_still_act_on_the_device_token_it_was_born_with() {
        let layout = Layout::at(crate::testdir::unique_path("daycare-adopted"));
        let config = legacy_config(&layout);
        let store = MemoryTokenStore::default();
        store.store("device-1", "dck_legacy").unwrap();

        let mut identities = Identities::default();
        migrate_legacy(
            &layout,
            &store,
            &config,
            &mut identities,
            "2026-08-06T00:00:00Z",
        )
        .unwrap();
        let active = activate(&layout, &store, &config, &identities, "actor-1").unwrap();
        assert_eq!(active.token(), "dck_legacy");
        std::fs::remove_dir_all(layout.root()).ok();
    }

    #[test]
    fn resolving_an_identity_that_does_not_exist_yet_tells_the_user_how_to_make_it() {
        let layout = Layout::at(crate::testdir::unique_path("daycare-resolve-empty"));
        let config = legacy_config(&layout);
        let store = MemoryTokenStore::default();
        let error = resolve(
            &layout,
            &store,
            &config,
            &Selector::Default,
            Path::new("/Users/x/dev/voh"),
        )
        .unwrap_err();
        assert!(error.message().contains("identity create"), "{error}");
        // The suggestion has to be a command that works, and since the bare
        // invocation now means the universal Claude, that command is the
        // general one. A suggestion of `--name voh` would be the old default
        // leaking into the copy after the behaviour moved.
        assert!(error.message().contains("--general"), "{error}");
        assert!(!error.message().contains("voh"), "{error}");
    }
}

/// The slice-1 guard rail, re-asserted across identities.
///
/// Slice 2 adds a second Claude to the machine. Everything that made one turn
/// safe has to hold for each of them separately, and two things have to hold
/// *between* them: neither may see the other's credential, and neither may run
/// in the other's workspace.
#[cfg(test)]
mod multi_identity_guard {
    use super::tests_support::*;
    use super::*;
    use crate::keychain::MemoryTokenStore;
    use crate::launch::{build_launch_plan, LaunchOptions, SessionMode};

    #[test]
    fn two_identities_share_no_workspace_no_session_and_no_credential() {
        let (layout, config, store, identities) = two_claudes();

        let patch = activate(&layout, &store, &config, &identities, "actor-1").unwrap();
        let scout = activate(&layout, &store, &config, &identities, "actor-2").unwrap();

        assert_ne!(patch.workspace.dir, scout.workspace.dir);
        assert_ne!(patch.token(), scout.token());

        for (active, own, other) in [
            (&patch, "dck_patch", "dck_scout"),
            (&scout, "dck_scout", "dck_patch"),
        ] {
            let plan = build_launch_plan(LaunchOptions {
                claude_bin: "/mock/claude",
                mode: SessionMode::New {
                    reserved_session_id: "0d2b1f4e-1c3a-4b5d-8e9f-0a1b2c3d4e5f".into(),
                },
                message: "Take one world turn.",
                workspace: &active.workspace.dir,
                mcp_config: &active.workspace.mcp_config(),
                system_prompt_file: &active.workspace.controller_prompt(),
                tools: crate::launch::LaunchTools::DaycareWorld,
                model: crate::launch::DEFAULT_TURN_MODEL,
            })
            .unwrap();

            // Every safety flag still applies to every identity.
            assert!(pair(&plan.args, "--setting-sources", ""));
            assert!(pair(&plan.args, "--strict-mcp-config", ""));
            assert!(pair(&plan.args, "--permission-mode", "dontAsk"));
            assert!(pair(&plan.args, "--allowedTools", "mcp__daycare"));

            // The child runs in its own Claude's workspace and reads that
            // Claude's CLAUDE.md, not a sibling's.
            assert_eq!(plan.cwd, active.workspace.dir);
            assert!(std::fs::read_to_string(active.workspace.claude_md())
                .unwrap()
                .contains(&active.identity.name));

            // The credential reaches the child only through the environment —
            // proven end to end in tests/child_env.rs — so neither this
            // identity's token nor its sibling's may appear in the invocation.
            assert_eq!(active.token(), own);
            let rendered = format!("{:?} {:?}", plan.args, plan.stdin);
            assert!(!rendered.contains(other), "a sibling's token reached argv");
            assert!(!rendered.contains(own), "the token reached argv");
            let config = std::fs::read_to_string(active.workspace.mcp_config()).unwrap();
            assert!(
                !config.contains(own),
                "the token was written to the MCP config"
            );
            assert!(
                !config.contains(other),
                "a sibling's token reached the MCP config"
            );
        }

        std::fs::remove_dir_all(layout.root()).ok();
    }

    #[test]
    fn a_second_identity_cannot_be_reached_with_the_first_ones_name() {
        let (layout, _config, _store, identities) = two_claudes();
        // Selection is by name, and a name resolves to exactly one identity —
        // there is no path where a typo silently runs the other Claude.
        assert_eq!(
            identities
                .resolve(&Selector::Named("Patch".into()), Path::new("/tmp"))
                .unwrap(),
            crate::identity::Resolution::Use("actor-1".into())
        );
        assert!(identities
            .resolve(&Selector::Named("Patchh".into()), Path::new("/tmp"))
            .is_err());
        std::fs::remove_dir_all(layout.root()).ok();
    }

    fn two_claudes() -> (Layout, Config, MemoryTokenStore, Identities) {
        let layout = Layout::at(crate::testdir::unique_path("daycare-guard"));
        let config = Config {
            platform_url: "https://example.test".into(),
            device_id: "device-1".into(),
            actor_id: "actor-1".into(),
            actor_name: "Patch".into(),
            workspace_dir: layout.workspace_dir("actor-1"),
            mcp_url: "https://example.test/api/daycare/mcp/mcp".into(),
            device_name: None,
        };
        let store = MemoryTokenStore::default();
        store.store(&token_account("actor-1"), "dck_patch").unwrap();
        store.store(&token_account("actor-2"), "dck_scout").unwrap();

        let mut identities = Identities::default();
        for (id, name, kind) in [
            ("actor-1", "Patch", IdentityKind::General),
            ("actor-2", "Scout", IdentityKind::Workspace),
        ] {
            identities.insert(Identity {
                identity_id: id.into(),
                name: name.into(),
                kind,
                bound_workspace: None,
                workspace_label: None,
                mcp_url: config.mcp_url.clone(),
                created_at: "2026-08-06T00:00:00Z".into(),
            });
        }
        (layout, config, store, identities)
    }
}

#[cfg(test)]
mod tests_support {
    /// `--strict-mcp-config` takes no value, so the generic pair check treats an
    /// empty expectation as "the flag is present at all".
    pub fn pair(args: &[String], flag: &str, value: &str) -> bool {
        if value.is_empty() {
            return args.iter().any(|arg| arg == flag);
        }
        args.windows(2).any(|w| w[0] == flag && w[1] == value)
    }
}
