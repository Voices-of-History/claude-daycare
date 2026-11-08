//! The interactive challenge session (plan task N9): verify, read the meter,
//! start the owner's client in this terminal, wait for it, read the meter
//! again.
//!
//! The client is started with exactly the program, arguments, folder and
//! environment `--print` shows (`exclusion::LaunchSpec`): the session marker
//! set, `ANTHROPIC_API_KEY` and every Daycare credential variable removed. The
//! runner's own token is held in memory and never reaches the child.
//!
//! While the client runs, this process ignores Ctrl-C and Ctrl-\ so the
//! keys reach the client (Claude Code and Codex both use Ctrl-C themselves)
//! and the runner survives to take the end reading. The child gets the
//! default handling back before it starts.

use super::client::{ChallengeClient, ExclusionPlan, Verification};
use super::exclusion::{Excluded, LaunchSpec, PlanInput};
use super::meter::{self, MeterContext, Reading};
use crate::approval::scan::Client;
use crate::out::Out;
use crate::paths::Layout;
use crate::{Error, Result};
use serde_json::{json, Value};
use std::process::{Command, Stdio};

/// How the client's process ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionExit {
    pub code: Option<i32>,
    pub signal: Option<i32>,
}

impl SessionExit {
    pub fn to_json(self) -> Value {
        json!({ "started": true, "exit_code": self.code, "signal": self.signal })
    }

    fn describe(self) -> String {
        match (self.code, self.signal) {
            (Some(code), _) => format!("exit code {code}"),
            (None, Some(signal)) => format!("signal {signal}"),
            (None, None) => "an unknown status".into(),
        }
    }
}

/// A `Command` for `spec`: its program, folder and environment, no arguments.
pub fn base_command(spec: &LaunchSpec) -> Command {
    let mut command = Command::new(&spec.program);
    command.current_dir(&spec.cwd);
    for name in &spec.env_unset {
        command.env_remove(name);
    }
    for (name, value) in &spec.env_set {
        command.env(name, value);
    }
    command
}

/// SIGINT and SIGQUIT ignored in this process until dropped.
struct TerminalKeysGoToTheClient {
    previous: Vec<(libc::c_int, libc::sighandler_t)>,
}

impl TerminalKeysGoToTheClient {
    fn hold() -> Self {
        let previous = [libc::SIGINT, libc::SIGQUIT]
            .into_iter()
            // SAFETY: setting a disposition to SIG_IGN has no preconditions.
            .map(|signal| (signal, unsafe { libc::signal(signal, libc::SIG_IGN) }))
            .collect();
        TerminalKeysGoToTheClient { previous }
    }
}

impl Drop for TerminalKeysGoToTheClient {
    fn drop(&mut self) {
        for (signal, handler) in &self.previous {
            // SAFETY: restores the disposition this guard replaced.
            unsafe { libc::signal(*signal, *handler) };
        }
    }
}

/// Run the client in this terminal and wait for it. Stdin and stderr are
/// inherited; stdout too, unless `stdout_to_stderr` (for `--json`).
pub fn run_interactive(spec: &LaunchSpec, stdout_to_stderr: bool) -> Result<SessionExit> {
    use std::os::unix::process::{CommandExt, ExitStatusExt};
    let mut command = base_command(spec);
    command
        .args(&spec.args)
        .stdin(Stdio::inherit())
        .stderr(Stdio::inherit());
    if stdout_to_stderr {
        command.stdout(Stdio::from(std::io::stderr()));
    } else {
        command.stdout(Stdio::inherit());
    }
    // SAFETY: only async-signal-safe calls between fork and exec.
    unsafe {
        command.pre_exec(|| {
            libc::signal(libc::SIGINT, libc::SIG_DFL);
            libc::signal(libc::SIGQUIT, libc::SIG_DFL);
            Ok(())
        });
    }
    let _keys = TerminalKeysGoToTheClient::hold();
    let mut child = command.spawn().map_err(|error| {
        Error::new(format!(
            "could not start {}: {error}. Is it installed and on PATH?",
            spec.program
        ))
    })?;
    let status = child.wait()?;
    Ok(SessionExit {
        code: status.code(),
        signal: status.signal(),
    })
}

/// What the interactive start needs beyond the plan.
pub struct SessionRun<'a> {
    pub client: &'a dyn ChallengeClient,
    pub plan: &'a ExclusionPlan,
    pub input: &'a PlanInput<'a>,
    pub layout: &'a Layout,
    pub challenge_id: &'a str,
    pub title: &'a str,
    /// The approved copies, already as printed by `--print`.
    pub approved: Vec<Value>,
    pub excluded: &'a [Excluded],
    pub notes: Vec<&'static str>,
    pub now: fn() -> String,
}

fn say(out: Out, line: impl std::fmt::Display) {
    // Under --json, stdout is the one result; the person watching the
    // terminal still reads what happens on stderr.
    if out.json {
        eprintln!("{line}");
    } else {
        println!("{line}");
    }
}

fn print_verification(out: Out, product: &str, verification: &Verification) {
    for line in &verification.proven {
        say(out, format!("Checked: {line}."));
    }
    if !verification.manual.is_empty() {
        say(out, format!("Once {product} starts, check it yourself:"));
        for line in &verification.manual {
            say(out, format!("- {line}"));
        }
    }
}

fn reading_summary(reading: &Reading) -> String {
    match &reading.outcome {
        Ok(snapshot) => format!("{}% of the weekly allowance used", snapshot.used_percentage),
        Err(error) => format!("no reading ({error})"),
    }
}

