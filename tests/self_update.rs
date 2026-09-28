//! `daycare-runner update` against a local platform: pointer, download,
//! checksum, the new build's `--version` probe, the rename, and the new
//! build's `skill install`. The "binary" served is a shell script, which is
//! enough to exercise every step without shipping a real release.

mod support;

use daycare_runner::self_update::{sha256_hex, update, Outcome, TARGET};
use std::path::{Path, PathBuf};
use support::{scratch_dir, MockPlatform, Response};

/// A stand-in release build: reports `release` on `--version` and records its
/// `skill install` argv beside itself.
fn fake_release(dir: &Path, release: &str) -> String {
    let marker = dir.join("skill-install.argv");
    format!(
        "#!/bin/sh\ncase \"$1\" in\n  --version) echo \"daycare-runner 0.1.0 (release {release})\" ;;\n  \
         skill) echo \"$@\" > '{}'; echo '{{\"ok\":true}}' ;;\nesac\n",
        marker.display()
    )
}

fn installed_runner(dir: &Path) -> PathBuf {
    let exe = dir.join("daycare-runner");
    std::fs::write(&exe, "old build").unwrap();
    exe
}

fn manifest(release: &str, target: &str, url: &str, sha256: &str) -> String {
    serde_json::json!({
        "release": release,
        "targets": { target: { "url": url, "sha256": sha256 } },
    })
    .to_string()
}

/// A platform serving `current.json` (built per request, so it can point at
/// this server's own origin), optionally `install.sh`, and one release build.
fn serve<F>(manifest_for: F, install_sh: Option<String>, binary: String) -> MockPlatform
where
    F: Fn(&str) -> Option<String> + Send + 'static,
{
    MockPlatform::start(move |request| {
        let base = format!(
            "http://{}",
            request.headers.get("host").cloned().unwrap_or_default()
        );
        match request.path.as_str() {
            "/releases/current.json" => match manifest_for(&base) {
                Some(body) => Response::json(200, &body),
                None => Response::json(404, "not found"),
            },
            "/install.sh" => match &install_sh {
                Some(body) => Response::json(200, body),
                None => Response::json(404, "not found"),
            },
            "/releases/new-build" => Response::json(200, &binary),
            _ => Response::json(404, "not found"),
        }
    })
}

fn build_url(base: &str) -> String {
    format!("{base}/releases/new-build")
}

#[test]
fn an_old_release_is_replaced_and_the_new_build_installs_the_skill() {
    let dir = scratch_dir("self-update-ok");
    let exe = installed_runner(&dir);
    let binary = fake_release(&dir, "new1");
    let sha = sha256_hex(binary.as_bytes());
    let platform = serve(
        {
            let sha = sha.clone();
            move |base| {
                Some(
                    serde_json::json!({
                        "release": "new1",
                        "targets": {
                            TARGET: { "url": build_url(base), "sha256": sha },
                            "some-other-target": { "url": "https://x.test/y", "sha256": "0".repeat(64) },
                        },
                    })
                    .to_string(),
                )
            }
        },
        None,
        binary.clone(),
    );

    let mut notes = Vec::new();
    let outcome = update(&platform.base_url, Some("old0"), &exe, &mut |line| {
        notes.push(line.to_string())
    })
    .unwrap();

    let canonical = std::fs::canonicalize(&exe).unwrap();
    assert_eq!(
        outcome,
        Outcome::Updated {
            from: "old0".into(),
            to: "new1".into(),
            path: canonical,
            skill_error: None,
        }
    );
    assert_eq!(std::fs::read_to_string(&exe).unwrap(), binary);
    assert_eq!(
        std::fs::read_to_string(dir.join("skill-install.argv"))
            .unwrap()
            .trim(),
        "skill install --json"
    );
    assert!(
        notes.iter().any(|n| n.contains("old0 -> new1")),
        "{notes:?}"
    );
    // No staging file is left beside the runner.
    let leftovers: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.file_name()
                .to_string_lossy()
                .starts_with(".daycare-runner.update")
        })
        .collect();
    assert!(leftovers.is_empty());
}

