//! The identities one paired computer holds, and how a visit picks one.
//!
//! Slice 1 had a single actor per device, so the actor lived in `config.json`.
//! Slice 2 keeps many, so `config.json` describes the *device* and this file
//! describes who lives on it. `sessions.json` needed no change at all: it was
//! already keyed by actor, which is why `--resume` works per identity for free.
//!
//! An identity is not a process and not a workspace. `bound_workspace` is a
//! label — the project a user thinks of this Claude as belonging to — and the
//! runner never treats it as a path to read, a cwd, or an `--add-dir`. What the
//! user gets from it is memory continuity: the identity sent from `~/dev/foo`
//! is the same identity every time, with its own Daycare memories. See
//! `docs/daycare/SLICE-2-identities.md` §4.

use crate::paths::{write_atomic, Layout};
use crate::{Error, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

/// Keychain account for an identity's own minted token. Each identity holds a
/// separate credential so a compromised one cannot act as its siblings, and so
/// the permitted actor never has to travel in a tool argument.
pub fn token_account(identity_id: &str) -> String {
    format!("identity:{identity_id}")
}

/// Keychain account for the device token, which lists and mints identities.
///
/// It is not handed to a child for any identity this build creates. The one
/// exception is the adopted slice-1 actor, whose per-identity token and device
/// token are the same secret under two names — see the module comment on
/// `session.rs` for why that is safe and why it cannot widen.
pub fn device_token_account(device_id: &str) -> String {
    format!("device:{device_id}")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum IdentityKind {
    /// Labelled with a project directory.
    Workspace,
    /// The account-wide Claude, not tied to any project.
    General,
}

impl IdentityKind {
    /// The platform's `daycare_actors_kind_check` vocabulary.
    pub fn as_str(self) -> &'static str {
        match self {
            IdentityKind::Workspace => "workspace",
            IdentityKind::General => "general",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Identity {
    pub identity_id: String,
    pub name: String,
    pub kind: IdentityKind,
    /// Absolute project path this identity is labelled with on this machine.
    /// Never read, never passed to the child. `None` for a general identity or
    /// for a workspace identity newly moved onto a machine without that path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bound_workspace: Option<PathBuf>,
    /// Human label supplied to the platform for a workspace identity.
    ///
    /// This is deliberately separate from `bound_workspace`: after moving an
    /// identity to a fresh machine the server can truthfully tell us that it is
    /// the "voices-of-history" workspace Claude, but it cannot know where (or
    /// whether) that project exists on this filesystem.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_label: Option<String>,
    /// Absolute Daycare MCP endpoint for this identity. Routing, not a secret.
    pub mcp_url: String,
    pub created_at: String,
}

impl Identity {
    /// A stable, explicit description for UI and JSON consumers.
    pub fn binding_state(&self) -> &'static str {
        match (self.kind, self.bound_workspace.is_some()) {
            (IdentityKind::General, _) => "not_applicable",
            (IdentityKind::Workspace, true) => "bound_on_this_machine",
            (IdentityKind::Workspace, false) => "unbound_on_this_machine",
        }
    }

    /// Prefer the server-carried label, while deriving one for older local
    /// records that predate the separate field.
    pub fn display_workspace_label(&self) -> Option<String> {
        self.workspace_label.clone().or_else(|| {
            self.bound_workspace
                .as_deref()
                .map(crate::wire::workspace_label)
        })
    }
}

/// `identity_id -> Identity`, persisted as `identities.json`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Identities(pub BTreeMap<String, Identity>);

impl Identities {
    pub fn load(layout: &Layout) -> Result<Self> {
        let path = layout.identities_file();
        match fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(|error| {
                Error::new(format!(
                    "{} is not a valid identities map: {error}",
                    path.display()
                ))
            }),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Identities::default()),
            Err(error) => Err(error.into()),
        }
    }

    pub fn save(&self, layout: &Layout) -> Result<()> {
        layout.ensure_root()?;
        let bytes = serde_json::to_vec_pretty(self)?;
        write_atomic(&layout.identities_file(), &bytes, 0o600)
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn get(&self, identity_id: &str) -> Option<&Identity> {
        self.0.get(identity_id)
    }

    pub fn insert(&mut self, identity: Identity) {
        self.0.insert(identity.identity_id.clone(), identity);
    }

    pub fn remove(&mut self, identity_id: &str) -> Option<Identity> {
        self.0.remove(identity_id)
    }

    /// Ordered for display: general identities first, then by name.
    pub fn all(&self) -> Vec<&Identity> {
        let mut all: Vec<&Identity> = self.0.values().collect();
        all.sort_by_key(|identity| {
            (
                identity.kind != IdentityKind::General,
                identity.name.to_lowercase(),
            )
        });
        all
    }

    /// Names are what the user types, so they must be unique and are matched
    /// case-insensitively.
    pub fn by_name(&self, name: &str) -> Option<&Identity> {
        let wanted = name.trim().to_lowercase();
        self.0
            .values()
            .find(|identity| identity.name.to_lowercase() == wanted)
    }

    /// The machine's general Claude — the earliest one to hold the slot.
    ///
    /// `create` refuses to mint a second general, so normally there is one and
    /// the tie-break never runs. It exists because an install written by an
    /// older binary can already hold two, and the alternative is worse than a
    /// rule: this map is a `HashMap`, so `.values().find(..)` picked an
    /// arbitrary one and could pick a *different* one on the next run. A Claude
    /// that changes identity between two invocations of the same command is the
    /// kind of bug that gets blamed on the model.
    ///
    /// Earliest-created wins because that is the one the user has been using.
    pub fn general(&self) -> Option<&Identity> {
        self.0
            .values()
            .filter(|identity| identity.kind == IdentityKind::General)
            .min_by(|a, b| {
                a.created_at
                    .cmp(&b.created_at)
                    .then_with(|| a.identity_id.cmp(&b.identity_id))
            })
    }

    pub fn names(&self) -> String {
        let names: Vec<&str> = self.all().iter().map(|i| i.name.as_str()).collect();
        if names.is_empty() {
            "none yet".to_string()
        } else {
            names.join(", ")
        }
    }
}

/// What the caller asked for. `Default` is the bare `daycare` invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Selector {
    Id(String),
    Named(String),
    General,
    New { name: String, general: bool },
    Default,
}

/// What resolution decided. Creating is separated from using because minting an
/// identity needs the platform and a token, and choosing one does not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolution {
    Use(String),
    Create {
        name: String,
        kind: IdentityKind,
        bound_workspace: Option<PathBuf>,
    },
}

