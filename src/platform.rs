//! The companion's half of the Daycare REST contract.
//!
//! Three calls: claim a pairing code, take the next queued world command, and
//! report the receipt. The device token is a parameter on every call rather
//! than a field on the client, so it lives in memory only for the duration of a
//! request and can never be printed by a `Debug` of the client.

use crate::stream::TurnUsage;
use crate::wire::{paths, VisitEndReason};
use crate::{Error, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fmt;
use std::time::Duration;

pub const PAIR_CLAIM_PATH: &str = paths::PAIR_CLAIM;
pub const NEXT_COMMAND_PATH: &str = paths::NEXT_COMMAND;
/// Mirrors `DAYCARE_MAX_WORKSPACE_LABEL` on the platform.
const MAX_WORKSPACE_LABEL_CHARS: usize = 80;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MatchOutcomeResult {
    Won,
    Lost,
    Drew,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MatchOutcomeWinner {
    You,
    Opponent,
    Draw,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MatchOutcomeBoard {
    pub yours: u32,
    pub opponent: u32,
}

/// Canonical terminal facts already translated into this participant's point
/// of view. Strict decoding prevents a future server payload from silently
/// carrying a stable opponent id into a private local prompt.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MatchOutcome {
    pub kind: String,
    pub result: MatchOutcomeResult,
    pub winner: MatchOutcomeWinner,
    pub board: MatchOutcomeBoard,
    #[serde(rename = "verdictCompletedAt")]
    pub verdict_completed_at: String,
    pub summary: String,
}

impl MatchOutcome {
    fn is_valid(&self) -> bool {
        if self.kind != "debate_league" {
            return false;
        }
        let coherent = matches!(
            (self.result, self.winner),
            (MatchOutcomeResult::Won, MatchOutcomeWinner::You)
                | (MatchOutcomeResult::Lost, MatchOutcomeWinner::Opponent)
                | (MatchOutcomeResult::Drew, MatchOutcomeWinner::Draw)
        );
        if !coherent {
            return false;
        }
        let expected_summary = match self.result {
            MatchOutcomeResult::Won => format!(
                "You won the Debate League match, {}–{} on the final board.",
                self.board.yours, self.board.opponent
            ),
            MatchOutcomeResult::Lost => format!(
                "You lost the Debate League match, {}–{} on the final board.",
                self.board.yours, self.board.opponent
            ),
            MatchOutcomeResult::Drew => format!(
                "The Debate League match ended in a {}–{} draw.",
                self.board.yours, self.board.opponent
            ),
        };
        self.summary == expected_summary
            && !self.verdict_completed_at.is_empty()
            && self.verdict_completed_at.len() <= 64
            && self
                .verdict_completed_at
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"-:.+".contains(&byte))
    }
}

/// What the platform hands back when a pairing code is redeemed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PairingActorKind {
    Workspace,
    General,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairingIdentityMetadata {
    pub actor_kind: PairingActorKind,
    pub workspace_label: Option<String>,
}

#[derive(Clone, Deserialize)]
pub struct PairingClaim {
    pub device_token: String,
    pub device_id: String,
    pub actor_id: String,
    pub actor_name: String,
    /// Path (or absolute URL) of the Daycare MCP endpoint for this device.
    pub mcp_path: String,
    /// True when this claim moved an existing identity onto this machine rather
    /// than creating a new one.
    ///
    /// It cannot be derived locally in the case that matters. On a machine that
    /// already knows the identity, `identities.json` answers it — but the whole
    /// point of a re-point is a *fresh* machine, whose `identities.json` is
    /// empty, making a re-point byte-for-byte identical to a first pairing.
    ///
    /// Defaults to false so a platform that has not shipped the re-point path
    /// yet degrades to "this is a new pairing" — which is what every claim was
    /// until 2026-08-06.
    ///
    /// The name is the server's, deliberately. This field spent a few hours
    /// spelled `re_pointed` here while the server spelled it `repointed`, and
    /// because the default is `false` the mismatch reported every re-pair as a
    /// fresh pairing without erroring anywhere. A tolerant default and a
    /// guessed name are safe apart and dangerous together.
    #[serde(default)]
    pub repointed: bool,
    /// The identity's own acting credential. Present on a re-point, null on a
    /// first pairing, and that asymmetry is the server's design rather than a
    /// rollout gap.
    ///
    /// A first pairing leaves the identity tokenless on purpose so the device
    /// token resolves to it through `resolveLegacyDeviceActor`. A re-point
    /// rotates a fresh hash onto the identity row, and that rotation is what
    /// kills the old machine's copy — so the new machine must store the token
    /// returned here or it has no credential able to act at all.
    ///
    /// Both cases are live. Null is not a degraded mode to be designed away;
    /// it is what most claims will carry.
    #[serde(default)]
    pub identity_token: Option<String>,
    /// Server-owned profile metadata. A credential authorizes this actor; it
    /// does not define whether the actor is a general or workspace identity.
    /// These optional fields are absent on older platform builds.
    #[serde(default)]
    pub actor_kind: Option<PairingActorKind>,
    #[serde(default)]
    pub workspace_label: Option<String>,
}

impl PairingClaim {
    /// Validate the two profile fields as one unit. Partial metadata is more
    /// dangerous than no metadata because it can silently change identity
    /// type during a move to a fresh machine.
    pub fn identity_metadata(&self) -> Result<Option<PairingIdentityMetadata>> {
        match (self.actor_kind, self.workspace_label.as_deref()) {
            (None, None) => Ok(None), // Explicit compatibility with old servers.
            (None, Some(_)) => Err(Error::new(
                "pairing claim carried workspace_label without actor_kind",
            )),
            (Some(PairingActorKind::General), None) => Ok(Some(PairingIdentityMetadata {
                actor_kind: PairingActorKind::General,
                workspace_label: None,
            })),
            (Some(PairingActorKind::General), Some(_)) => Err(Error::new(
                "pairing claim labelled a general identity as a workspace",
            )),
            // The platform's default first identity is workspace-scoped before
            // it has a path label. Null is therefore authoritative metadata,
            // not a malformed claim: preserve the kind and show it as unbound.
            (Some(PairingActorKind::Workspace), None) => Ok(Some(PairingIdentityMetadata {
                actor_kind: PairingActorKind::Workspace,
                workspace_label: None,
            })),
            (Some(PairingActorKind::Workspace), Some(label)) => {
                if label.is_empty()
                    || label.trim() != label
                    || label.chars().count() > MAX_WORKSPACE_LABEL_CHARS
                    || label.chars().any(char::is_control)
                {
                    return Err(Error::new(
                        "pairing claim carried an invalid workspace_label",
                    ));
                }
                Ok(Some(PairingIdentityMetadata {
                    actor_kind: PairingActorKind::Workspace,
                    workspace_label: Some(label.to_string()),
                }))
            }
        }
    }
}

/// Redacted on purpose: this struct carries two credentials, and a stray
/// `{:?}` in a log or an error is exactly how credentials escape. The identity
/// token is printed as present-or-absent rather than omitted, so a reader can
/// still tell which pairing shape the server used without the value leaking.
impl fmt::Debug for PairingClaim {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PairingClaim")
            .field("device_token", &"<redacted>")
            .field("device_id", &self.device_id)
            .field("actor_id", &self.actor_id)
            .field("actor_name", &self.actor_name)
            .field("mcp_path", &self.mcp_path)
            .field("repointed", &self.repointed)
            .field("actor_kind", &self.actor_kind)
            .field("workspace_label", &self.workspace_label)
            .field(
                "identity_token",
                &if self.identity_token.is_some() {
                    "<redacted>"
                } else {
                    "<none>"
                },
            )
            .finish()
    }
}

