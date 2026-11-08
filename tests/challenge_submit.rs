//! `challenge submit`, end to end (plan tasks N14–N16): the listing, the
//! local secret scan, and the signed uploads, against a mock platform that
//! plays the submit, storage and confirm routes.
//!
//! Most cases drive the library's `submit::run` with a scripted prompt, so
//! they run on every platform; the binary is checked for what it refuses
//! before any request, and (macOS) for the whole flow on a real terminal. No
//! network, no keychain, no model, and never the real `~/.claude-daycare`.

mod support;

use daycare_runner::approval::scan::Roots;
use daycare_runner::approval::Prompt;
use daycare_runner::challenge::scan::{CredentialFiles, FindingKind, SecretSet};
use daycare_runner::challenge::submit::{
    self, gather_secrets, ChallengeFolder, SecretSources, SubmitRequest,
};
use daycare_runner::keychain::FileTokenStore;
use daycare_runner::paths::Layout;
use daycare_runner::platform::{PlatformClient, SignedUploader};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::{Arc, Mutex, OnceLock};
use support::{MockPlatform, RecordedRequest, Response};

const BIN: &str = env!("CARGO_BIN_EXE_daycare-runner");
const TOKEN: &str = "dev_token_do_not_leak_9876";
const UPLOAD_GRANT: &str = "signed-upload-grant-5150";
const ENTRY_ID: &str = "entry-1";

fn sha256(bytes: &[u8]) -> String {
    daycare_runner::self_update::sha256_hex(bytes)
}

