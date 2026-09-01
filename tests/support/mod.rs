#![allow(dead_code)]
//! Test doubles: a local HTTP server and a fake `claude` binary.
//! Nothing here touches the network, the real keychain, or a model.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

/// The same fixture-path helper the unit tests use, included rather than copied
/// so uniqueness has one implementation on both sides of the test boundary.
#[path = "../../src/testdir.rs"]
mod testdir;

#[derive(Debug, Clone)]
pub struct RecordedRequest {
    pub method: String,
    pub path: String,
    pub headers: HashMap<String, String>,
    pub body: String,
}

impl RecordedRequest {
    pub fn json(&self) -> serde_json::Value {
        serde_json::from_str(&self.body).unwrap_or(serde_json::Value::Null)
    }

    pub fn authorization(&self) -> Option<&str> {
        self.headers.get("authorization").map(String::as_str)
    }
}

pub struct Response {
    pub status: u16,
    pub body: String,
    disconnect: bool,
}

impl Response {
    pub fn json(status: u16, body: &str) -> Self {
        Response {
            status,
            body: body.to_string(),
            disconnect: false,
        }
    }

    pub fn no_content() -> Self {
        Response {
            status: 204,
            body: String::new(),
            disconnect: false,
        }
    }

    /// Accept the request, then drop the socket without an HTTP response.
    /// This is the packet-loss boundary the runner sees as a transport error.
    pub fn disconnect() -> Self {
        Response {
            status: 0,
            body: String::new(),
            disconnect: true,
        }
    }
}

pub struct MockPlatform {
    pub base_url: String,
    requests: Arc<Mutex<Vec<RecordedRequest>>>,
}

impl MockPlatform {
    /// `handler` maps (method, path) to a canned response.
    pub fn start<F>(handler: F) -> Self
    where
        F: Fn(&RecordedRequest) -> Response + Send + 'static,
    {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock platform");
        let port = listener.local_addr().unwrap().port();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&requests);

        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { break };
                if let Some(request) = read_request(&stream) {
                    let response = handler(&request);
                    recorded.lock().unwrap().push(request);
                    write_response(stream, response);
                }
            }
        });

        MockPlatform {
            base_url: format!("http://127.0.0.1:{port}"),
            requests,
        }
    }

    pub fn requests(&self) -> Vec<RecordedRequest> {
        self.requests.lock().unwrap().clone()
    }
}

fn read_request(stream: &TcpStream) -> Option<RecordedRequest> {
    let mut reader = BufReader::new(stream.try_clone().ok()?);
    let mut request_line = String::new();
    reader.read_line(&mut request_line).ok()?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next()?.to_string();
    let path = parts.next()?.to_string();

    let mut headers = HashMap::new();
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).ok()?;
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            headers.insert(name.trim().to_lowercase(), value.trim().to_string());
        }
    }

    let length: usize = headers
        .get("content-length")
        .and_then(|value| value.parse().ok())
        .unwrap_or(0);
    let mut body = vec![0u8; length];
    if length > 0 {
        reader.read_exact(&mut body).ok()?;
    }

    Some(RecordedRequest {
        method,
        path,
        headers,
        body: String::from_utf8_lossy(&body).into_owned(),
    })
}

fn write_response(mut stream: TcpStream, response: Response) {
    if response.disconnect {
        return;
    }
    let reason = match response.status {
        200 => "OK",
        204 => "No Content",
        401 => "Unauthorized",
        500 => "Internal Server Error",
        _ => "Status",
    };
    let payload = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        response.status,
        reason,
        response.body.len(),
        response.body
    );
    let _ = stream.write_all(payload.as_bytes());
    let _ = stream.flush();
}

/// A unique scratch directory for one test.
pub fn scratch_dir(label: &str) -> PathBuf {
    testdir::unique_dir(&format!("daycare-test-{label}"))
}

/// Where the refusing shim records that it was reached.
pub fn claude_marker(root: &PathBuf) -> PathBuf {
    root.join("real-claude-was-launched")
}