/// One queued world turn. Unknown fields are ignored so the platform can add to
/// the payload without breaking installed companions.
#[derive(Debug, Clone, Deserialize)]
pub struct WorldCommand {
    pub id: String,
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub actor_id: Option<String>,
    #[serde(default)]
    pub actor_name: Option<String>,
    /// Optional server-authored turn prompt. Treated as the turn's content, not
    /// as configuration: it cannot change the sandbox, the flags, or the tools.
    #[serde(default)]
    pub prompt: Option<String>,
    /// The visit this command belongs to, when it belongs to one. A companion
    /// still runs bare turns outside any visit, so this stays optional.
    #[serde(default)]
    pub visit_id: Option<String>,
    /// Why a `visit_end` command was queued, in the server's vocabulary. The
    /// runner does not translate this into its own reason: the server saw
    /// something the runner did not, and overwriting it would be a guess.
    #[serde(default)]
    pub end_reason: Option<String>,
    /// The command's payload. `visit_end` carries `{visit_id, end_reason}` here,
    /// so both are read from the payload first and from the top level second —
    /// the platform serves the payload form, and reading both costs nothing.
    #[serde(default)]
    pub payload: Option<serde_json::Value>,
}

impl WorldCommand {
    pub fn visit(&self) -> Option<String> {
        self.payload
            .as_ref()
            .and_then(|payload| payload.get("visit_id"))
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
            .or_else(|| self.visit_id.clone())
    }

    pub fn reason(&self) -> Option<String> {
        self.payload
            .as_ref()
            .and_then(|payload| payload.get("end_reason"))
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
            .or_else(|| self.end_reason.clone())
    }

    pub fn match_outcome(&self) -> Result<Option<MatchOutcome>> {
        let Some(value) = self
            .payload
            .as_ref()
            .and_then(|payload| payload.get("match_outcome"))
        else {
            return Ok(None);
        };
        parse_match_outcome(value, "visit_end command")
    }

    /// An unrecognised kind is not an error — a newer server may queue work
    /// this build cannot do, and the honest response is to report it unrun.
    pub fn command_kind(&self) -> Option<crate::wire::CommandKind> {
        match self.kind.as_deref() {
            None => Some(crate::wire::CommandKind::WorldTurn),
            Some(raw) => crate::wire::CommandKind::parse(raw),
        }
    }
}

/// One identity as the device sees it. The server knows a workspace only by its
/// hash; `workspace_label` is what a human reads.
#[derive(Debug, Clone, Deserialize)]
pub struct IdentitySummary {
    pub identity_id: String,
    pub name: String,
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub workspace_key: Option<String>,
    #[serde(default)]
    pub workspace_label: Option<String>,
    #[serde(default)]
    pub created_at: Option<String>,
}

/// What minting an identity returns. The token is shown exactly once, here.
#[derive(Clone, Deserialize)]
pub struct MintedIdentity {
    pub identity_id: String,
    /// Returned exactly once, at mint, and never by any other route.
    pub token: String,
    pub name: String,
}