/// `challenge start <id>` without `--print`.
pub fn run_session(run: SessionRun<'_>, out: Out) -> Result<()> {
    let SessionRun {
        client,
        plan,
        input,
        layout,
        challenge_id,
        title,
        approved,
        excluded,
        notes,
        now,
    } = run;
    let folder = input.folder;
    let product = client.product_name();

    // A refusal here launches nothing; the folder stays for a retry.
    let verification = client.verify(plan, input)?;

    say(out, format!("Set up {title} in {}.", folder.display()));
    say(
        out,
        format!(
            "{} approved item(s) brought in; {} left out.",
            approved.len(),
            excluded.len()
        ),
    );
    print_verification(out, product, &verification);
    for note in &notes {
        say(out, format!("- {note}"));
    }

    let context = MeterContext {
        client: client.client(),
        challenge_id,
        entry_id: input.entry_id,
    };
    say(out, "Reading your weekly usage before the session...");
    let start = meter::take(client, layout, now);
    let start_file = meter::write_start(
        folder,
        &meter::reading_json("start", &start, &context, None),
    )?;
    say(
        out,
        format!(
            "Usage reading before the session: {}. It is a reading, not a limit.",
            reading_summary(&start)
        ),
    );
    // SAFETY: isatty has no preconditions.
    if unsafe { libc::isatty(0) } != 1 {
        say(
            out,
            format!(
                "This is not a terminal, so {product} may not start as an interactive session. \
                 Run `challenge start` in your own terminal, or use --print and run the command \
                 yourself."
            ),
        );
    }
    say(out, format!("Starting {product} in {}.", folder.display()));
    say(out, "");

    let exit = client.interactive(plan, out.json);
    let session = match &exit {
        Ok(exit) => exit.to_json(),
        Err(error) => json!({ "started": false, "error": error.message() }),
    };
    say(out, "");
    say(out, "Reading your weekly usage after the session...");
    let end = meter::take(client, layout, now);
    let end_file = meter::write_end(
        folder,
        &meter::reading_json("end", &end, &context, Some(session.clone())),
    )?;
    let exit = exit?;

    out.emit(
        json!({
            "ok": true,
            "challenge_id": challenge_id,
            "entry_id": input.entry_id,
            "client": client.client(),
            "folder": folder,
            "launch": plan.launch.to_json(),
            "settings_file": plan.settings_file,
            "mcp_config_file": plan.mcp_config_file,
            "codex_home": plan.codex_home,
            "verification": {
                "proven": verification.proven,
                "manual": verification.manual,
            },
            "session": session,
            "meter": {
                "start_file": start_file,
                "end_file": end_file,
                "start": meter::reading_json("start", &start, &context, None),
                "end": meter::reading_json("end", &end, &context, None),
            },
            "approved": approved,
            "excluded": excluded.iter().map(|excluded| json!({
                "id": excluded.id, "how": excluded.how,
            })).collect::<Vec<_>>(),
            "notes": notes,
        }),
        || {
            println!("{product} ended with {}.", exit.describe());
            println!(
                "Usage reading after the session: {}.",
                reading_summary(&end)
            );
            println!(
                "Both readings are in {}.",
                folder.join(meter::METER_DIR).display()
            );
            println!(
                "Put what you want to submit in {}. Only output/ is ever uploaded, and only \
                 after you type yes.",
                folder.join("output").display()
            );
        },
    );
    Ok(())
}

/// Notes for the interactive launch, in place of `--print`'s.
pub fn notes(client: Client) -> Vec<&'static str> {
    let mut notes = vec![
        "Only output/ is ever uploaded, and only after you type yes.",
        "A program running as you can still read what you can read. Your account of how the \
         work was done is self-reported.",
    ];
    match client {
        Client::Claude => notes.push("Deny rules bind Claude's file tools, not shell commands."),
        Client::Codex => {
            notes.push("Codex's workspace-write sandbox limits writes to this folder, not reads.")
        }
    }
    notes
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    #[test]
    fn the_child_gets_exactly_the_launch_environment_and_folder() {
        let base = crate::testdir::unique_dir("daycare-interactive");
        std::fs::create_dir_all(&base).unwrap();
        let script = base.join("client.sh");
        crate::testdir::write_executable(
            &script,
            &format!(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" > {argv}\nenv > {env}\npwd > {cwd}\nexit 3\n",
                argv = base.join("argv").display(),
                env = base.join("env").display(),
                cwd = base.join("cwd").display(),
            ),
        );
        std::env::set_var("DAYCARE_LAUNCH_TEST_SECRET", "kept-out");
        let spec = LaunchSpec {
            program: script.to_string_lossy().into_owned(),
            args: vec!["--settings".into(), "a b".into()],
            cwd: base.clone(),
            env_set: BTreeMap::from([("DAYCARE_CHALLENGE_SESSION".to_string(), "e1".to_string())]),
            env_unset: vec!["DAYCARE_LAUNCH_TEST_SECRET".into()],
        };
        let exit = run_interactive(&spec, true).unwrap();
        assert_eq!(
            exit,
            SessionExit {
                code: Some(3),
                signal: None
            }
        );
        assert_eq!(
            std::fs::read_to_string(base.join("argv")).unwrap(),
            "--settings\na b\n"
        );
        let env = std::fs::read_to_string(base.join("env")).unwrap();
        assert!(env.contains("DAYCARE_CHALLENGE_SESSION=e1"), "{env}");
        assert!(!env.contains("kept-out"), "{env}");
        assert_eq!(
            PathBuf::from(std::fs::read_to_string(base.join("cwd")).unwrap().trim()),
            std::fs::canonicalize(&base).unwrap()
        );
        // A program that is not there is an error, not a silent pass.
        let missing = LaunchSpec {
            program: base.join("nope").to_string_lossy().into_owned(),
            ..spec
        };
        assert!(run_interactive(&missing, true).is_err());
        std::fs::remove_dir_all(base).ok();
    }
}