/// What the mock platform does, and what it saw.
#[derive(Default)]
struct World {
    base: String,
    state: String,
    versions: u64,
    /// Answer the POST with this status and body instead.
    submit_refusal: Option<(u16, &'static str)>,
    /// Answer the confirm with this status and body instead.
    confirm_refusal: Option<(u16, &'static str)>,
    /// Drop the confirm's connection without an answer.
    confirm_lost: bool,
}

fn mock_platform() -> (MockPlatform, Arc<Mutex<World>>) {
    let world = Arc::new(Mutex::new(World {
        state: "open".into(),
        ..World::default()
    }));
    let seen = Arc::clone(&world);
    let base: Arc<OnceLock<String>> = Arc::new(OnceLock::new());
    let seen_base = Arc::clone(&base);
    let platform = MockPlatform::start(move |request: &RecordedRequest| {
        let mut world = seen.lock().unwrap();
        world.base = seen_base.get().cloned().unwrap_or_default();
        handle(&mut world, request)
    });
    base.set(platform.base_url.clone()).unwrap();
    (platform, world)
}

fn handle(world: &mut World, request: &RecordedRequest) -> Response {
    let path = request.path.as_str();
    let authorized = request.authorization() == Some(&format!("Bearer {TOKEN}"));
    if path.ends_with("/pair/claim") {
        return Response::json(
            200,
            &format!(
                r#"{{"device_token":"{TOKEN}","device_id":"device-1",
                    "actor_id":"actor-1","actor_name":"Pip","mcp_path":"/api/daycare/mcp"}}"#
            ),
        );
    }
    if request.method == "PUT" && path.starts_with("/storage/v1/object/upload/sign/") {
        // The signed URL is its own grant: no Daycare credential is expected.
        return if path.contains(UPLOAD_GRANT) && request.authorization().is_none() {
            Response::json(200, r#"{"Key":"daycare-challenge-artifacts/x"}"#)
        } else {
            Response::json(403, r#"{"error":"bad grant"}"#)
        };
    }
    if request.method == "GET" && path.starts_with("/storage/v1/object/sign/skills/") {
        // A signed read link: its own grant, no Daycare credential.
        let file = path.split('?').next().unwrap().rsplit('/').next().unwrap();
        let version = path.split('/').nth(7).unwrap_or_default();
        return match (file, version) {
            ("SKILL.md", _) => Response::json(200, SKILL_MD),
            // Version 4 serves a file that does not match its declared hash.
            ("a.md", "4") => Response::json(200, "tampered"),
            ("a.md", _) => Response::json(200, REF_MD),
            _ => Response::json(404, "{}"),
        };
    }
    if !authorized {
        return Response::json(401, r#"{"error":"Unauthorized"}"#);
    }
    if request.method == "GET" && path.starts_with("/api/daycare/skills/") {
        return skill_version(world, path);
    }
    if request.method == "GET" && path == "/api/daycare/challenges/c-1" {
        return Response::json(
            200,
            &serde_json::json!({
                "id": "c-1", "slug": "signup-rescue", "title": "Signup Rescue",
                "state": world.state, "closes_at": "2099-11-20T00:00:00Z",
                "limits": {
                    "upload_folder": "output", "max_files": 100,
                    "max_file_bytes": 26_214_400, "max_total_bytes": 47_185_920,
                    "test_budget": null, "allowed_extensions": null,
                },
                "inputs": [],
            })
            .to_string(),
        );
    }
    if request.method == "POST" && path == "/api/daycare/challenges/c-1/submissions" {
        if let Some((status, body)) = world.submit_refusal {
            return Response::json(status, body);
        }
        let body = request.json();
        world.versions += 1;
        let version = world.versions;
        let uploads: Vec<serde_json::Value> = body["files"]
            .as_array()
            .unwrap()
            .iter()
            .map(|file| {
                let path = file["path"].as_str().unwrap();
                let storage = format!("entries/{ENTRY_ID}/v{version}/{path}");
                serde_json::json!({
                    "path": path,
                    "storage_path": storage,
                    "signed_url": format!(
                        "{}/storage/v1/object/upload/sign/daycare-challenge-artifacts/{storage}?token={UPLOAD_GRANT}",
                        world.base
                    ),
                    "token": UPLOAD_GRANT,
                })
            })
            .collect();
        return Response::json(
            201,
            &serde_json::json!({
                "submission_id": format!("sub-{version}"),
                "entry_id": ENTRY_ID,
                "version": version,
                "uploads": uploads,
            })
            .to_string(),
        );
    }
    if request.method == "POST"
        && path.starts_with("/api/daycare/challenges/c-1/submissions/")
        && path.ends_with("/confirm")
    {
        if let Some((status, body)) = world.confirm_refusal {
            return Response::json(status, body);
        }
        if world.confirm_lost {
            return Response::disconnect();
        }
        return Response::json(
            200,
            &format!(
                r#"{{"ok":true,"entry_id":"{ENTRY_ID}","version":{}}}"#,
                world.versions
            ),
        );
    }
    Response::json(404, r#"{"error":"not here"}"#)
}

const SKILL_MD: &str = "---\nname: tidy-csv\ndescription: Tidy a CSV\n---\nTidy it.\n";
const REF_MD: &str = "# Reference\n";

/// Stream K's install API: version 3 is good, 4 is tampered in storage, 2 was
/// withdrawn (410), and `paid-skill` is not the owner's (403).
fn skill_version(world: &World, path: &str) -> Response {
    let parts: Vec<&str> = path
        .trim_start_matches("/api/daycare/skills/")
        .split('/')
        .collect();
    match parts.as_slice() {
        ["tidy-csv", "versions", "2"] => Response::json(410, r#"{"error":"revoked"}"#),
        ["paid-skill", "versions", _] => Response::json(403, r#"{"error":"no entitlement"}"#),
        ["tidy-csv", "versions", version @ ("3" | "4")] => {
            let file = |name: &str, body: &str| {
                serde_json::json!({
                    "path": if name == "a.md" { "ref/a.md" } else { name },
                    "bytes": body.len(),
                    "sha256": sha256(body.as_bytes()),
                    "url": format!(
                        "{}/storage/v1/object/sign/skills/tidy-csv/{version}/{name}?token=read-grant",
                        world.base
                    ),
                })
            };
            Response::json(
                200,
                &serde_json::json!({
                    "name": "tidy-csv", "version": version, "licence": "CC-BY-4.0",
                    "files": [file("SKILL.md", SKILL_MD), file("a.md", REF_MD)],
                })
                .to_string(),
            )
        }
        _ => Response::json(404, r#"{"error":"not here"}"#),
    }
}

/// The file-list sha256 of version 3, computed here from the canonical form
/// rather than by the runner's own function.
fn tidy_csv_list_sha256() -> String {
    sha256(
        format!(
            r#"[{{"path":"SKILL.md","bytes":{},"sha256":"{}"}},{{"path":"ref/a.md","bytes":{},"sha256":"{}"}}]"#,
            SKILL_MD.len(),
            sha256(SKILL_MD.as_bytes()),
            REF_MD.len(),
            sha256(REF_MD.as_bytes())
        )
        .as_bytes(),
    )
}

/// A scratch machine: Daycare state, a fake HOME, and a challenge root with
/// one challenge folder as `challenge start` leaves it.
struct Machine {
    daycare_home: PathBuf,
    fake_home: PathBuf,
    token_file: PathBuf,
    path: String,
}

impl Machine {
    fn new(label: &str) -> Machine {
        let root = support::scratch_dir(label);
        let fake_home = root.join("fake-home");
        std::fs::create_dir_all(&fake_home).unwrap();
        let machine = Machine {
            daycare_home: root.join("daycare-state"),
            token_file: root.join("tokens.json"),
            path: support::no_claude_path(&root),
            fake_home,
        };
        machine.make_folder();
        machine
    }

    fn challenge_root(&self) -> PathBuf {
        self.fake_home.join("daycare/challenges")
    }

    fn folder(&self) -> PathBuf {
        self.challenge_root().join("c-1")
    }

    fn make_folder(&self) {
        std::fs::create_dir_all(self.folder().join("output")).unwrap();
        std::fs::write(
            self.folder().join("ENTRY.json"),
            serde_json::json!({
                "version": 1, "challenge_id": "c-1", "challenge_slug": "signup-rescue",
                "title": "Signup Rescue", "entry_id": ENTRY_ID, "actor_id": "actor-1",
            })
            .to_string(),
        )
        .unwrap();
    }

    fn put_output(&self, relative: &str, text: &str) -> PathBuf {
        let path = self.folder().join("output").join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, text).unwrap();
        path
    }

    /// Three files, one in a subfolder.
    fn put_entry(&self) {
        self.put_output(
            "entry.html",
            "<!doctype html><form><label>Email</label></form>\n",
        );
        self.put_output("NOTES.md", "# Notes\nI fixed the labels.\n");
        self.put_output("data/out.csv", "name,email\nAda,ada@example.test\n");
    }

    fn command(&self) -> Command {
        let mut command = Command::new(BIN);
        command
            .env("DAYCARE_HOME", &self.daycare_home)
            .env("DAYCARE_TOKEN_FILE", &self.token_file)
            .env("DAYCARE_SKIP_CLAUDE_PREFLIGHT", "1")
            .env("DAYCARE_CHALLENGE_ROOT", self.challenge_root())
            .env("PATH", &self.path)
            .env("HOME", &self.fake_home)
            .env_remove("DAYCARE_WORKSPACE_ROOT")
            .env_remove("DAYCARE_CHALLENGE_SESSION")
            .env_remove("CLAUDE_CONFIG_DIR")
            .env_remove("CODEX_HOME")
            .env_remove("XDG_CONFIG_HOME")
            .env_remove("XDG_DATA_HOME")
            .env_remove("ANTHROPIC_API_KEY")
            .stdin(Stdio::null());
        command
    }

    fn run(&self, args: &[&str]) -> Output {
        self.command()
            .args(args)
            .output()
            .expect("run daycare-runner")
    }

    fn enroll(&self, platform: &MockPlatform) {
        let out = self.run(&["enroll", "--url", &platform.base_url, "--code", "PAIR-1234"]);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// The secret sources `challenge submit` uses, all inside this machine.
    fn secrets(&self) -> SecretSet {
        let layout = Layout::at(&self.daycare_home);
        let store = FileTokenStore::new(self.token_file.clone());
        let roots = Roots {
            home: self.fake_home.clone(),
            claude_config_dir: self.fake_home.join(".claude"),
            claude_json: self.fake_home.join(".claude.json"),
            codex_home: self.fake_home.join(".codex"),
            agents_home: self.fake_home.join(".agents"),
            challenge_root: self.challenge_root(),
            daycare_root: self.daycare_home.clone(),
        };
        let credentials = CredentialFiles {
            daycare_token_files: vec![self.token_file.clone()],
            codex_auth: self.fake_home.join(".codex/auth.json"),
            opencode_auth: self.fake_home.join(".local/share/opencode/auth.json"),
            claude_credentials: self.fake_home.join(".claude/.credentials.json"),
        };
        gather_secrets(SecretSources {
            layout: &layout,
            store: &store,
            identity_token: TOKEN,
            roots: &roots,
            credentials: &credentials,
        })
        .unwrap()
    }

    fn entry_json(&self) -> serde_json::Value {
        serde_json::from_slice(&std::fs::read(self.folder().join("ENTRY.json")).unwrap()).unwrap()
    }
}

struct Scripted(Vec<&'static str>);

impl Prompt for Scripted {
    fn ask(&mut self, _question: &str) -> Option<String> {
        (!self.0.is_empty()).then(|| format!("{}\n", self.0.remove(0)))
    }
}

/// The answers a person gives: a little direction, one tool, yes.
fn answers() -> Scripted {
    Scripted(vec!["light", "Claude Code, a browser", "yes"])
}

/// Run one submit through the library, as the CLI does after its checks.
fn submit_with(
    machine: &Machine,
    platform: &MockPlatform,
    secrets: &SecretSet,
    prompt: &mut dyn Prompt,
) -> (daycare_runner::Result<submit::Submitted>, String) {
    let client = PlatformClient::new(&platform.base_url);
    let uploader = SignedUploader::new();
    let folder =
        ChallengeFolder::locate(Some("c-1"), &machine.challenge_root(), Path::new("/")).unwrap();
    let mut screen = Vec::new();
    let result = submit::run(
        SubmitRequest {
            platform: &client,
            uploader: &uploader,
            token: TOKEN,
            folder,
            secrets,
            submitted_at: "2026-11-21T12:00:00Z",
        },
        prompt,
        &mut screen,
    );
    (result, String::from_utf8_lossy(&screen).into_owned())
}

fn requests_of(platform: &MockPlatform) -> Vec<RecordedRequest> {
    platform
        .requests()
        .into_iter()
        .filter(|request| !request.path.ends_with("/pair/claim"))
        .collect()
}

fn method_paths(platform: &MockPlatform) -> Vec<String> {
    requests_of(platform)
        .iter()
        .map(|request| {
            let path = request.path.split('?').next().unwrap();
            format!("{} {path}", request.method)
        })
        .collect()
}

/// The `path bytes sha256` lines the owner saw.
fn listing_on(screen: &str) -> Vec<serde_json::Value> {
    screen
        .lines()
        .filter_map(|line| {
            let parts: Vec<&str> = line.trim().split(' ').collect();
            match parts.as_slice() {
                [path, bytes, sha]
                    if sha.len() == 64 && sha.bytes().all(|b| b.is_ascii_hexdigit()) =>
                {
                    Some(serde_json::json!({
                        "path": path, "bytes": bytes.parse::<u64>().ok()?, "sha256": sha,
                    }))
                }
                _ => None,
            }
        })
        .collect()
}

#[test]
fn three_puts_with_matching_sha256_one_confirm_and_the_listing_is_the_manifest() {
    let machine = Machine::new("submit-happy");
    let (platform, _world) = mock_platform();
    machine.put_entry();
    std::fs::create_dir_all(machine.folder().join("meter")).unwrap();
    std::fs::write(
        machine.folder().join("meter/start.json"),
        r#"{"ok":true,"used_percentage":12.5}"#,
    )
    .unwrap();

    let (result, screen) = submit_with(&machine, &platform, &SecretSet::new(), &mut answers());
    let submitted = result.unwrap_or_else(|error| panic!("{error}\n{screen}"));
    assert_eq!(submitted.files, 3);
    assert_eq!(submitted.delivered.version, 1);

    assert_eq!(
        method_paths(&platform),
        [
            "GET /api/daycare/challenges/c-1",
            "POST /api/daycare/challenges/c-1/submissions",
            "PUT /storage/v1/object/upload/sign/daycare-challenge-artifacts/entries/entry-1/v1/NOTES.md",
            "PUT /storage/v1/object/upload/sign/daycare-challenge-artifacts/entries/entry-1/v1/data/out.csv",
            "PUT /storage/v1/object/upload/sign/daycare-challenge-artifacts/entries/entry-1/v1/entry.html",
            "POST /api/daycare/challenges/c-1/submissions/sub-1/confirm",
        ]
    );
    let requests = requests_of(&platform);
    let posted = requests[1].json();
    // The listing the owner said yes to is exactly the manifest that was sent.
    assert_eq!(
        listing_on(&screen),
        posted["files"].as_array().unwrap().clone()
    );
    assert_eq!(posted["declaration"]["direction"], "light");
    assert_eq!(
        posted["declaration"]["tools"],
        serde_json::json!(["Claude Code", "a browser"])
    );
    assert!(posted.get("actor_id").is_none(), "{posted}");
    assert_eq!(
        requests[1].authorization(),
        Some(format!("Bearer {TOKEN}").as_str())
    );

    // Each PUT carries the bytes the listing hashed, and no Daycare credential.
    for put in requests.iter().filter(|request| request.method == "PUT") {
        let path = put
            .path
            .split("/entries/entry-1/v1/")
            .nth(1)
            .unwrap()
            .split('?')
            .next()
            .unwrap();
        let listed = posted["files"]
            .as_array()
            .unwrap()
            .iter()
            .find(|file| file["path"] == path)
            .unwrap();
        assert_eq!(
            sha256(put.body.as_bytes()),
            listed["sha256"].as_str().unwrap()
        );
        assert_eq!(put.body.len() as u64, listed["bytes"].as_u64().unwrap());
        assert!(put.authorization().is_none());
        assert!(!put.headers.contains_key("x-daycare-runner-release"));
    }
    assert_eq!(requests[5].json(), serde_json::json!({"confirm": "yes"}));

    assert!(screen.contains("Delivered: version 1"), "{screen}");
    assert!(
        !screen.contains(TOKEN) && !screen.contains(UPLOAD_GRANT),
        "{screen}"
    );
    let entry = machine.entry_json();
    assert_eq!(entry["meter"]["start"]["used_percentage"], 12.5);
    assert_eq!(entry["submissions"][0]["version"], 1);
    assert_eq!(entry["submissions"][0]["submission_id"], "sub-1");
    assert_eq!(
        entry["submissions"][0]["manifest_sha256"],
        submitted.delivered.manifest_sha256
    );
}

#[test]
fn a_resubmission_makes_a_new_version() {
    let machine = Machine::new("submit-again");
    let (platform, _world) = mock_platform();
    machine.put_entry();
    let secrets = SecretSet::new();
    let (first, _) = submit_with(&machine, &platform, &secrets, &mut answers());
    assert_eq!(first.unwrap().delivered.version, 1);
    machine.put_output("NOTES.md", "# Notes\nSecond try.\n");
    let (second, screen) = submit_with(&machine, &platform, &secrets, &mut answers());
    assert_eq!(second.unwrap().delivered.version, 2, "{screen}");
    let paths = method_paths(&platform);
    assert_eq!(
        paths
            .iter()
            .filter(|path| path.ends_with("/submissions"))
            .count(),
        2
    );
    assert!(paths.iter().any(|path| path.contains("/v2/NOTES.md")));
    let entry = machine.entry_json();
    assert_eq!(entry["submissions"].as_array().unwrap().len(), 2);
    assert_eq!(entry["submissions"][1]["version"], 2);
}

#[test]
fn a_symlink_in_output_aborts_before_anything_is_sent() {
    let machine = Machine::new("submit-symlink");
    let (platform, _world) = mock_platform();
    machine.put_entry();
    std::os::unix::fs::symlink(
        machine.fake_home.join(".ssh/id_ed25519"),
        machine.folder().join("output/key"),
    )
    .unwrap();
    let (result, screen) = submit_with(&machine, &platform, &SecretSet::new(), &mut answers());
    let error = result.unwrap_err();
    assert!(error.message().contains("output/key is a link"), "{error}");
    assert_eq!(method_paths(&platform), ["GET /api/daycare/challenges/c-1"]);
    assert!(!screen.contains("Delivered"));
}

#[test]
fn a_planted_device_token_blocks_and_a_clean_folder_passes() {
    let machine = Machine::new("submit-scan");
    let (platform, _world) = mock_platform();
    machine.enroll(&platform);
    machine.put_entry();
    // The scan reads the machine's real credential store (the token file
    // here), not a list the test hands it.
    let secrets = machine.secrets();
    assert!(secrets.counts().0 >= 1);

    machine.put_output(
        "NOTES.md",
        &format!("# Notes\nIt used {TOKEN} to log in.\n"),
    );
    let before = requests_of(&platform).len();
    let (result, screen) = submit_with(&machine, &platform, &secrets, &mut answers());
    let error = result.unwrap_err();
    assert!(error.message().contains("output/NOTES.md"), "{error}");
    assert!(
        error
            .message()
            .contains(FindingKind::DeviceToken.describe()),
        "{error}"
    );
    assert!(
        !error.message().contains(TOKEN),
        "the refusal echoed the secret"
    );
    assert!(!screen.contains(TOKEN));
    // Only the challenge read went out; no file list, no upload.
    assert_eq!(requests_of(&platform).len(), before + 1);

    machine.put_output("NOTES.md", "# Notes\nI fixed the labels.\n");
    let (clean, screen) = submit_with(&machine, &platform, &secrets, &mut answers());
    assert!(clean.is_ok(), "{screen}");
    assert!(screen.contains("The local secret scan found nothing."));
}

#[test]
fn private_memory_and_the_home_path_block_too() {
    let machine = Machine::new("submit-memory");
    let (platform, _world) = mock_platform();
    machine.put_entry();
    let rules = machine.fake_home.join(".claude/CLAUDE.md");
    std::fs::create_dir_all(rules.parent().unwrap()).unwrap();
    std::fs::write(&rules, "# PRIVATE-GLOBAL-RULES-do-not-copy\n").unwrap();
    let secrets = machine.secrets();

    machine.put_output("NOTES.md", "PRIVATE-GLOBAL-RULES-do-not-copy\n");
    machine.put_output(
        "entry.html",
        &format!("<!-- built in {}/work -->\n", machine.fake_home.display()),
    );
    let (result, _) = submit_with(&machine, &platform, &secrets, &mut answers());
    let error = result.unwrap_err();
    assert!(
        error
            .message()
            .contains(FindingKind::DontUseMemory.describe()),
        "{error}"
    );
    assert!(
        error.message().contains(FindingKind::HomePath.describe()),
        "{error}"
    );
    assert!(!error.message().contains("PRIVATE-GLOBAL-RULES"), "{error}");
}

#[test]
fn a_410_on_confirm_surfaces_as_not_delivered() {
    let machine = Machine::new("submit-410");
    let (platform, world) = mock_platform();
    machine.put_entry();
    world.lock().unwrap().confirm_refusal = Some((410, r#"{"error":"gone"}"#));
    let (result, screen) = submit_with(&machine, &platform, &SecretSet::new(), &mut answers());
    let error = result.unwrap_err();
    assert!(error.message().starts_with("Not delivered"), "{error}");
    assert!(error.message().contains("closed"), "{error}");
    assert_eq!(error.http_status(), Some(410));
    assert!(!screen.contains("Delivered:"), "{screen}");
    // Every file was uploaded, and still nothing counts: no record of success.
    assert_eq!(
        method_paths(&platform)
            .iter()
            .filter(|p| p.starts_with("PUT"))
            .count(),
        3
    );
    assert!(machine.entry_json().get("submissions").is_none());
}

#[test]
fn a_409_closed_on_submit_is_not_delivered_and_uploads_nothing() {
    let machine = Machine::new("submit-409");
    let (platform, world) = mock_platform();
    machine.put_entry();
    world.lock().unwrap().submit_refusal = Some((
        409,
        r#"{"error":"closed","reason":"challenge_closed","message":"This challenge is not taking entries. It has closed."}"#,
    ));
    let (result, screen) = submit_with(&machine, &platform, &SecretSet::new(), &mut answers());
    let error = result.unwrap_err();
    assert!(error.message().starts_with("Not delivered"), "{error}");
    assert!(error.message().contains("HTTP 409"), "{error}");
    assert!(!screen.contains("Delivered:"));
    assert!(!method_paths(&platform).iter().any(|p| p.starts_with("PUT")));
    assert!(machine.entry_json().get("submissions").is_none());
}

#[test]
fn another_refusal_on_confirm_is_not_delivered_either() {
    let machine = Machine::new("submit-mismatch");
    let (platform, world) = mock_platform();
    machine.put_entry();
    world.lock().unwrap().confirm_refusal = Some((
        409,
        r#"{"error":"A stored file does not match the list you sent. Start a new upload.","reason":"upload_mismatch","path":"NOTES.md"}"#,
    ));
    let (result, _) = submit_with(&machine, &platform, &SecretSet::new(), &mut answers());
    let error = result.unwrap_err();
    assert!(error.message().starts_with("Not delivered"), "{error}");
    assert!(error.message().contains("does not match"), "{error}");
}

#[test]
fn a_lost_confirm_answer_is_reported_as_unknown_not_as_success() {
    let machine = Machine::new("submit-lost");
    let (platform, world) = mock_platform();
    machine.put_entry();
    world.lock().unwrap().confirm_lost = true;
    let (result, screen) = submit_with(&machine, &platform, &SecretSet::new(), &mut answers());
    let error = result.unwrap_err();
    assert!(
        error
            .message()
            .starts_with("Not confirmed as far as this runner knows"),
        "{error}"
    );
    assert!(!screen.contains("Delivered:"));
    assert!(machine.entry_json().get("submissions").is_none());
}

#[test]
fn a_closed_challenge_is_refused_before_the_listing() {
    let machine = Machine::new("submit-closed");
    let (platform, world) = mock_platform();
    machine.put_entry();
    world.lock().unwrap().state = "closed".into();
    let (result, screen) = submit_with(&machine, &platform, &SecretSet::new(), &mut answers());
    assert!(result.unwrap_err().message().starts_with("Not delivered"));
    assert!(listing_on(&screen).is_empty());
    assert_eq!(method_paths(&platform), ["GET /api/daycare/challenges/c-1"]);
}

#[test]
fn anything_but_a_typed_yes_sends_nothing() {
    let machine = Machine::new("submit-no");
    let (platform, _world) = mock_platform();
    machine.put_entry();
    for script in [
        vec!["light", "", "no"],
        vec!["light", ""],
        vec!["maybe", "perhaps", "unsure"],
    ] {
        let (result, _) = submit_with(
            &machine,
            &platform,
            &SecretSet::new(),
            &mut Scripted(script),
        );
        let error = result.unwrap_err();
        assert!(
            error.message().to_lowercase().contains("nothing was sent"),
            "{error}"
        );
    }
    assert!(!method_paths(&platform)
        .iter()
        .any(|path| path.contains("/submissions")));
    assert!(machine.entry_json().get("submissions").is_none());
}

#[test]
fn the_binary_refuses_json_no_terminal_and_the_marker_before_any_request() {
    let machine = Machine::new("submit-refusals");
    let (platform, _world) = mock_platform();
    machine.enroll(&platform);
    machine.put_entry();
    let before = platform.requests().len();

    let json = machine.run(&["challenge", "submit", "c-1", "--json"]);
    assert_eq!(json.status.code(), Some(1));
    let body: serde_json::Value =
        serde_json::from_str(String::from_utf8_lossy(&json.stdout).trim()).unwrap();
    assert_eq!(body["ok"], false);
    assert!(body["error"].as_str().unwrap().contains("--json"), "{body}");

    let no_terminal = machine.run(&["challenge", "submit", "c-1"]);
    assert_eq!(no_terminal.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&no_terminal.stderr).contains("real terminal"),
        "{}",
        String::from_utf8_lossy(&no_terminal.stderr)
    );

    let marked = machine
        .command()
        .env("DAYCARE_CHALLENGE_SESSION", ENTRY_ID)
        .args(["challenge", "submit", "c-1", "--json"])
        .output()
        .unwrap();
    assert_eq!(marked.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&marked.stdout).contains("inside a challenge session"));

    assert_eq!(
        platform.requests().len(),
        before,
        "a refused submit made a request"
    );
    for out in [&json, &no_terminal, &marked] {
        assert!(!String::from_utf8_lossy(&out.stdout).contains(TOKEN));
        assert!(!String::from_utf8_lossy(&out.stderr).contains(TOKEN));
    }
}

/// The whole CLI on a real pseudo-terminal: the person reads the listing,
/// answers, and types yes. macOS `script` only; the library cases above cover
/// the same flow on every platform.
#[cfg(target_os = "macos")]
#[test]
fn a_typed_yes_on_a_real_terminal_delivers_the_entry() {
    use std::io::{Read, Write};
    let machine = Machine::new("submit-tty");
    let (platform, _world) = mock_platform();
    machine.enroll(&platform);
    machine.put_entry();

    let mut inner = machine.command();
    inner.current_dir(machine.folder().join("output")).args([
        "-q",
        "/dev/null",
        BIN,
        "challenge",
        "submit",
    ]);
    let mut script = Command::new("/usr/bin/script");
    for (name, value) in inner.get_envs() {
        match value {
            Some(value) => script.env(name, value),
            None => script.env_remove(name),
        };
    }
    let mut child = script
        .current_dir(machine.folder().join("output"))
        .args(inner.get_args())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("run script(1)");
    let mut stdin = child.stdin.take().unwrap();
    let mut screen = child.stdout.take().unwrap();
    let (send, receive) = std::sync::mpsc::channel::<String>();
    let reader = std::thread::spawn(move || {
        let mut seen = String::new();
        let mut buffer = [0u8; 4096];
        while let Ok(count) = screen.read(&mut buffer) {
            if count == 0 {
                break;
            }
            seen.push_str(&String::from_utf8_lossy(&buffer[..count]));
            let _ = send.send(seen.clone());
        }
        seen
    });
    let wait_for = |needle: &str| {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            match receive.recv_timeout(left) {
                Ok(seen) if seen.contains(needle) => return,
                Ok(_) => continue,
                Err(_) => panic!("never saw {needle:?}"),
            }
        }
    };
    wait_for("Type none, light or heavy");
    stdin.write_all(b"heavy\n").unwrap();
    wait_for("Which tools did it use?");
    stdin.write_all(b"Claude Code\n").unwrap();
    wait_for("Type yes to send these 3 file(s)");
    stdin.write_all(b"yes\n").unwrap();
    let status = child.wait().unwrap();
    drop(stdin);
    let screen = reader.join().unwrap().replace('\r', "");
    assert!(status.success(), "{screen}");
    assert!(screen.contains("Delivered: version 1"), "{screen}");
    assert!(!screen.contains(TOKEN), "the token reached the screen");

    let requests = requests_of(&platform);
    let posted = requests
        .iter()
        .find(|request| request.path.ends_with("/submissions"))
        .unwrap()
        .json();
    assert_eq!(
        listing_on(&screen),
        posted["files"].as_array().unwrap().clone()
    );
    assert_eq!(posted["declaration"]["direction"], "heavy");
    assert_eq!(requests.iter().filter(|r| r.method == "PUT").count(), 3);
    assert_eq!(
        requests
            .iter()
            .filter(|r| r.path.ends_with("/confirm"))
            .count(),
        1
    );
    // The identity that started the folder is the one that submitted.
    assert!(requests
        .iter()
        .filter(|r| r.method != "PUT")
        .all(|r| r.authorization() == Some(format!("Bearer {TOKEN}").as_str())));
}

impl Machine {
    fn skill_libraries(&self) -> Vec<PathBuf> {
        [
            ".claude/skills",
            ".agents/skills",
            ".config/opencode/skills",
        ]
        .iter()
        .map(|library| self.fake_home.join(library).join("tidy-csv"))
        .collect()
    }
}

fn stdout_json(output: &Output) -> serde_json::Value {
    serde_json::from_str(String::from_utf8_lossy(&output.stdout).trim())
        .unwrap_or_else(|_| panic!("not JSON: {}", String::from_utf8_lossy(&output.stdout)))
}

#[test]
fn skill_install_name_at_version_verifies_then_installs_with_a_marker() {
    let machine = Machine::new("skill-install");
    let (platform, _world) = mock_platform();
    machine.enroll(&platform);
    let accept = tidy_csv_list_sha256();
    assert_eq!(
        accept,
        daycare_runner::skills::file_list_sha256([
            (
                "ref/a.md",
                REF_MD.len() as u64,
                sha256(REF_MD.as_bytes()).as_str()
            ),
            (
                "SKILL.md",
                SKILL_MD.len() as u64,
                sha256(SKILL_MD.as_bytes()).as_str()
            ),
        ])
    );

    let out = machine.run(&[
        "skill",
        "install",
        "tidy-csv@3",
        "--json",
        "--accept-sha256",
        &accept,
    ]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    let result = stdout_json(&out);
    assert_eq!(result["file_list_sha256"], accept);
    assert_eq!(result["licence"], "CC-BY-4.0");
    for dir in machine.skill_libraries() {
        assert_eq!(
            std::fs::read_to_string(dir.join("SKILL.md")).unwrap(),
            SKILL_MD
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("ref/a.md")).unwrap(),
            REF_MD
        );
        let marker: serde_json::Value =
            serde_json::from_slice(&std::fs::read(dir.join(".daycare-skill.json")).unwrap())
                .unwrap();
        assert_eq!(marker["name"], "tidy-csv");
        assert_eq!(marker["version"], "3");
    }
    let requests = requests_of(&platform);
    let api = requests
        .iter()
        .find(|request| request.path == "/api/daycare/skills/tidy-csv/versions/3")
        .unwrap();
    assert_eq!(
        api.authorization(),
        Some(format!("Bearer {TOKEN}").as_str())
    );
    assert!(requests
        .iter()
        .filter(|request| request.path.starts_with("/storage/"))
        .all(|request| request.authorization().is_none()));

    // The memory review sees it as a library skill, not the owner's own.
    let scan = stdout_json(&machine.run(&["memory", "setup", "--scan", "--json"]));
    assert!(scan["items"]
        .as_array()
        .unwrap()
        .iter()
        .any(|item| item["name"] == "tidy-csv" && item["origin"] == "daycare-library"));

    let removed = stdout_json(&machine.run(&["skill", "uninstall", "tidy-csv", "--json"]));
    assert_eq!(removed["removed"].as_array().unwrap().len(), 3);
    assert!(machine.skill_libraries().iter().all(|dir| !dir.exists()));
}

#[test]
fn a_sha_mismatch_installs_nothing() {
    let machine = Machine::new("skill-mismatch");
    let (platform, _world) = mock_platform();
    machine.enroll(&platform);
    let out = machine.run(&[
        "skill",
        "install",
        "tidy-csv@4",
        "--json",
        "--accept-sha256",
        &tidy_csv_list_sha256(),
    ]);
    assert_eq!(out.status.code(), Some(1));
    let error = stdout_json(&out);
    assert!(
        error["error"]
            .as_str()
            .unwrap()
            .contains("ref/a.md does not match its sha256"),
        "{error}"
    );
    for dir in machine.skill_libraries() {
        assert!(!dir.exists(), "{} was written", dir.display());
        assert!(
            !dir.parent().unwrap().exists()
                || std::fs::read_dir(dir.parent().unwrap())
                    .unwrap()
                    .next()
                    .is_none()
        );
    }
}

#[test]
fn unsafe_or_unreviewed_library_installs_are_refused_before_any_request() {
    let machine = Machine::new("skill-refusals");
    let (platform, _world) = mock_platform();
    machine.enroll(&platform);
    let before = platform.requests().len();
    let accept = tidy_csv_list_sha256();
    for args in [
        vec![
            "skill",
            "install",
            "../x@1",
            "--json",
            "--accept-sha256",
            accept.as_str(),
        ],
        vec![
            "skill",
            "install",
            "x@../1",
            "--json",
            "--accept-sha256",
            accept.as_str(),
        ],
        vec![
            "skill",
            "install",
            "x@1.0",
            "--json",
            "--accept-sha256",
            accept.as_str(),
        ],
        vec!["skill", "install", "tidy-csv@3", "--json"],
        vec![
            "skill",
            "install",
            "tidy-csv@3",
            "--json",
            "--accept-sha256",
            "not-a-hash",
        ],
        vec![
            "skill",
            "install",
            "tidy-csv@3",
            "--force",
            "--json",
            "--accept-sha256",
            accept.as_str(),
        ],
        vec![
            "skill",
            "install",
            "--json",
            "--accept-sha256",
            accept.as_str(),
        ],
        vec!["skill", "uninstall", "../x", "--json"],
    ] {
        let out = machine.run(&args);
        assert_eq!(out.status.code(), Some(1), "{args:?}");
        assert_eq!(stdout_json(&out)["ok"], false, "{args:?}");
    }
    // No terminal and no --accept-sha256: there is nobody to type yes.
    let out = machine.run(&["skill", "install", "tidy-csv@3"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("real terminal"));
    assert_eq!(
        platform.requests().len(),
        before,
        "a refused install made a request"
    );
    assert!(machine.skill_libraries().iter().all(|dir| !dir.exists()));
}

#[test]
fn a_withdrawn_unowned_or_unreviewed_version_installs_nothing() {
    let machine = Machine::new("skill-410");
    let (platform, _world) = mock_platform();
    machine.enroll(&platform);
    let accept = tidy_csv_list_sha256();
    let withdrawn = machine.run(&[
        "skill",
        "install",
        "tidy-csv@2",
        "--json",
        "--accept-sha256",
        &accept,
    ]);
    assert!(stdout_json(&withdrawn)["error"]
        .as_str()
        .unwrap()
        .contains("withdrawn"));
    let unowned = machine.run(&[
        "skill",
        "install",
        "paid-skill@1",
        "--json",
        "--accept-sha256",
        &accept,
    ]);
    assert!(stdout_json(&unowned)["error"]
        .as_str()
        .unwrap()
        .contains("do not have paid-skill"));
    let other = "0".repeat(64);
    let mismatch = machine.run(&[
        "skill",
        "install",
        "tidy-csv@3",
        "--json",
        "--accept-sha256",
        &other,
    ]);
    assert!(stdout_json(&mismatch)["error"]
        .as_str()
        .unwrap()
        .contains("not the version you reviewed"));
    assert!(machine.skill_libraries().iter().all(|dir| !dir.exists()));
}

#[test]
fn a_folder_without_the_marker_is_never_replaced_or_removed() {
    let machine = Machine::new("skill-own");
    let (platform, _world) = mock_platform();
    machine.enroll(&platform);
    let own = machine.fake_home.join(".agents/skills/tidy-csv");
    std::fs::create_dir_all(&own).unwrap();
    std::fs::write(own.join("SKILL.md"), "my own tidy-csv\n").unwrap();
    let out = machine.run(&[
        "skill",
        "install",
        "tidy-csv@3",
        "--json",
        "--accept-sha256",
        &tidy_csv_list_sha256(),
    ]);
    assert_eq!(out.status.code(), Some(1));
    assert!(stdout_json(&out)["error"]
        .as_str()
        .unwrap()
        .contains("not from the Daycare library"));
    assert!(!machine.fake_home.join(".claude/skills/tidy-csv").exists());
    let kept = stdout_json(&machine.run(&["skill", "uninstall", "tidy-csv", "--json"]));
    assert_eq!(kept["removed"].as_array().unwrap().len(), 0);
    assert_eq!(kept["left"].as_array().unwrap().len(), 1);
    assert_eq!(
        std::fs::read_to_string(own.join("SKILL.md")).unwrap(),
        "my own tidy-csv\n"
    );
}
