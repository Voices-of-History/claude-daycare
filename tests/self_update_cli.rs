//! The self-update paths in `main`: `visit start` updating first, a 426
//! updating once and retrying on the new binary, the loop guard, and a dev
//! build never updating. These need binaries stamped with a release id, so
//! the test builds two (`old0`, `new1`) into its own target dir once.

use daycare_runner::self_update::{sha256_hex, TARGET};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex, OnceLock};

#[path = "../src/testdir.rs"]
mod testdir;

struct Builds {
    old: PathBuf,
    new: PathBuf,
}

fn builds() -> &'static Builds {
    static BUILDS: OnceLock<Builds> = OnceLock::new();
    BUILDS.get_or_init(|| {
        let target_dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("stamped");
        let stamp = |release: &str| {
            let status = Command::new(env!("CARGO"))
                .args(["build", "--locked", "--bin", "daycare-runner"])
                .current_dir(env!("CARGO_MANIFEST_DIR"))
                .env("CARGO_TARGET_DIR", &target_dir)
                .env("DAYCARE_RUNNER_RELEASE", release)
                .status()
                .expect("run cargo build");
            assert!(status.success(), "stamped build {release} failed");
            let copy = target_dir.join(format!("daycare-runner-{release}"));
            std::fs::copy(target_dir.join("debug/daycare-runner"), &copy).unwrap();
            copy
        };
        Builds {
            old: stamp("old0"),
            new: stamp("new1"),
        }
    })
}

#[derive(Debug, Clone)]
struct Seen {
    method: String,
    path: String,
    release: String,
}

/// A site that serves `current.json` for `new1`, the `new1` build, and a
/// pair/claim route that answers 426 to any release in `refuse`.
struct Site {
    base: String,
    seen: Arc<Mutex<Vec<Seen>>>,
}

impl Site {
    fn start(refuse: &'static [&'static str]) -> Site {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let binary = std::fs::read(&builds().new).unwrap();
        let manifest = serde_json::json!({
            "release": "new1",
            "targets": { TARGET: { "url": format!("{base}/releases/new1"), "sha256": sha256_hex(&binary) } },
        })
        .to_string();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = Arc::clone(&seen);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                if reader.read_line(&mut line).is_err() {
                    continue;
                }
                let mut parts = line.split_whitespace();
                let method = parts.next().unwrap_or("").to_string();
                let path = parts.next().unwrap_or("").to_string();
                let (mut release, mut length) = (String::new(), 0usize);
                loop {
                    let mut header = String::new();
                    reader.read_line(&mut header).unwrap_or(0);
                    let header = header.trim_end();
                    if header.is_empty() {
                        break;
                    }
                    let (name, value) = header.split_once(':').unwrap_or((header, ""));
                    match name.trim().to_ascii_lowercase().as_str() {
                        "x-daycare-runner-release" => release = value.trim().to_string(),
                        "content-length" => length = value.trim().parse().unwrap_or(0),
                        _ => {}
                    }
                }
                let mut body = vec![0; length];
                let _ = reader.read_exact(&mut body);
                let (status, bytes): (u16, Vec<u8>) = match path.as_str() {
                    "/releases/current.json" => (200, manifest.clone().into_bytes()),
                    "/releases/new1" => (200, binary.clone()),
                    "/api/daycare/pair/claim" if refuse.contains(&release.as_str()) => {
                        (426, br#"{"error":"runner_update_required"}"#.to_vec())
                    }
                    "/api/daycare/pair/claim" => (400, br#"{"error":"bad code"}"#.to_vec()),
                    _ => (404, b"{}".to_vec()),
                };
                log.lock().unwrap().push(Seen {
                    method,
                    path,
                    release,
                });
                let _ = write!(
                    stream,
                    "HTTP/1.1 {status} X\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    bytes.len()
                );
                let _ = stream.write_all(&bytes);
            }
        });
        Site { base, seen }
    }

    fn seen(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }

    fn claims(&self) -> Vec<String> {
        self.seen()
            .into_iter()
            .filter(|s| s.method == "POST")
            .map(|s| s.release)
            .collect()
    }

    fn pointer_reads(&self) -> usize {
        self.seen()
            .iter()
            .filter(|s| s.path == "/releases/current.json")
            .count()
    }
}

/// An enrolled-looking home (config only; the token store is a file so no
/// test ever touches the keychain) and a copy of `build` as the installed
/// runner.
fn machine(label: &str, site: &Site, build: &Path) -> (PathBuf, PathBuf) {
    let root = testdir::unique_dir(label);
    let home = root.join("home");
    std::fs::create_dir_all(home.join(".claude-daycare")).unwrap();
    std::fs::write(
        home.join(".claude-daycare/config.json"),
        serde_json::json!({
            "platform_url": site.base, "device_id": "d", "actor_id": "a", "actor_name": "n",
            "workspace_dir": root.join("ws"), "mcp_url": format!("{}/mcp", site.base),
        })
        .to_string(),
    )
    .unwrap();
    let exe = root.join("bin/daycare-runner");
    std::fs::create_dir_all(exe.parent().unwrap()).unwrap();
    std::fs::copy(build, &exe).unwrap();
    (home, exe)
}