/// Same reasoning as `PairingClaim`: this struct carries a credential, and a
/// stray `{:?}` in a log is exactly how credentials escape.
impl fmt::Debug for MintedIdentity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MintedIdentity")
            .field("token", &"<redacted>")
            .field("identity_id", &self.identity_id)
            .field("name", &self.name)
            .finish()
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct StartedVisit {
    pub visit_id: String,
    /// The server answers a second `start` for an identity already at daycare
    /// with 200 and this flag, handing back the visit in progress instead of
    /// refusing. The companion must adopt that visit rather than treat it as a
    /// new one — see `VisitRecord::adopt`.
    #[serde(default)]
    pub already_active: bool,
    /// Turns the server has counted against this visit. Only meaningful
    /// alongside `already_active`, and only used when this machine has no local
    /// record of the visit to carry forward.
    #[serde(default)]
    pub turns_used: Option<u32>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct VisitResponse {
    /// Canonical first terminal reason from the server row. Optional only for
    /// compatibility with platform builds that predate this response field.
    #[serde(default)]
    end_reason: Option<VisitEndReason>,
    #[serde(default)]
    match_outcome_state: Option<MatchOutcomeState>,
    #[serde(default)]
    match_outcome: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MatchOutcomeState {
    Unassigned,
    Pending,
    Ready,
    None,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VisitOutcomeDelivery {
    Pending,
    Ready(MatchOutcome),
    None,
}

impl VisitResponse {
    pub fn end_reason(&self) -> Option<VisitEndReason> {
        self.end_reason
    }

    pub fn match_outcome(&self) -> Result<Option<MatchOutcome>> {
        match self.match_outcome.as_ref() {
            Some(value) => parse_match_outcome(value, "visit response"),
            None => Ok(None),
        }
    }

    /// Resolve the durable state without inferring `none` from elapsed time or
    /// a missing field. Only `ready` may carry an outcome; only `none` permits
    /// a generic homecoming.
    pub fn outcome_delivery(&self) -> Result<VisitOutcomeDelivery> {
        let state = self
            .match_outcome_state
            .ok_or_else(|| Error::new("visit response omitted durable match_outcome_state"))?;
        let outcome = self.match_outcome()?;
        match (state, outcome) {
            (MatchOutcomeState::Pending, None) => Ok(VisitOutcomeDelivery::Pending),
            (MatchOutcomeState::Ready, Some(outcome)) => Ok(VisitOutcomeDelivery::Ready(outcome)),
            (MatchOutcomeState::None, None) => Ok(VisitOutcomeDelivery::None),
            (MatchOutcomeState::Unassigned, _) => Err(Error::new(
                "ended visit remained unassigned instead of resolving outcome delivery",
            )),
            (MatchOutcomeState::Pending, Some(_))
            | (MatchOutcomeState::Ready, None)
            | (MatchOutcomeState::None, Some(_)) => Err(Error::new(
                "visit response carried an outcome inconsistent with match_outcome_state",
            )),
        }
    }
}

fn parse_match_outcome(value: &serde_json::Value, source: &str) -> Result<Option<MatchOutcome>> {
    if value.is_null() {
        return Ok(None);
    }
    let outcome = serde_json::from_value::<MatchOutcome>(value.clone()).map_err(|error| {
        Error::new(format!(
            "{source} carried a malformed match_outcome: {error}"
        ))
    })?;
    if !outcome.is_valid() {
        return Err(Error::new(format!(
            "{source} carried an invalid match_outcome"
        )));
    }
    Ok(Some(outcome))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CompletionStatus {
    Completed,
    Failed,
}

#[derive(Debug, Clone, Serialize)]
pub struct TurnResult {
    pub result_text: Option<String>,
    pub duration_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<TurnUsage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// The turn completed without touching the world: Claude watched, waited,
    /// or declined. Present only when true.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub held: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct CompletionReport {
    pub status: CompletionStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub claude_session_id: Option<String>,
    pub result: TurnResult,
}

pub struct PlatformClient {
    base_url: String,
    agent: ureq::Agent,
}

impl fmt::Debug for PlatformClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PlatformClient")
            .field("base_url", &self.base_url)
            .finish()
    }
}

impl PlatformClient {
    pub fn new(base_url: &str) -> Self {
        PlatformClient {
            base_url: base_url.trim_end_matches('/').to_string(),
            agent: ureq::AgentBuilder::new()
                .timeout_connect(Duration::from_secs(10))
                .timeout(Duration::from_secs(30))
                .user_agent(concat!("daycare-runner/", env!("CARGO_PKG_VERSION")))
                .build(),
        }
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    pub fn url(&self, path: &str) -> String {
        if path.starts_with("http://") || path.starts_with("https://") {
            return path.to_string();
        }
        format!("{}/{}", self.base_url, path.trim_start_matches('/'))
    }

    /// Every request carries the build's release id so the server-side version
    /// floor sees every client. A client-side check alone cannot constrain
    /// binaries shipped before the check existed; the header is what lets the
    /// platform refuse them.
    fn request(&self, method: &str, url: &str) -> ureq::Request {
        let mut request = self.agent.request(method, url);
        if let Some(release) = crate::wire::RELEASE {
            request = request.set(crate::wire::RELEASE_HEADER, release);
        }
        request
    }

    fn get(&self, url: &str) -> ureq::Request {
        self.request("GET", url)
    }

    fn post(&self, url: &str) -> ureq::Request {
        self.request("POST", url)
    }

    /// The release id the site's installer currently pins, from
    /// `releases/current.txt`. `None` when the file is missing, unreachable,
    /// or not a plausible release id — the version floor fails open; only a
    /// confirmed mismatch may stop a runner.
    pub fn current_release(&self) -> Option<String> {
        let response = self.get(&self.url("releases/current.txt")).call().ok()?;
        let text = response.into_string().ok()?;
        let value = text.trim();
        let plausible = !value.is_empty()
            && value.len() <= 64
            && value
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.');
        if !plausible {
            return None;
        }
        Some(value.to_string())
    }

    pub fn claim_pairing(&self, code: &str, device_name: Option<&str>) -> Result<PairingClaim> {
        let body = serde_json::json!({
            "code": code,
            "device_name": device_name,
        });
        let response = self
            .post(&self.url(PAIR_CLAIM_PATH))
            .set("Content-Type", "application/json")
            .send_string(&body.to_string());
        let text = read_body(response, "pairing claim")?;
        serde_json::from_str(&text).map_err(|error| {
            Error::new(format!(
                "pairing response was not the expected shape: {error}"
            ))
        })
    }

    /// `None` means the queue is empty (HTTP 204), which is the normal case.
    pub fn next_command(&self, token: &str) -> Result<Option<WorldCommand>> {
        let response = self
            .get(&self.url(NEXT_COMMAND_PATH))
            .set("Authorization", &format!("Bearer {token}"))
            .call();

        let (status, text) = match response {
            Ok(response) => (
                response.status(),
                response.into_string().unwrap_or_default(),
            ),
            Err(error) => return Err(request_error("next command", error)),
        };

        if status == 204 || text.trim().is_empty() || text.trim() == "null" {
            return Ok(None);
        }

        let value: serde_json::Value = serde_json::from_str(&text)
            .map_err(|error| Error::new(format!("command response was not JSON: {error}")))?;
        // Accept either the bare command or `{ "command": ... }`.
        let payload = match value.get("command") {
            Some(serde_json::Value::Null) | None if value.get("id").is_none() => return Ok(None),
            Some(serde_json::Value::Null) => return Ok(None),
            Some(inner) => inner.clone(),
            None => value,
        };
        let command: WorldCommand = serde_json::from_value(payload).map_err(|error| {
            Error::new(format!(
                "command payload was not the expected shape: {error}"
            ))
        })?;
        Ok(Some(command))
    }

    /// Every identity on this device. Device token, not an identity token: this
    /// is a question about the machine.
    pub fn list_identities(&self, device_token: &str) -> Result<Vec<IdentitySummary>> {
        let response = self
            .get(&self.url(paths::IDENTITIES))
            .set("Authorization", &format!("Bearer {device_token}"))
            .call();
        let text = read_body(response, "identity list")?;
        let value: serde_json::Value = serde_json::from_str(&text)
            .map_err(|error| Error::new(format!("identity list was not JSON: {error}")))?;
        let list = value.get("identities").cloned().unwrap_or(value);
        serde_json::from_value(list).map_err(|error| {
            Error::new(format!("identity list was not the expected shape: {error}"))
        })
    }

    /// Mint a new identity on this device. The returned token is the only copy
    /// the server will ever hand out, so the caller must store it before doing
    /// anything else that can fail.
    pub fn mint_identity(
        &self,
        device_token: &str,
        name: &str,
        kind: &str,
        workspace: Option<&crate::wire::WorkspaceBinding>,
    ) -> Result<MintedIdentity> {
        let mut body = serde_json::json!({ "name": name, "kind": kind });
        if let Some(workspace) = workspace {
            body["workspace_key"] = serde_json::json!(workspace.workspace_key);
            body["workspace_label"] = serde_json::json!(workspace.workspace_label);
        }
        let response = self
            .post(&self.url(paths::IDENTITIES))
            .set("Authorization", &format!("Bearer {device_token}"))
            .set("Content-Type", "application/json")
            .send_string(&body.to_string());
        let text = read_body(response, "identity mint")?;
        serde_json::from_str(&text).map_err(|error| {
            Error::new(format!(
                "identity mint response was not the expected shape: {error}"
            ))
        })
    }

    /// Open a visit for the identity the token belongs to.
    ///
    /// Time and turn safeguards plus the user-facing weekly percentage are
    /// sent so the hub can caption the visit accurately. The runner remains
    /// the percentage enforcer because only it can read Claude's account meter.
    pub fn start_visit(
        &self,
        identity_token: &str,
        budget: &crate::visit::Budget,
        instructions: Option<&str>,
    ) -> Result<StartedVisit> {
        let mut body = serde_json::json!({});
        if let Some(seconds) = budget.wall_clock_secs {
            body["budget_seconds"] = serde_json::json!(seconds);
        }
        if let Some(turns) = budget.turns {
            body["budget_turns"] = serde_json::json!(turns);
        }
        if let Some(weekly_share) = budget.weekly_share {
            body["budget_usage_pct"] = serde_json::json!(weekly_share * 100.0);
        }
        if let Some(instructions) = instructions {
            body["instructions"] = serde_json::json!(instructions);
        }
        let response = self
            .post(&self.url(paths::VISITS))
            .set("Authorization", &format!("Bearer {identity_token}"))
            .set("Content-Type", "application/json")
            .send_string(&body.to_string());
        let text = read_body(response, "visit start")?;
        serde_json::from_str(&text).map_err(|error| {
            Error::new(format!(
                "visit start response was not the expected shape: {error}"
            ))
        })
    }

    /// Close a visit. The body carries the reason and nothing else: the route's
    /// schema is strict, and an extra field is exactly the silent refusal this
    /// team has already lost a day to.
    pub fn end_visit(
        &self,
        identity_token: &str,
        visit_id: &str,
        reason: VisitEndReason,
    ) -> Result<VisitResponse> {
        let body = serde_json::json!({ "end_reason": reason });
        let response = self
            .post(&self.url(&paths::end_visit(visit_id)))
            .set("Authorization", &format!("Bearer {identity_token}"))
            .set("Content-Type", "application/json")
            .send_string(&body.to_string());
        let text = read_body(response, "visit end")?;
        serde_json::from_str(&text).map_err(|error| {
            Error::new(format!(
                "visit end response was not the expected shape: {error}"
            ))
        })
    }

    /// Deliver the owner-facing day report the Claude wrote at homecoming.
    ///
    /// Write-once on the server: an identical retry succeeds, a different text
    /// against a stored report is refused with 409 — which this treats as
    /// delivered, because a report already stands and re-sending cannot and
    /// should not replace it.
    pub fn submit_day_report(
        &self,
        identity_token: &str,
        visit_id: &str,
        report: &str,
    ) -> Result<()> {
        let body = serde_json::json!({ "report": report });
        let response = self
            .post(&self.url(&paths::visit_report(visit_id)))
            .set("Authorization", &format!("Bearer {identity_token}"))
            .set("Content-Type", "application/json")
            .send_string(&body.to_string());
        match response {
            Err(ureq::Error::Status(409, _)) => Ok(()),
            other => {
                read_body(other, "day report")?;
                Ok(())
            }
        }
    }

    /// Upload the full rendered transcript the homecoming reader read, so the
    /// owner can read the record on the hub. Idempotent on the server (an
    /// identical re-post after a retried homecoming is fine).
    pub fn submit_transcript(
        &self,
        identity_token: &str,
        visit_id: &str,
        transcript: &str,
    ) -> Result<()> {
        let body = serde_json::json!({ "transcript": transcript });
        let response = self
            .post(&self.url(&paths::visit_transcript(visit_id)))
            .set("Authorization", &format!("Bearer {identity_token}"))
            .set("Content-Type", "application/json")
            .send_string(&body.to_string());
        read_body(response, "visit transcript")?;
        Ok(())
    }

    pub fn get_visit(&self, identity_token: &str, visit_id: &str) -> Result<VisitResponse> {
        let response = self
            .get(&self.url(&paths::visit(visit_id)))
            .set("Authorization", &format!("Bearer {identity_token}"))
            .call();
        let text = read_body(response, "visit status")?;
        serde_json::from_str(&text).map_err(|error| {
            Error::new(format!(
                "visit status response was not the expected shape: {error}"
            ))
        })
    }

    pub fn complete_command(
        &self,
        token: &str,
        command_id: &str,
        report: &CompletionReport,
    ) -> Result<()> {
        let path = format!("/api/daycare/commands/{command_id}/complete");
        let body = serde_json::to_string(report)?;
        let response = self
            .post(&self.url(&path))
            .set("Authorization", &format!("Bearer {token}"))
            .set("Content-Type", "application/json")
            .send_string(&body);
        read_body(response, "command completion")?;
        Ok(())
    }

    /// Every memory this identity has saved, read straight off the Daycare MCP
    /// endpoint.
    ///
    /// Q10 was blocked for a while on a REST route to list memories, because
    /// `daycare_memory_list` is an MCP tool and the runner is an MCP *host*
    /// rather than a client. That framing was the error: the endpoint is plain
    /// HTTP JSON-RPC and the runner already holds the two things it wants —
    /// the identity token and the MCP URL. Nothing new was needed on the
    /// server, and no route had to be waited for.
    ///
    /// The response is server-sent events carrying one JSON-RPC message, whose
    /// result content is itself a JSON document. The tool caps each response,
    /// so this walks deterministic pages and checks the server's total on every
    /// page. A concurrent change or duplicate aborts the export before the
    /// caller can replace a good local mirror with a partial one.
    pub fn list_memories(&self, mcp_url: &str, identity_token: &str) -> Result<Vec<Memory>> {
        collect_memory_pages(|offset, limit| {
            self.memory_page(mcp_url, identity_token, offset, limit)
        })
    }

    fn memory_page(
        &self,
        mcp_url: &str,
        identity_token: &str,
        offset: usize,
        limit: usize,
    ) -> Result<MemoryPage> {
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {
                "name": "daycare_memory_list",
                "arguments": { "limit": limit, "offset": offset },
            },
        });
        let response = self
            .post(&self.url(mcp_url))
            .set("Authorization", &format!("Bearer {identity_token}"))
            .set("Content-Type", "application/json")
            .set("Accept", "application/json, text/event-stream")
            .send_string(&body.to_string());
        let text = read_body(response, "memory list")?;
        parse_memory_page(&text)
    }
}

fn collect_memory_pages(
    mut fetch: impl FnMut(usize, usize) -> Result<MemoryPage>,
) -> Result<Vec<Memory>> {
    const PAGE_SIZE: usize = 50;
    let mut memories = Vec::new();
    let mut seen = HashSet::new();
    let mut expected_total = None;

    loop {
        let offset = memories.len();
        let page = fetch(offset, PAGE_SIZE)?;
        if page.offset != offset {
            return Err(Error::new(format!(
                "memory list returned offset {} while {} was requested",
                page.offset, offset
            )));
        }
        match expected_total {
            Some(total) if total != page.total => {
                return Err(Error::new(
                    "memories changed during local sync; the previous local copy was kept",
                ));
            }
            None => expected_total = Some(page.total),
            _ => {}
        }
        if page.memories.len() > PAGE_SIZE {
            return Err(Error::new("memory list returned an oversized page"));
        }
        for memory in page.memories {
            if !seen.insert(memory.id.clone()) {
                return Err(Error::new(
                    "memory list repeated a row; the previous local copy was kept",
                ));
            }
            memories.push(memory);
        }

        let total = expected_total.unwrap_or(0);
        if memories.len() == total {
            return Ok(memories);
        }
        if memories.len() > total || memories.len() == offset {
            return Err(Error::new(format!(
                "memory list ended at {} of {total}; the previous local copy was kept",
                memories.len()
            )));
        }
    }
}

/// One memory as the server keeps it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Memory {
    pub id: String,
    pub text: String,
    pub created_at: String,
}