/// A PATH whose only `claude` refuses to run.
///
/// A test that launches Claude must say which binary it means. When one does
/// not, the runner falls back to the name `claude` and finds the user's real
/// install — a real model call, on a real subscription, from a unit test. This
/// makes that mistake fail loudly and immediately instead of succeeding
/// quietly, which is the only reason it went unnoticed for a day.
///
/// The returned PATH keeps `/usr/bin` and `/bin` so ordinary tools still work,
/// but puts the refusing shim first, where a bare `claude` will find it.
pub fn no_claude_path(root: &PathBuf) -> String {
    let bin = root.join("no-claude-bin");
    std::fs::create_dir_all(&bin).unwrap();
    let shim = bin.join("claude");
    // The shim leaves a marker before refusing. Refusing alone makes the
    // mistake harmless but invisible — the detached poller's exit code is
    // observed by nobody, so the test still passes and the next author learns
    // nothing. `Install`'s Drop reads this marker and fails the test.
    std::fs::write(
        &shim,
        format!(
            "#!/bin/sh\n\
             printf '%s\\n' \"$*\" >> {marker}\n\
             echo 'refusing to run: this test launched the real claude.' >&2\n\
             echo 'Pass --claude-bin <fake> so the suite never calls a model.' >&2\n\
             exit 97\n",
            marker = claude_marker(root).display()
        ),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    format!("{}:/usr/bin:/bin", bin.display())
}

/// A stand-in for the `claude` binary. It records the argv, environment, and
/// stdin it was given, then prints `stream` on stdout. `delay_secs` lets a test
/// exercise the turn timeout without waiting on a model.
pub fn fake_claude(dir: &PathBuf, stream: &str, delay_secs: u64, exit_code: i32) -> PathBuf {
    let stream_file = dir.join("canned-stream.jsonl");
    std::fs::write(&stream_file, stream).unwrap();
    // The homecoming double keeps the init event as the real capture reports
    // it — ToolSearch plus the daycare server's tools, server connected —
    // because a live homecoming runs with the memory tools reachable. It
    // drops every in-visit tool call: this Claude looked back and chose to
    // keep nothing, which is a valid homecoming.
    let private_stream_file = dir.join("canned-private-stream.jsonl");
    let private_stream = stream
        .lines()
        .filter_map(|line| {
            let event = serde_json::from_str::<serde_json::Value>(line).ok()?;
            let keep = (event["type"] == "system" && event["subtype"] == "init")
                || event["type"] == "result";
            if !keep {
                return None;
            }
            Some(serde_json::to_string(&event).unwrap())
        })
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    std::fs::write(&private_stream_file, private_stream).unwrap();
    // The day report runs tool-free: no tools, no MCP server, no calls.
    let dayreport_stream_file = dir.join("canned-dayreport-stream.jsonl");
    let dayreport_stream = stream
        .lines()
        .filter_map(|line| {
            let mut event = serde_json::from_str::<serde_json::Value>(line).ok()?;
            let keep = (event["type"] == "system" && event["subtype"] == "init")
                || event["type"] == "result";
            if !keep {
                return None;
            }
            if event["type"] == "system" {
                event["tools"] = serde_json::json!([]);
                event["mcp_servers"] = serde_json::json!([]);
            }
            Some(serde_json::to_string(&event).unwrap())
        })
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    std::fs::write(&dayreport_stream_file, dayreport_stream).unwrap();
    let script = dir.join("fake-claude.sh");
    let body = format!(
        r#"#!/bin/sh
if [ "$1" = "auth" ] && [ "$2" = "status" ]; then
  printf '%s\n' '{{"loggedIn":true,"authMethod":"claude.ai","apiProvider":"firstParty","subscriptionType":"max"}}'
  exit 0
fi
if [ -n "$DAYCARE_USAGE_SAMPLER" ]; then
  printf '%s\n' 'usage' >> "{usage_calls}"
  printf '%s\n' "$@" >> "{usage_argv}"
  IFS= read -r usage_command
  if [ -f "{usage_generation}" ]; then
    usage_generation=$(cat "{usage_generation}")
  else
    usage_generation=9999999999999
  fi
  usage_generation=$((usage_generation + 1))
  printf '%s\n' "$usage_generation" > "{usage_generation}"
  mkdir -p "$HOME"
  printf '%s\n' "{{\"cachedUsageUtilization\":{{\"fetchedAtMs\":$usage_generation,\"utilization\":{{\"limits\":[{{\"kind\":\"weekly_all\",\"group\":\"weekly\",\"percent\":64,\"resets_at\":\"2026-09-02T07:00:00Z\",\"is_active\":false,\"scope\":null}}]}}}}}}" > "$HOME/.claude.json"
  printf '%s\n' 'Refreshing...' 'Current week (all models)' '64% used' 'Resets Sep 2'
  exit 0
fi
record="{record}"
: > "$record.argv"
printf '%s\n' '-- call --' >> "$record.argv.all"
assigned_session=""
capture_session=0
for arg in "$@"; do
  printf '%s\n' "$arg" >> "$record.argv"
  printf '%s\n' "$arg" >> "$record.argv.all"
  if [ "$capture_session" -eq 1 ]; then
    assigned_session="$arg"
    capture_session=0
  elif [ "$arg" = "--session-id" ] || [ "$arg" = "--resume" ]; then
    capture_session=1
  fi
done
tee -a "$record.stdin.all" > "$record.stdin"
env > "$record.env"
pwd > "$record.cwd"
if [ {delay} -gt 0 ]; then sleep {delay}; fi
selected_stream="{stream_file}"
if grep -q 'Your visit is over and you are on your way home' "$record.stdin"; then
  selected_stream="{private_stream_file}"
elif grep -q 'your owner will see what you write here' "$record.stdin"; then
  selected_stream="{dayreport_stream_file}"
fi
if [ -n "$assigned_session" ]; then
  if [ "$selected_stream" = "{private_stream_file}" ] && [ -f "$record.pause-private" ]; then
    sed "s/18f44c2e-ff64-4e94-a89d-bdbeaa9ab9f7/$assigned_session/g" "$selected_stream" | {{
      IFS= read -r first_line
      printf '%s\n' "$first_line"
      sleep "$(cat "$record.pause-private")"
      cat
    }}
  else
    sed "s/18f44c2e-ff64-4e94-a89d-bdbeaa9ab9f7/$assigned_session/g" "$selected_stream"
  fi
else
  cat "$selected_stream"
fi
exit {exit_code}
"#,
        record = dir.join("call").display(),
        usage_calls = dir.join("usage-calls").display(),
        usage_argv = dir.join("usage-argv").display(),
        usage_generation = dir.join("usage-generation").display(),
        delay = delay_secs,
        stream_file = stream_file.display(),
        private_stream_file = private_stream_file.display(),
        dayreport_stream_file = dayreport_stream_file.display(),
        exit_code = exit_code
    );
    std::fs::write(&script, body).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    script
}

/// A stale-session double: resumed sessions exit before reading input, while a
/// fresh session accepts the same turn and succeeds. Each launch appends its
/// argv so the end-to-end test can prove both halves happened in order.
pub fn fake_claude_stale_resume(dir: &PathBuf, stream: &str) -> PathBuf {
    let stream_file = dir.join("stale-resume-stream.jsonl");
    std::fs::write(&stream_file, stream).unwrap();
    let script = dir.join("fake-claude-stale-resume.sh");
    let body = format!(
        r#"#!/bin/sh
if [ "$1" = "auth" ] && [ "$2" = "status" ]; then
  printf '%s\n' '{{"loggedIn":true,"authMethod":"claude.ai","apiProvider":"firstParty","subscriptionType":"max"}}'
  exit 0
fi
if [ -n "$DAYCARE_USAGE_SAMPLER" ]; then
  IFS= read -r usage_command
  mkdir -p "$HOME"
  printf '%s\n' '{{"cachedUsageUtilization":{{"fetchedAtMs":9999999999999,"utilization":{{"limits":[{{"kind":"weekly_all","group":"weekly","percent":64,"resets_at":"2026-09-02T07:00:00Z","is_active":false,"scope":null}}]}}}}}}' > "$HOME/.claude.json"
  printf '%s\n' 'Refreshing...' 'Current week (all models)' '64% used' 'Resets Sep 2'
  exit 0
fi
record="{record}"
printf '%s\n' '-- call --' >> "$record.argv.all"
assigned_session=""
capture_session=0
resuming=0
for arg in "$@"; do
  printf '%s\n' "$arg" >> "$record.argv.all"
  if [ "$capture_session" -eq 1 ]; then
    assigned_session="$arg"
    capture_session=0
  elif [ "$arg" = "--session-id" ]; then
    capture_session=1
  elif [ "$arg" = "--resume" ]; then
    resuming=1
    capture_session=1
  fi
done
if [ "$resuming" -eq 1 ]; then
  exit 0
fi
tee -a "$record.stdin.all" > "$record.stdin"
sed "s/18f44c2e-ff64-4e94-a89d-bdbeaa9ab9f7/$assigned_session/g" "{stream_file}"
"#,
        record = dir.join("call").display(),
        stream_file = stream_file.display(),
    );
    std::fs::write(&script, body).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    script
}

pub fn recorded_argv(dir: &PathBuf) -> Vec<String> {
    std::fs::read_to_string(dir.join("call.argv"))
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect()
}

pub fn recorded_stdin(dir: &PathBuf) -> String {
    std::fs::read_to_string(dir.join("call.stdin")).unwrap_or_default()
}

/// Every prompt this fake claude was given, in order.
///
/// `recorded_stdin` holds only the last call, which is the wrong one for a
/// visit: the homecoming turn runs after the world turn and overwrites it.
pub fn recorded_stdin_all(dir: &PathBuf) -> String {
    std::fs::read_to_string(dir.join("call.stdin.all")).unwrap_or_default()
}

pub fn recorded_env(dir: &PathBuf) -> HashMap<String, String> {
    std::fs::read_to_string(dir.join("call.env"))
        .unwrap_or_default()
        .lines()
        .filter_map(|line| line.split_once('='))
        .map(|(name, value)| (name.to_string(), value.to_string()))
        .collect()
}

pub fn recorded_cwd(dir: &PathBuf) -> String {
    std::fs::read_to_string(dir.join("call.cwd"))
        .unwrap_or_default()
        .trim()
        .to_string()
}

/// A real Claude Code 2.1.220 daycare turn: four daycare MCP tools, four tool
/// calls, and a result. Captured live on 2026-08-05, paths scrubbed.
pub fn fixture_stream() -> String {
    std::fs::read_to_string(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/daycare-turn-2.1.220.jsonl"),
    )
    .unwrap()
}

/// The fixture is a real capture, so its `init` event names the cwd of the run
/// that produced it. A test driving `run_turn` has its own workspace; this
/// rewrites just that field so the sandbox check sees a consistent turn.
pub fn fixture_stream_from(workspace: &std::path::Path) -> String {
    patch_init(&fixture_stream(), |init| {
        init.insert(
            "cwd".to_string(),
            serde_json::Value::String(workspace.display().to_string()),
        );
    })
}

/// Rewrite the `system`/`init` event of a stream. Used to simulate a child that
/// reports a sandbox other than the one we asked for.
pub fn patch_init<F>(stream: &str, edit: F) -> String
where
    F: Fn(&mut serde_json::Map<String, serde_json::Value>),
{
    let mut out = String::new();
    for line in stream.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let mut event: serde_json::Value = serde_json::from_str(line).unwrap();
        if event.get("type").and_then(serde_json::Value::as_str) == Some("system")
            && event.get("subtype").and_then(serde_json::Value::as_str) == Some("init")
        {
            edit(event.as_object_mut().unwrap());
        }
        out.push_str(&serde_json::to_string(&event).unwrap());
        out.push('\n');
    }
    out
}