impl Identities {
    /// Pick the identity for a visit. `project` is the current project root —
    /// a label only; nothing inside it is read.
    ///
    /// The bare-invocation row is a product call, QUESTIONS.md #8, and Josh
    /// ANSWERED it on 2026-08-06 by reversing the provisional default: bare
    /// `daycare` means the machine's universal Claude, never the one labelled
    /// with the current directory. His reason is the one that settles it —
    /// people run Claude from many folders and mostly do not know which one
    /// they are in, so scoping by cwd makes the same command mean different
    /// things for a reason the user cannot see.
    ///
    /// A project-bound identity is still reachable, by name, with `--identity`.
    /// It just stops being what silence selects. Reversing this is restoring
    /// the `bound_to(project)` arm below the delegation.
    pub fn resolve(&self, selector: &Selector, project: &Path) -> Result<Resolution> {
        match selector {
            Selector::Id(identity_id) => self
                .get(identity_id)
                .map(|identity| Resolution::Use(identity.identity_id.clone()))
                .ok_or_else(|| {
                    Error::new(format!(
                        "no identity with id {identity_id:?} on this computer. Known: {}",
                        self.names()
                    ))
                }),
            Selector::Named(name) => self
                .by_name(name)
                .map(|identity| Resolution::Use(identity.identity_id.clone()))
                .ok_or_else(|| {
                    Error::new(format!(
                        "no identity named {name:?} on this computer. Known: {}",
                        self.names()
                    ))
                }),
            Selector::General => Ok(match self.general() {
                Some(identity) => Resolution::Use(identity.identity_id.clone()),
                None => Resolution::Create {
                    name: "general".to_string(),
                    kind: IdentityKind::General,
                    bound_workspace: None,
                },
            }),
            Selector::New { name, general } => {
                if let Some(existing) = self.by_name(name) {
                    return Err(Error::new(format!(
                        "an identity named {:?} already exists; use --identity {} to send it",
                        existing.name, existing.name
                    )));
                }
                // There is one general Claude, not a set of them — every
                // command that says "the machine's general Claude" means a
                // definite thing. Minting a second silently displaced the
                // first as the default, so the user's own Claude stopped
                // being the one that answered.
                if *general {
                    if let Some(existing) = self.general() {
                        return Err(Error::new(format!(
                            "{:?} is already this machine's general Claude. Give this one a \
                             project with --bind, or send {} with --general.",
                            existing.name, existing.name
                        )));
                    }
                }
                Ok(Resolution::Create {
                    name: name.clone(),
                    kind: if *general {
                        IdentityKind::General
                    } else {
                        IdentityKind::Workspace
                    },
                    bound_workspace: (!*general).then(|| project.to_path_buf()),
                })
            }
            Selector::Default => self.resolve(&Selector::General, project),
        }
    }
}