fn run(exe: &Path, home: &Path, args: &[&str]) -> std::process::Output {
    Command::new(exe)
        .args(args)
        .env("HOME", home)
        .env_remove("DAYCARE_HOME")
        .env_remove("DAYCARE_RUNNER_SELF_UPDATED")
        .env("DAYCARE_TOKEN_FILE", home.join("token"))
        .env("DAYCARE_WORKSPACE_ROOT", home.join("workspaces"))
        .env("DAYCARE_SKIP_CLAUDE_PREFLIGHT", "1")
        .env("PATH", "/usr/bin:/bin")
        .output()
        .unwrap()
}

fn version(exe: &Path) -> String {
    let out = Command::new(exe).arg("--version").output().unwrap();
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn enroll_args(site: &Site) -> Vec<String> {
    ["enroll", "--url", &site.base, "--code", "ABCD"]
        .map(String::from)
        .to_vec()
}

#[test]
fn a_426_updates_once_and_the_same_command_retries_on_the_new_binary() {
    let site = Site::start(&["old0"]);
    let (home, exe) = machine("su-cli-426", &site, &builds().old);
    let args = enroll_args(&site);
    let out = run(
        &exe,
        &home,
        &args.iter().map(String::as_str).collect::<Vec<_>>(),
    );
    let stderr = String::from_utf8_lossy(&out.stderr);

    assert_eq!(site.claims(), ["old0", "new1"], "{stderr}");
    assert_eq!(site.pointer_reads(), 1);
    assert!(
        stderr.contains("Updated daycare-runner to release new1"),
        "{stderr}"
    );
    // The retry's own answer is what the person sees.
    assert!(stderr.contains("bad code"), "{stderr}");
    assert!(version(&exe).contains("(release new1)"));
    assert!(home.join(".claude/skills/daycare/SKILL.md").exists());
}

#[test]
fn a_site_that_still_refuses_after_the_update_does_not_loop() {
    let site = Site::start(&["old0", "new1"]);
    let (home, exe) = machine("su-cli-loop", &site, &builds().old);
    let args = enroll_args(&site);
    let out = run(
        &exe,
        &home,
        &args.iter().map(String::as_str).collect::<Vec<_>>(),
    );
    let stderr = String::from_utf8_lossy(&out.stderr);

    assert!(!out.status.success());
    assert_eq!(site.claims(), ["old0", "new1"], "{stderr}");
    assert_eq!(site.pointer_reads(), 1, "{stderr}");
    assert!(stderr.contains("HTTP 426"), "{stderr}");
}

#[test]
fn visit_start_updates_before_anything_else_and_reruns_as_the_new_release() {
    let site = Site::start(&[]);
    let (home, exe) = machine("su-cli-visit", &site, &builds().old);
    let out = run(&exe, &home, &["visit", "start", "--json"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let stdout = String::from_utf8_lossy(&out.stdout);

    assert!(
        stderr.contains("Updated daycare-runner to release new1"),
        "{stderr}"
    );
    assert!(
        !stderr.contains("could not start the updated runner"),
        "{stderr}"
    );
    assert!(version(&exe).contains("(release new1)"));
    // One pointer read: the re-executed binary does not check again.
    assert_eq!(site.pointer_reads(), 1);
    // `--json` stdout is still a single result, not progress lines.
    assert_eq!(stdout.lines().count(), 1, "{stdout}");
    assert!(serde_json::from_str::<serde_json::Value>(stdout.trim()).is_ok());
}

#[test]
fn a_current_runner_starts_its_visit_without_downloading() {
    let site = Site::start(&[]);
    let (home, exe) = machine("su-cli-current", &site, &builds().new);
    let before = std::fs::read(&exe).unwrap();
    run(&exe, &home, &["visit", "start", "--json"]);
    assert_eq!(site.pointer_reads(), 1);
    assert!(site.seen().iter().all(|s| s.path != "/releases/new1"));
    assert_eq!(std::fs::read(&exe).unwrap(), before);
}

#[test]
fn a_dev_build_never_updates_even_on_a_426() {
    let site = Site::start(&[""]);
    let (home, _) = machine("su-cli-dev", &site, &builds().old);
    let dev = PathBuf::from(env!("CARGO_BIN_EXE_daycare-runner"));
    let args = enroll_args(&site);
    let out = run(
        &dev,
        &home,
        &args.iter().map(String::as_str).collect::<Vec<_>>(),
    );
    assert!(!out.status.success());
    assert_eq!(site.claims(), [""]);
    assert_eq!(site.pointer_reads(), 0);
    run(&dev, &home, &["visit", "start", "--json"]);
    assert_eq!(site.pointer_reads(), 0);
}