#[derive(Debug, PartialEq, Eq, Deserialize)]
struct MemoryPage {
    total: usize,
    offset: usize,
    memories: Vec<Memory>,
}

/// Unwrap SSE → JSON-RPC → tool content → `{ "memories": [...] }`.
///
/// Each layer is checked rather than assumed. A tool error arrives as a normal
/// result with `isError`, so it has to be looked for explicitly or a failed
/// read would sync an empty list over a good local copy.
fn parse_memory_page(body: &str) -> Result<MemoryPage> {
    let payload = body
        .lines()
        .filter_map(|line| line.strip_prefix("data:"))
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or(body.trim());

    let message: serde_json::Value = serde_json::from_str(payload)
        .map_err(|error| Error::new(format!("memory list was not JSON-RPC: {error}")))?;

    if let Some(error) = message.get("error") {
        return Err(Error::new(format!("memory list refused: {error}")));
    }
    let result = message
        .get("result")
        .ok_or_else(|| Error::new("memory list carried no result"))?;
    if result.get("isError").and_then(serde_json::Value::as_bool) == Some(true) {
        return Err(Error::new("the memory tool reported an error"));
    }
    let text = result
        .get("content")
        .and_then(serde_json::Value::as_array)
        .and_then(|items| items.first())
        .and_then(|item| item.get("text"))
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| Error::new("memory list carried no text content"))?;

    let document: serde_json::Value = serde_json::from_str(text)
        .map_err(|error| Error::new(format!("memory list content was not JSON: {error}")))?;
    serde_json::from_value(document).map_err(|error| {
        Error::new(format!(
            "memory list page was not the expected shape: {error}"
        ))
    })
}

