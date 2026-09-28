//! Exercise the real `usage --agent codex` command with a refreshing fake
//! app-server. Every credential below is synthetic; no network or login used.
#![cfg(unix)]
mod support;
use serde_json::{json, Value};
use std::os::unix::fs::PermissionsExt;
use std::process::Command;

fn usage(refresh_method: &str, failure: &str, model: &str) -> (std::process::Output, Value, Value) {
    usage_version(refresh_method, failure, model, "0.158.0")
}

fn usage_version(
    refresh_method: &str,
    failure: &str,
    model: &str,
    version: &str,
) -> (std::process::Output, Value, Value) {
    let root = support::scratch_dir("codex-usage-refresh");
    let owner = root.join("owner");
    std::fs::create_dir_all(&owner).unwrap();
    let auth = json!({"auth_mode":"chatgpt", "OPENAI_API_KEY":null,
        "last_refresh":"2026-09-28T00:00:00Z", "tokens":{"account_id":"mock-account","access_token":"old-fake","refresh_token":"old-fake"}});
    std::fs::write(owner.join("auth.json"), auth.to_string()).unwrap();
    let script = root.join("codex");
    std::fs::write(&script, r#"#!/usr/bin/env python3
import json, os, pathlib, sys
if sys.argv[1:] == ['--version']:
    print('codex-cli ' + os.environ['MOCK_CODEX_VERSION']); sys.exit(0)
assert sys.argv[1:] == ['app-server']
for line in sys.stdin:
    req = json.loads(line)
    if 'id' not in req: continue
    method = req['method']
    if method == os.environ['REFRESH_METHOD']:
        path = pathlib.Path(os.environ['CODEX_HOME']) / 'auth.json'
        auth = json.loads(path.read_text())
        auth['last_refresh'] = '2026-09-28T01:00:00Z'
        auth['tokens']['refresh_token'] = 'refreshed-fake'
        path.write_text(json.dumps(auth))
        failure = os.environ['REFRESH_FAILURE']
        if failure == 'disconnect': sys.exit(0)
        if failure == 'rpc':
            print(json.dumps({'id':req['id'],'error':{'message':'mock failure'}}),flush=True)
            continue
        if failure == 'malformed':
            print(json.dumps({'id':req['id'],'result':{}}),flush=True)
            continue
    result = {
        'initialize': {},
        'account/read': {'account':{'type':'chatgpt','planType':'pro'}},
        'model/list': {'data':[{'id':'gpt-5.5'},{'id':'gpt-6-astra'}]},
        'account/rateLimits/read': {'rateLimits':{'primary':{'usedPercent':20,'windowDurationMins':10080,'resetsAt':1791055017}}}
    }[method]
    print(json.dumps({'id':req['id'],'result':result}),flush=True)
"#).unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_daycare-runner"))
        .args(["usage", "--agent", "codex", "--codex-bin"])
        .arg(&script)
        .args(["--model", model, "--json"])
        .env("CODEX_HOME", &owner)
        .env("DAYCARE_HOME", root.join("runner"))
        .env("REFRESH_METHOD", refresh_method)
        .env("REFRESH_FAILURE", failure)
        .env("MOCK_CODEX_VERSION", version)
        .output()
        .unwrap();
    let owner_auth =
        serde_json::from_str(&std::fs::read_to_string(owner.join("auth.json")).unwrap()).unwrap();
    let sealed_path = root.join("runner/codex/home/auth.json");
    let sealed_auth = std::fs::read_to_string(sealed_path)
        .ok()
        .map(|text| serde_json::from_str(&text).unwrap())
        .unwrap_or(Value::Null);
    (output, owner_auth, sealed_auth)
}

#[test]
fn usage_returns_a_meter_refresh_to_the_owner() {
    for method in [
        "initialize",
        "account/read",
        "model/list",
        "account/rateLimits/read",
    ] {
        let (output, owner, sealed) = usage(method, "", "gpt-5.5");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(owner["tokens"]["refresh_token"], "refreshed-fake");
        assert_eq!(owner, sealed);
    }
}

#[test]
fn old_codex_is_rejected_before_auth_is_copied_or_app_server_starts() {
    let (output, owner, sealed) = usage_version("initialize", "", "gpt-5.5", "0.154.0");
    assert!(!output.status.success());
    assert_eq!(owner["tokens"]["refresh_token"], "old-fake");
    assert_eq!(sealed, Value::Null);
    assert!(String::from_utf8_lossy(&output.stdout).contains("0.158.0"));
}

#[test]
fn failed_app_server_paths_also_return_refreshes() {
    for method in [
        "initialize",
        "account/read",
        "model/list",
        "account/rateLimits/read",
    ] {
        for failure in ["rpc", "disconnect", "malformed"] {
            if method == "initialize" && failure == "malformed" {
                continue;
            }
            let (output, owner, sealed) = usage(method, failure, "gpt-5.5");
            assert!(!output.status.success(), "{method} {failure}");
            assert_eq!(
                owner["tokens"]["refresh_token"], "refreshed-fake",
                "{method} {failure}"
            );
            assert_eq!(owner, sealed, "{method} {failure}");
        }
    }
}

#[test]
fn code_mode_only_models_are_rejected_even_when_the_catalog_lists_them() {
    let (output, _, _) = usage("none", "", "gpt-6-astra");
    assert!(!output.status.success());
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        text.contains("verified") && text.contains("gpt-5.5"),
        "{text}"
    );
}