#[test]
fn the_current_release_downloads_nothing() {
    let dir = scratch_dir("self-update-current");
    let exe = installed_runner(&dir);
    let platform = serve(
        |_| {
            Some(manifest(
                "same",
                TARGET,
                "https://x.test/never",
                &"a".repeat(64),
            ))
        },
        None,
        String::new(),
    );
    let outcome = update(&platform.base_url, Some("same"), &exe, &mut |_| {}).unwrap();
    assert_eq!(
        outcome,
        Outcome::Current {
            release: "same".into()
        }
    );
    assert_eq!(std::fs::read_to_string(&exe).unwrap(), "old build");
    assert!(platform
        .requests()
        .iter()
        .all(|r| r.path == "/releases/current.json"));
}

#[test]
fn a_checksum_mismatch_replaces_nothing() {
    let dir = scratch_dir("self-update-sha");
    let exe = installed_runner(&dir);
    let binary = fake_release(&dir, "new1");
    let platform = serve(
        |base| Some(manifest("new1", TARGET, &build_url(base), &"b".repeat(64))),
        None,
        binary,
    );
    let error = update(&platform.base_url, Some("old0"), &exe, &mut |_| {}).unwrap_err();
    assert!(error.message().contains("checksum mismatch"), "{error}");
    assert_eq!(std::fs::read_to_string(&exe).unwrap(), "old build");
}

#[test]
fn a_build_that_does_not_report_the_promised_release_replaces_nothing() {
    let dir = scratch_dir("self-update-version");
    let exe = installed_runner(&dir);
    let binary = fake_release(&dir, "something-else");
    let sha = sha256_hex(binary.as_bytes());
    let platform = serve(
        move |base| Some(manifest("new1", TARGET, &build_url(base), &sha)),
        None,
        binary,
    );
    let error = update(&platform.base_url, Some("old0"), &exe, &mut |_| {}).unwrap_err();
    assert!(
        error.message().contains("does not report release new1"),
        "{error}"
    );
    assert_eq!(std::fs::read_to_string(&exe).unwrap(), "old build");
}

#[test]
fn a_release_without_this_target_says_what_it_carries() {
    let dir = scratch_dir("self-update-target");
    let exe = installed_runner(&dir);
    let platform = serve(
        |_| {
            Some(manifest(
                "new1",
                "riscv64-unknown-none",
                "https://x.test/y",
                &"c".repeat(64),
            ))
        },
        None,
        String::new(),
    );
    let error = update(&platform.base_url, Some("old0"), &exe, &mut |_| {}).unwrap_err();
    assert!(error.message().contains(TARGET), "{error}");
    assert!(error.message().contains("riscv64-unknown-none"), "{error}");
}

#[test]
fn a_site_without_current_json_falls_back_to_the_installer_pins_on_apple_silicon_only() {
    let dir = scratch_dir("self-update-legacy");
    let exe = installed_runner(&dir);
    let install_sh = format!(
        "#!/bin/sh\nRUNNER_VERSION=\"same\"\nRUNNER_URL=\"https://x.test/y\"\nRUNNER_SHA256=\"{}\"\n",
        "d".repeat(64)
    );
    let platform = serve(|_| None, Some(install_sh), String::new());
    let result = update(&platform.base_url, Some("same"), &exe, &mut |_| {});
    if TARGET == "aarch64-apple-darwin" {
        assert_eq!(
            result.unwrap(),
            Outcome::Current {
                release: "same".into()
            }
        );
    } else {
        // The installer only ever pinned the Apple Silicon build; no other
        // target may take it.
        assert!(result.is_err());
        assert!(platform.requests().iter().all(|r| r.path != "/install.sh"));
    }
}