fn read_body(
    response: std::result::Result<ureq::Response, ureq::Error>,
    what: &str,
) -> Result<String> {
    match response {
        Ok(response) => Ok(response.into_string()?),
        Err(error) => Err(request_error(what, error)),
    }
}

/// Errors must be safe to print and safe to send back to the platform, so this
/// reports status and a short body excerpt — never the request headers.
fn request_error(what: &str, error: ureq::Error) -> Error {
    match error {
        ureq::Error::Status(401, response) => {
            let body = response.into_string().unwrap_or_default();
            let excerpt: String = body.chars().take(200).collect();
            Error::new(format!(
                "{what} failed: HTTP 401 {excerpt}\n\
                 This machine's credential is no longer accepted. The usual cause is \
                 that this Claude was brought to another computer — re-pairing moves \
                 the identity and rotates its token, which retires this copy. Pair \
                 again here to bring it back. (It can also mean the Claude or this \
                 device was retired from the hub.)"
            ))
        }
        ureq::Error::Status(code, response) => {
            let body = response.into_string().unwrap_or_default();
            // A refusal the platform wrote in words is printed as those words;
            // only a body without a sentence falls back to the status line.
            let sentence = serde_json::from_str::<serde_json::Value>(&body)
                .ok()
                .and_then(|value| value.get("error")?.as_str().map(str::to_owned))
                .filter(|text| !text.trim().is_empty());
            let message = match sentence {
                Some(text) => format!("{what} failed: {text} (HTTP {code})"),
                None => {
                    let excerpt: String = body.chars().take(200).collect();
                    format!("{what} failed: HTTP {code} {excerpt}")
                }
            };
            Error::new(message).with_status(code)
        }
        ureq::Error::Transport(transport) => {
            Error::transport(format!("{what} failed: {transport}"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_join_without_doubling_slashes() {
        let client = PlatformClient::new("https://example.test/");
        assert_eq!(
            client.url("/api/daycare/pair/claim"),
            "https://example.test/api/daycare/pair/claim"
        );
        assert_eq!(
            client.url("api/daycare/commands/next"),
            "https://example.test/api/daycare/commands/next"
        );
        assert_eq!(
            client.url("https://other.test/mcp"),
            "https://other.test/mcp"
        );
    }

    #[test]
    fn pairing_claim_debug_redacts_the_token() {
        let claim = PairingClaim {
            device_token: "dev_super_secret_value".into(),
            device_id: "device-1".into(),
            actor_id: "actor-1".into(),
            actor_name: "Pip".into(),
            mcp_path: "/api/daycare/mcp".into(),
            repointed: false,
            identity_token: Some("ident_super_secret_value".into()),
            actor_kind: Some(PairingActorKind::Workspace),
            workspace_label: Some("voices-of-history".into()),
        };
        let rendered = format!("{claim:?}");
        assert!(!rendered.contains("super_secret"), "{rendered}");
        assert!(rendered.contains("<redacted>"));
        assert!(rendered.contains("actor-1"));
        assert!(rendered.contains("Workspace"));
        assert!(rendered.contains("voices-of-history"));
    }

    #[test]
    fn pairing_claim_profile_metadata_is_validated_as_one_unit() {
        let workspace: PairingClaim = serde_json::from_str(
            r#"{"device_token":"secret","device_id":"d1","actor_id":"a1",
                "actor_name":"Pip","mcp_path":"/mcp","actor_kind":"workspace",
                "workspace_label":"voices-of-history"}"#,
        )
        .unwrap();
        assert_eq!(
            workspace.identity_metadata().unwrap(),
            Some(PairingIdentityMetadata {
                actor_kind: PairingActorKind::Workspace,
                workspace_label: Some("voices-of-history".into()),
            })
        );

        let old_server: PairingClaim = serde_json::from_str(
            r#"{"device_token":"secret","device_id":"d1","actor_id":"a1",
                "actor_name":"Pip","mcp_path":"/mcp"}"#,
        )
        .unwrap();
        assert_eq!(old_server.identity_metadata().unwrap(), None);

        let unlabeled_workspace: PairingClaim = serde_json::from_str(
            r#"{"device_token":"secret","device_id":"d1","actor_id":"a1",
                "actor_name":"Pip","mcp_path":"/mcp","actor_kind":"workspace",
                "workspace_label":null}"#,
        )
        .unwrap();
        assert_eq!(
            unlabeled_workspace.identity_metadata().unwrap(),
            Some(PairingIdentityMetadata {
                actor_kind: PairingActorKind::Workspace,
                workspace_label: None,
            })
        );

        for malformed in [
            r#"{"device_token":"secret","device_id":"d1","actor_id":"a1","actor_name":"Pip","mcp_path":"/mcp","workspace_label":"orphan"}"#,
            r#"{"device_token":"secret","device_id":"d1","actor_id":"a1","actor_name":"Pip","mcp_path":"/mcp","actor_kind":"general","workspace_label":"wrong"}"#,
        ] {
            let claim: PairingClaim = serde_json::from_str(malformed).unwrap();
            assert!(claim.identity_metadata().is_err(), "accepted {malformed}");
        }
    }

    #[test]
    fn pairing_workspace_label_uses_the_servers_eighty_character_contract() {
        let claim_with = |label: String| {
            serde_json::from_value::<PairingClaim>(serde_json::json!({
                "device_token": "secret",
                "device_id": "d1",
                "actor_id": "a1",
                "actor_name": "Pip",
                "mcp_path": "/mcp",
                "actor_kind": "workspace",
                "workspace_label": label,
            }))
            .unwrap()
        };

        // `é` is two UTF-8 bytes. The server caps characters, not wire bytes.
        let eighty_multibyte = "é".repeat(80);
        let metadata = claim_with(eighty_multibyte.clone())
            .identity_metadata()
            .unwrap()
            .unwrap();
        assert_eq!(
            metadata.workspace_label.as_deref(),
            Some(eighty_multibyte.as_str())
        );
        assert!(claim_with("é".repeat(81)).identity_metadata().is_err());
        assert!(claim_with("a".repeat(81)).identity_metadata().is_err());
    }

    #[test]
    fn a_visit_end_command_is_read_from_the_payload_the_platform_serves() {
        let command: WorldCommand = serde_json::from_str(
            r#"{"id":"cmd-1","kind":"visit_end",
                "payload":{"visit_id":"visit-1","end_reason":"activity_ended"}}"#,
        )
        .unwrap();
        assert_eq!(
            command.command_kind(),
            Some(crate::wire::CommandKind::VisitEnd)
        );
        assert_eq!(command.visit().as_deref(), Some("visit-1"));
        assert_eq!(command.reason().as_deref(), Some("activity_ended"));
    }

    #[test]
    fn a_visit_end_reads_only_a_relative_validated_match_outcome() {
        let command: WorldCommand = serde_json::from_str(
            r#"{"id":"cmd-1","kind":"visit_end","payload":{
                "visit_id":"visit-1","end_reason":"activity_ended",
                "match_outcome":{"kind":"debate_league","result":"lost",
                "winner":"opponent","board":{"yours":7,"opponent":10},
                "verdictCompletedAt":"2026-08-08T18:45:00.000Z",
                "summary":"You lost the Debate League match, 7–10 on the final board."}}}"#,
        )
        .unwrap();

        let outcome = command
            .match_outcome()
            .expect("valid payload")
            .expect("relative outcome");
        assert_eq!(outcome.board.yours, 7);
        assert_eq!(outcome.board.opponent, 10);

        let leaked: WorldCommand = serde_json::from_str(
            r#"{"id":"cmd-2","kind":"visit_end","payload":{
                "match_outcome":{"kind":"debate_league","result":"lost",
                "winner":"opponent","board":{"yours":7,"opponent":10},
                "verdictCompletedAt":"2026-08-08T18:45:00.000Z",
                "summary":"You lost the Debate League match, 7–10 on the final board.",
                "opponent_actor_id":"stable-id"}}}"#,
        )
        .unwrap();
        assert!(leaked.match_outcome().is_err());
    }

    #[test]
    fn durable_visit_state_never_infers_none_from_null_or_omission() {
        let pending: VisitResponse =
            serde_json::from_str(r#"{"match_outcome_state":"pending","match_outcome":null}"#)
                .unwrap();
        assert_eq!(
            pending.outcome_delivery().unwrap(),
            VisitOutcomeDelivery::Pending
        );

        let none: VisitResponse =
            serde_json::from_str(r#"{"match_outcome_state":"none","match_outcome":null}"#).unwrap();
        assert_eq!(none.outcome_delivery().unwrap(), VisitOutcomeDelivery::None);

        let omitted: VisitResponse = serde_json::from_str(r#"{"match_outcome":null}"#).unwrap();
        assert!(omitted.outcome_delivery().is_err());

        let malformed: VisitResponse =
            serde_json::from_str(r#"{"match_outcome_state":"ready","match_outcome":null}"#)
                .unwrap();
        assert!(malformed.outcome_delivery().is_err());
    }

    #[test]
    fn memory_list_unwraps_the_mcp_sse_response() {
        let page = parse_memory_page(
            r#"event: message
data: {"jsonrpc":"2.0","id":1,"result":{"content":[{"type":"text","text":"{\"total\":1,\"offset\":0,\"memories\":[{\"id\":\"m-1\",\"text\":\"I found the chalk.\",\"created_at\":\"2026-08-07T06:00:00Z\"}]}"}]}}
"#,
        )
        .unwrap();
        assert_eq!(page.total, 1);
        assert_eq!(page.offset, 0);
        assert_eq!(page.memories[0].id, "m-1");
        assert_eq!(page.memories[0].text, "I found the chalk.");
    }

    #[test]
    fn memory_list_refuses_a_tool_error_instead_of_syncing_empty() {
        let error = parse_memory_page(
            r#"{"jsonrpc":"2.0","id":1,"result":{"isError":true,"content":[{"type":"text","text":"refused"}]}}"#,
        )
        .unwrap_err();
        assert!(error.message().contains("reported an error"), "{error}");
    }

    #[test]
    fn memory_export_reads_every_page_without_truncating_at_fifty() {
        let mut requests = Vec::new();
        let memories = collect_memory_pages(|offset, limit| {
            requests.push((offset, limit));
            let end = usize::min(offset + limit, 51);
            Ok(MemoryPage {
                total: 51,
                offset,
                memories: (offset..end)
                    .map(|index| Memory {
                        id: format!("m-{index:02}"),
                        text: format!("memory {index}"),
                        created_at: "2026-08-07T06:00:00Z".into(),
                    })
                    .collect(),
            })
        })
        .unwrap();

        assert_eq!(requests, vec![(0, 50), (50, 50)]);
        assert_eq!(memories.len(), 51);
        assert_eq!(memories.last().unwrap().id, "m-50");
    }

    #[test]
    fn memory_export_keeps_the_old_mirror_when_the_total_changes_mid_sync() {
        let error = collect_memory_pages(|offset, _| {
            Ok(MemoryPage {
                total: if offset == 0 { 51 } else { 50 },
                offset,
                memories: if offset == 0 {
                    (0..50)
                        .map(|index| Memory {
                            id: format!("m-{index:02}"),
                            text: format!("memory {index}"),
                            created_at: "2026-08-07T06:00:00Z".into(),
                        })
                        .collect()
                } else {
                    Vec::new()
                },
            })
        })
        .unwrap_err();

        assert!(
            error.message().contains("changed during local sync"),
            "{error}"
        );
    }

    #[test]
    fn a_command_kind_this_build_has_never_heard_of_parses_without_panicking() {
        // The platform asked directly whether an unknown kind breaks the loop.
        // It does not: it parses, reports as unrecognised, and the caller
        // completes it failed rather than leaving it claimed forever.
        let command: WorldCommand =
            serde_json::from_str(r#"{"id":"cmd-2","kind":"join_match","payload":{}}"#).unwrap();
        assert_eq!(command.command_kind(), None);
        // A slice-1 command with no kind at all is still a world turn.
        let bare: WorldCommand = serde_json::from_str(r#"{"id":"cmd-3"}"#).unwrap();
        assert_eq!(
            bare.command_kind(),
            Some(crate::wire::CommandKind::WorldTurn)
        );
        assert_eq!(bare.visit(), None);
    }

    #[test]
    fn completion_report_serializes_the_agreed_shape() {
        let report = CompletionReport {
            status: CompletionStatus::Completed,
            claude_session_id: Some("550e8400-e29b-41d4-a716-446655440000".into()),
            result: TurnResult {
                result_text: Some("looked around".into()),
                duration_ms: Some(2493),
                usage: None,
                error: None,
                held: false,
            },
        };
        let value: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&report).unwrap()).unwrap();
        assert_eq!(value["status"], "completed");
        assert!(value["result"].get("held").is_none());
        assert_eq!(
            value["claude_session_id"],
            "550e8400-e29b-41d4-a716-446655440000"
        );
        assert_eq!(value["result"]["result_text"], "looked around");
        assert_eq!(value["result"]["duration_ms"], 2493);
        assert!(value["result"].get("usage").is_none());
    }

    #[test]
    fn held_report_is_completed_and_says_so() {
        let report = CompletionReport {
            status: CompletionStatus::Completed,
            claude_session_id: None,
            result: TurnResult {
                result_text: Some("Nothing needs me this turn.".into()),
                duration_ms: Some(900),
                usage: None,
                error: None,
                held: true,
            },
        };
        let value: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&report).unwrap()).unwrap();
        assert_eq!(value["status"], "completed");
        assert_eq!(value["result"]["held"], true);
        assert!(value["result"].get("error").is_none());
    }

    #[test]
    fn failed_report_carries_a_reason_and_no_success_text() {
        let report = CompletionReport {
            status: CompletionStatus::Failed,
            claude_session_id: None,
            result: TurnResult {
                result_text: None,
                duration_ms: Some(300_000),
                usage: None,
                error: Some("turn exceeded 300s and was killed".into()),
                held: false,
            },
        };
        let value: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&report).unwrap()).unwrap();
        assert_eq!(value["status"], "failed");
        assert!(value["result"]["error"].as_str().unwrap().contains("300s"));
        assert!(value.get("claude_session_id").is_none());
    }

    #[test]
    fn a_refusal_written_in_words_is_printed_as_those_words_with_its_status() {
        let response = ureq::Response::new(
            409,
            "Conflict",
            r#"{"error":"The previous visit still has a recall waiting to be acknowledged.","reason":"prior_delivery_pending"}"#,
        )
        .unwrap();
        let error = request_error("visit start", ureq::Error::Status(409, response));
        assert_eq!(error.http_status(), Some(409));
        assert_eq!(
            error.message(),
            "visit start failed: The previous visit still has a recall waiting to be acknowledged. (HTTP 409)"
        );
    }

    #[test]
    fn a_wordless_failure_still_names_the_status() {
        let response = ureq::Response::new(500, "Internal Server Error", "").unwrap();
        let error = request_error("visit start", ureq::Error::Status(500, response));
        assert_eq!(error.http_status(), Some(500));
        assert_eq!(error.message(), "visit start failed: HTTP 500 ");
    }
}