/// The project a visit is launched from: the git repository root if there is
/// one, else the current directory. Only ever used as a label and a map key.
pub fn project_root(cwd: &Path) -> PathBuf {
    let mut candidate = Some(cwd);
    while let Some(dir) = candidate {
        if dir.join(".git").exists() {
            return dir.to_path_buf();
        }
        candidate = dir.parent();
    }
    cwd.to_path_buf()
}

/// Fold a slice-1 install forward.
///
/// A slice-1 `config.json` carries `actor_id` / `actor_name` / `mcp_url`, and
/// the keychain holds one entry keyed by device id. Rather than telling an
/// already-paired user to re-enroll — which would abandon their Claude's
/// memories and session lineage — that actor becomes an identity in place. The
/// actor id becomes the identity id, so its workspace directory and its
/// `sessions.json` row are already correct and nothing moves on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LegacyIdentity {
    pub identity_id: String,
    pub name: String,
    pub mcp_url: String,
}

impl Identities {
    /// Returns true when a legacy actor was adopted, so the caller knows to
    /// re-key the keychain entry and rewrite `config.json`.
    pub fn adopt_legacy(&mut self, legacy: LegacyIdentity, created_at: &str) -> bool {
        if self.0.contains_key(&legacy.identity_id) {
            return false;
        }
        self.insert(Identity {
            identity_id: legacy.identity_id,
            name: legacy.name,
            // A slice-1 actor was the computer's only Claude and was bound to
            // nothing, which is exactly what `general` means.
            kind: IdentityKind::General,
            bound_workspace: None,
            workspace_label: None,
            mcp_url: legacy.mcp_url,
            created_at: created_at.to_string(),
        });
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity(name: &str, kind: IdentityKind, bound: Option<&str>) -> Identity {
        Identity {
            identity_id: format!("id-{name}"),
            name: name.to_string(),
            kind,
            bound_workspace: bound.map(PathBuf::from),
            workspace_label: bound.map(|path| crate::wire::workspace_label(Path::new(path))),
            mcp_url: "https://example.test/api/daycare/mcp/mcp".to_string(),
            created_at: "2026-08-06T00:00:00Z".to_string(),
        }
    }

    fn populated() -> Identities {
        let mut identities = Identities::default();
        identities.insert(identity("general", IdentityKind::General, None));
        identities.insert(identity(
            "voices-of-history",
            IdentityKind::Workspace,
            Some("/Users/x/dev/voh"),
        ));
        identities
    }

    #[test]
    fn identities_round_trip_and_default_to_empty() {
        let layout = Layout::at(crate::testdir::unique_path("daycare-identities"));
        assert!(Identities::load(&layout).unwrap().is_empty());

        let identities = populated();
        identities.save(&layout).unwrap();
        assert_eq!(Identities::load(&layout).unwrap(), identities);
    }

    #[test]
    fn a_named_selector_finds_its_identity_and_says_so_when_it_cannot() {
        let identities = populated();
        assert_eq!(
            identities
                .resolve(&Selector::Named("general".into()), Path::new("/tmp"))
                .unwrap(),
            Resolution::Use("id-general".into())
        );
        // Users type names, so case is not their problem.
        assert_eq!(
            identities
                .resolve(
                    &Selector::Named("Voices-Of-History".into()),
                    Path::new("/tmp")
                )
                .unwrap(),
            Resolution::Use("id-voices-of-history".into())
        );
        let error = identities
            .resolve(&Selector::Named("nobody".into()), Path::new("/tmp"))
            .unwrap_err();
        assert!(error.message().contains("nobody"), "{error}");
        // The error has to be actionable: it lists what does exist.
        assert!(error.message().contains("voices-of-history"), "{error}");
    }

    #[test]
    fn an_id_selector_is_exact_across_duplicate_names_and_general_profiles() {
        let mut identities = Identities::default();
        let mut old_workspace = identity("Quill", IdentityKind::Workspace, None);
        old_workspace.identity_id = "workspace-a".into();
        identities.insert(old_workspace);
        let mut claimed_workspace = identity("Quill", IdentityKind::Workspace, None);
        claimed_workspace.identity_id = "workspace-b".into();
        identities.insert(claimed_workspace);
        let mut old_general = identity("Pip", IdentityKind::General, None);
        old_general.identity_id = "general-a".into();
        identities.insert(old_general);
        let mut claimed_general = identity("Scout", IdentityKind::General, None);
        claimed_general.identity_id = "general-b".into();
        identities.insert(claimed_general);

        assert_eq!(
            identities
                .resolve(&Selector::Id("workspace-b".into()), Path::new("/tmp"))
                .unwrap(),
            Resolution::Use("workspace-b".into())
        );
        assert_eq!(
            identities
                .resolve(&Selector::Id("general-b".into()), Path::new("/tmp"))
                .unwrap(),
            Resolution::Use("general-b".into())
        );
        assert!(identities
            .resolve(&Selector::Id("missing".into()), Path::new("/tmp"))
            .unwrap_err()
            .to_string()
            .contains("no identity with id \"missing\""));
    }

    /// QUESTIONS.md #8, as Josh answered it: bare `daycare` is the universal
    /// Claude even standing in a directory that has its own.
    ///
    /// The cwd here is the exact path `id-voices-of-history` is bound to, so
    /// this fails the moment anyone restores the `bound_to(project)` arm. That
    /// is the whole point of asserting at this path rather than a neutral one.
    #[test]
    fn the_bare_invocation_ignores_the_project_binding() {
        let identities = populated();
        assert_eq!(
            identities
                .resolve(&Selector::Default, Path::new("/Users/x/dev/voh"))
                .unwrap(),
            Resolution::Use("id-general".into())
        );
        // And the project-bound Claude is still reachable — it lost the
        // default, not its existence.
        assert_eq!(
            identities
                .resolve(
                    &Selector::Named("voices-of-history".into()),
                    Path::new("/Users/x/dev/voh")
                )
                .unwrap(),
            Resolution::Use("id-voices-of-history".into())
        );
    }

    #[test]
    fn the_bare_invocation_falls_back_to_the_general_claude_before_creating_one() {
        let identities = populated();
        assert_eq!(
            identities
                .resolve(&Selector::Default, Path::new("/Users/x/dev/unknown"))
                .unwrap(),
            Resolution::Use("id-general".into())
        );
    }

    /// On a machine with nothing on it, the bare invocation asks for a general
    /// Claude — it never proposes one named after whatever directory the user
    /// happened to be standing in. The directory is still passed in, and is
    /// still ignored.
    #[test]
    fn first_use_asks_for_the_universal_claude_not_a_project_one() {
        let identities = Identities::default();
        let expected = Resolution::Create {
            name: "general".into(),
            kind: IdentityKind::General,
            bound_workspace: None,
        };
        assert_eq!(
            identities
                .resolve(&Selector::Default, Path::new("/Users/x/dev/voh"))
                .unwrap(),
            expected
        );
        // Same answer from a different directory. Under the old default these
        // two calls returned different identities, which is precisely the
        // "same command, different meaning" Josh rejected.
        assert_eq!(
            identities
                .resolve(&Selector::Default, Path::new("/Users/x/notes"))
                .unwrap(),
            expected
        );
    }

    #[test]
    fn a_new_identity_refuses_to_shadow_an_existing_name() {
        let identities = populated();
        let error = identities
            .resolve(
                &Selector::New {
                    name: "general".into(),
                    general: false,
                },
                Path::new("/Users/x/dev/voh"),
            )
            .unwrap_err();
        assert!(error.message().contains("already exists"), "{error}");
    }

    #[test]
    fn the_general_selector_creates_the_general_claude_once() {
        let identities = Identities::default();
        assert_eq!(
            identities
                .resolve(&Selector::General, Path::new("/Users/x/dev/voh"))
                .unwrap(),
            Resolution::Create {
                name: "general".into(),
                kind: IdentityKind::General,
                bound_workspace: None,
            }
        );
        // A general identity is never labelled with the directory it was made in.
        assert_eq!(
            populated()
                .resolve(&Selector::General, Path::new("/Users/x/dev/voh"))
                .unwrap(),
            Resolution::Use("id-general".into())
        );
    }

    /// There is one general Claude, and a second cannot be minted.
    ///
    /// Found live on 2026-08-06: `identity create --name Scout --general` on an
    /// install that already had a general Claude succeeded, and Scout then
    /// answered as the default. The user's own Claude stopped being the one
    /// that replied to bare commands, with nothing said about why.
    #[test]
    fn a_second_general_claude_is_refused_and_the_refusal_names_the_first() {
        let error = populated()
            .resolve(
                &Selector::New {
                    name: "Scout".into(),
                    general: true,
                },
                Path::new("/Users/x/dev/voh"),
            )
            .unwrap_err();
        let message = error.message();
        assert!(
            message.contains("general"),
            "the refusal does not say what the conflict is: {message}"
        );
        assert!(
            message.contains("general") && message.contains("Scout") || message.contains("--bind"),
            "the refusal leaves the user with no way to make the Claude they asked for: {message}"
        );
        // The same name bound to a project is still fine — only the slot is taken.
        assert!(populated()
            .resolve(
                &Selector::New {
                    name: "Scout".into(),
                    general: false
                },
                Path::new("/Users/x/dev/other"),
            )
            .is_ok());
    }

    /// Which Claude is "the general one" cannot depend on map iteration order.
    ///
    /// `Identities` is a `HashMap`. An install written by an older binary can
    /// hold two generals, and picking with `.values().find(..)` meant the
    /// default could resolve to a *different* Claude on the next invocation of
    /// the same command — a Claude that forgets everything, intermittently,
    /// which reads as a model failure rather than a lookup bug.
    /// Enough generals that hash order cannot accidentally agree with age.
    ///
    /// A two-entry version of this test passed against the arbitrary-order
    /// implementation it was written to catch: with one pair, whichever key the
    /// map happened to yield first was the right answer half the time, and it
    /// was. The fixture has to make the wrong implementation wrong.
    #[test]
    fn with_several_general_claudes_the_earliest_wins_every_time() {
        let mut identities = Identities::default();
        // Inserted newest-first, so insertion order is no help either.
        for day in (1..=12).rev() {
            let mut general = identity(&format!("General{day:02}"), IdentityKind::General, None);
            general.identity_id = format!("id-{:x}", day * 2_654_435_761u64);
            general.created_at = format!("2026-08-{day:02}T00:00:00Z");
            identities.insert(general);
        }

        for _ in 0..50 {
            assert_eq!(
                identities.general().map(|i| i.name.as_str()),
                Some("General01"),
                "the general Claude is not the earliest one, or changed between \
                 two identical lookups"
            );
        }
    }

    #[test]
    fn each_identity_has_its_own_keychain_account_and_never_the_devices() {
        assert_eq!(token_account("actor-1"), "identity:actor-1");
        assert_ne!(token_account("actor-1"), token_account("actor-2"));
        assert_ne!(token_account("d1"), device_token_account("d1"));
    }

    #[test]
    fn a_slice_one_actor_becomes_an_identity_without_moving_anything() {
        let mut identities = Identities::default();
        let adopted = identities.adopt_legacy(
            LegacyIdentity {
                identity_id: "fb1338e3".into(),
                name: "Patch".into(),
                mcp_url: "https://example.test/api/daycare/mcp/mcp".into(),
            },
            "2026-08-06T00:00:00Z",
        );
        assert!(adopted);
        let patch = identities.get("fb1338e3").unwrap();
        // The identity id IS the old actor id, so the workspace directory and
        // the sessions.json row keep working untouched.
        assert_eq!(patch.identity_id, "fb1338e3");
        assert_eq!(patch.kind, IdentityKind::General);
        assert!(patch.bound_workspace.is_none());
        // Adopting twice must not clobber an identity that has since changed.
        assert!(!identities.adopt_legacy(
            LegacyIdentity {
                identity_id: "fb1338e3".into(),
                name: "Renamed".into(),
                mcp_url: "https://example.test/api/daycare/mcp/mcp".into(),
            },
            "2026-08-07T00:00:00Z"
        ));
        assert_eq!(identities.get("fb1338e3").unwrap().name, "Patch");
    }

    #[test]
    fn a_project_root_is_the_git_repository_when_there_is_one() {
        let repo = crate::testdir::unique_dir("daycare-project");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        let nested = repo.join("apps/website");
        std::fs::create_dir_all(&nested).unwrap();
        assert_eq!(project_root(&nested), repo);

        let loose = crate::testdir::unique_dir("daycare-loose");
        assert_eq!(project_root(&loose), loose);
    }
}
