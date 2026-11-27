//! `setup` probes the agents on PATH before anything is claimed or saved.
//! Here only a fake `agy` is on PATH: no Google sign-in, no model, no network.
mod support;

use std::path::PathBuf;
use std::process::{Command, Output};

const BIN: &str = env!("CARGO_BIN_EXE_daycare-runner");

fn setup_with_agy(version: &str) -> (Output, PathBuf) {
    let dir = support::scratch_dir("antigravity-preflight");
    let bin_dir = dir.join("bin");
    std::fs::create_dir_all(&bin_dir).unwrap();
    support::testdir::write_executable(
        &bin_dir.join("agy"),
        &format!(
            "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then echo '{version}'; exit 0; fi\n\
             if [ \"$1\" = \"models\" ]; then echo 'gemini-3.8-flash-medium  Gemini 3.8 Flash'; exit 0; fi\n\
             exit 1\n"
        ),
    );
    let home = dir.join("home");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(dir.join("daycare")).unwrap();
    std::fs::write(
        dir.join("daycare").join("config.json"),
        r#"{"platform_url": "http://127.0.0.1:1", "device_id": "d", "actor_id": "a",
            "actor_name": "Pip", "workspace_dir": "/nowhere", "mcp_url": "http://127.0.0.1:1/mcp"}"#,
    )
    .unwrap();
    let output = Command::new(BIN)
        .args(["setup", "--agent", "antigravity"])
        .env("DAYCARE_HOME", dir.join("daycare"))
        .env("DAYCARE_TOKEN_FILE", dir.join("token"))
        .env("PATH", &bin_dir)
        .env("HOME", &home)
        .output()
        .expect("run daycare-runner setup");
    (output, dir)
}

// Antigravity visits are macOS-only, so elsewhere `setup` refuses it first.
#[cfg(target_os = "macos")]
#[test]
fn a_supported_antigravity_is_ready() {
    let (output, dir) = setup_with_agy("1.5.13");
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let saved = std::fs::read_to_string(dir.join("daycare").join("config.json")).unwrap();
    std::fs::remove_dir_all(&dir).ok();
    assert!(output.status.success(), "{stdout} {stderr}");
    assert!(saved.contains("\"antigravity\""), "{saved}");
}

#[cfg(target_os = "macos")]
#[test]
fn a_new_major_antigravity_is_refused_before_anything_is_claimed() {
    let (output, dir) = setup_with_agy("2.0.0");
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let saved = std::fs::read_to_string(dir.join("daycare").join("config.json")).unwrap();
    std::fs::remove_dir_all(&dir).ok();
    assert!(!output.status.success(), "{stdout} {stderr}");
    let text = format!("{stdout}{stderr}");
    assert!(text.contains("this is 2.0.0"), "{text}");
    assert!(text.contains("is not supported yet"), "{text}");
    assert!(text.contains("No pairing code was claimed"), "{text}");
    assert!(
        !saved.contains("antigravity"),
        "a refused agent must not be saved as ready: {saved}"
    );
}
