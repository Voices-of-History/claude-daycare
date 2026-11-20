//! Open a new terminal window that runs one shell script.
//!
//! `challenge start` has to run in a real terminal: it starts the person's
//! own agent interactively and waits for it, and `challenge submit` asks for a
//! typed yes. When the setup is driven by the person's agent (the setup prompt
//! from the site), that agent's shell tool is not a terminal, so the most it
//! could do before was print a command for the person to copy. This opens the
//! window for them instead, with the start already typed and running.
//!
//! Nothing here widens what runs: the window runs this same binary with the
//! same arguments the person could have typed, and every typed-yes gate stays
//! in that window, in front of the person.
//!
//! macOS uses Terminal.app through `osascript`. Linux desktops try the usual
//! terminal launchers, and only when a display is present. WSL and headless
//! machines return `Ok(None)`, and the caller prints the script to paste.

use crate::paths::shell_quote;
use crate::{Error, Result};
use std::process::{Command, Stdio};

/// Which launcher opened the window.
pub type Launcher = &'static str;

/// Open a terminal window running `script` with `sh -c`. `Ok(None)` means no
/// graphical terminal is available here; the caller should print `script`.
pub fn open(script: &str) -> Result<Option<Launcher>> {
    if std::env::var_os("DAYCARE_NO_NEW_WINDOW").is_some() {
        return Ok(None);
    }
    match std::env::consts::OS {
        "macos" => open_macos(script).map(Some),
        "linux" => Ok(open_linux(script)),
        _ => Ok(None),
    }
}

fn open_macos(script: &str) -> Result<Launcher> {
    let status = Command::new("osascript")
        .args(applescript_args(script))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .map_err(|error| Error::new(format!("could not run osascript: {error}")))?;
    if status.status.success() {
        Ok("Terminal")
    } else {
        let stderr = String::from_utf8_lossy(&status.stderr);
        let excerpt: String = stderr.chars().take(200).collect();
        Err(Error::new(format!(
            "Terminal did not open a window ({}): {excerpt}",
            status.status
        )))
    }
}

/// The `osascript` arguments that open one Terminal window running `script`.
/// Terminal's `do script` hands the text to the person's login shell, so the
/// script is wrapped in `sh -c` to run the same everywhere.
pub fn applescript_args(script: &str) -> Vec<String> {
    let command = format!("sh -c {}", shell_quote(script));
    vec![
        "-e".into(),
        "tell application \"Terminal\" to activate".into(),
        "-e".into(),
        format!(
            "tell application \"Terminal\" to do script \"{}\"",
            applescript_escape(&command)
        ),
    ]
}

/// Escape text for an AppleScript double-quoted string literal.
pub fn applescript_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            _ => out.push(c),
        }
    }
    out
}

fn open_linux(script: &str) -> Option<Launcher> {
    if crate::wsl::detect().is_some() {
        // A Windows-side window would start outside this distro's PATH and
        // login; printing the command is the honest answer until it is tested.
        return None;
    }
    let has_display =
        std::env::var_os("DISPLAY").is_some() || std::env::var_os("WAYLAND_DISPLAY").is_some();
    if !has_display {
        return None;
    }
    for (launcher, args) in linux_candidates(script) {
        let spawned = Command::new(launcher)
            .args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
        if spawned.is_ok() {
            return Some(launcher);
        }
    }
    None
}

/// Terminal launchers to try on a Linux desktop, in order, with the arguments
/// that make each one run `sh -c <script>`.
pub fn linux_candidates(script: &str) -> Vec<(Launcher, Vec<String>)> {
    let sh = |prefix: &[&str]| -> Vec<String> {
        prefix
            .iter()
            .map(|s| s.to_string())
            .chain(["sh".to_string(), "-c".to_string(), script.to_string()])
            .collect()
    };
    vec![
        ("x-terminal-emulator", sh(&["-e"])),
        ("gnome-terminal", sh(&["--"])),
        ("konsole", sh(&["-e"])),
        (
            "xfce4-terminal",
            vec!["-x".into(), "sh".into(), "-c".into(), script.into()],
        ),
        ("kitty", sh(&[])),
        ("alacritty", sh(&["-e"])),
        ("xterm", sh(&["-e"])),
    ]
}

/// The script a challenge window runs: start the challenge, and when the
/// agent session ends cleanly, go straight to `challenge submit`, which shows
/// every file and waits for the person's typed yes. The window then stays
/// open on a shell so the person can read what happened.
pub fn challenge_script(start_argv: &[String], submit_argv: &[String]) -> String {
    let join = |argv: &[String]| {
        argv.iter()
            .map(|word| shell_quote(word))
            .collect::<Vec<_>>()
            .join(" ")
    };
    format!(
        "{} && {}; echo; echo 'You can close this window.'; exec \"${{SHELL:-/bin/sh}}\"",
        join(start_argv),
        join(submit_argv)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn applescript_escapes_quotes_and_backslashes() {
        assert_eq!(applescript_escape(r#"a "b" \c"#), r#"a \"b\" \\c"#);
    }

    #[test]
    fn the_terminal_script_is_one_quoted_sh_command() {
        let args = applescript_args("echo 'hi there' && echo \"x\"");
        assert_eq!(args[0], "-e");
        assert_eq!(args[2], "-e");
        assert!(args[3].starts_with("tell application \"Terminal\" to do script \"sh -c "));
        assert!(args[3].ends_with('"'));
        // The inner double quotes are escaped for AppleScript.
        assert!(args[3].contains("\\\"x\\\""), "{}", args[3]);
    }

    #[test]
    fn the_challenge_script_submits_only_after_a_clean_start() {
        let script = challenge_script(
            &[
                "/home/me/.local/bin/daycare-runner".into(),
                "challenge".into(),
                "start".into(),
                "abc-123".into(),
            ],
            &[
                "/home/me/.local/bin/daycare-runner".into(),
                "challenge".into(),
                "submit".into(),
                "abc-123".into(),
            ],
        );
        let (start, rest) = script.split_once(" && ").unwrap();
        assert!(start.ends_with("'challenge' 'start' 'abc-123'"), "{start}");
        assert!(rest.contains("'challenge' 'submit' 'abc-123';"), "{rest}");
        assert!(rest.ends_with("exec \"${SHELL:-/bin/sh}\""), "{rest}");
    }

    #[test]
    fn every_linux_candidate_ends_with_the_script() {
        for (_, args) in linux_candidates("true") {
            assert_eq!(args.last().map(String::as_str), Some("true"));
        }
    }
}
