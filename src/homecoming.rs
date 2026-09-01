//! The homecoming reader.
//!
//! The session that lives a visit is the wrong one to write its memories: by
//! the end it may have been compacted, and it was told during the visit not to
//! manage its own memory. So when a visit ends the runner spawns a FRESH
//! session — never a resume — hands it the visit's complete turn archives
//! rendered as one transcript, and asks it to read the whole thing before it
//! decides what to keep. This module renders that transcript and writes the
//! prompt that points the reader at it. Who may launch the reader, and what it
//! may call, stays in `launch.rs` / `turn.rs`.

use crate::paths::{create_private_dir, sanitize_segment, write_atomic, Layout};
use crate::visit::VisitRecord;
use crate::{Error, Result};
use serde_json::Value;
use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

/// Directory inside the workspace that holds rendered transcripts. The reader
/// may Read this directory and nothing else on disk.
pub const TRANSCRIPT_DIR: &str = "homecoming";

/// The permission rule that grants Read on that directory alone.
pub const READ_RULE: &str = "Read(./homecoming/**)";

/// How many lines the reader is told to take per Read call.
pub const READ_CHUNK_LINES: usize = 250;

/// Largest tool result kept verbatim. A world snapshot can run to tens of
/// kilobytes; past this the tail is cut with a marker so one result cannot
/// drown the turns around it.
const RESULT_MAX_CHARS: usize = 6000;

/// A rendered transcript on disk: where it is, how the reader names it, and
/// how long it is (so the prompt can say "read through line N").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transcript {
    pub path: PathBuf,
    pub relative: String,
    pub lines: usize,
}

/// Render every recorded turn archive of `record` into one markdown
/// transcript, in visit order. Fails — and names the file — when a visit has
/// no recorded archives or any archive is missing, unreadable, or not stream
/// JSON: a homecoming is never written from nothing.
pub fn render(layout: &Layout, record: &VisitRecord) -> Result<String> {
    if record.turn_archives.is_empty() {
        return Err(Error::new(format!(
            "visit {} has no recorded turn archives; a homecoming cannot be written from nothing",
            record.visit_id
        )));
    }
    let total = record.turn_archives.len();
    let mut out = String::new();
    let _ = writeln!(
        out,
        "# Daycare visit {} — {}",
        record.visit_id, record.identity_name
    );
    let _ = writeln!(out);
    let _ = writeln!(out, "Started: {}", record.started_at);
    if let Some(ended) = &record.ended_at {
        let _ = writeln!(out, "Ended: {ended}");
    }
    if let Some(reason) = record.end_reason {
        let _ = writeln!(out, "End reason: {reason:?}");
    }
    match &record.instructions {
        Some(instructions) => {
            let _ = writeln!(
                out,
                "Your owner's instructions for the visit: {instructions}"
            );
        }
        None => {
            let _ = writeln!(out, "Your owner gave no instructions for the visit.");
        }
    }
    let _ = writeln!(
        out,
        "Turns recorded: {total} (ledger: {} used, {} failed, {} held)",
        record.ledger.turns_used, record.ledger.turns_failed, record.ledger.turns_held
    );
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "Each turn below is the raw record of one Claude Code run: `[you said]` is \
         your own text, `[you called <tool>]` is a tool call with its input, and \
         `[result of <tool>]` is what the tool returned. Results are the record; \
         your words are what you believed at the time."
    );

    for (index, command_id) in record.turn_archives.iter().enumerate() {
        let path = layout.turn_file(command_id);
        let text = std::fs::read_to_string(&path).map_err(|error| {
            Error::new(format!(
                "turn archive {} for visit {} is unreadable: {error}",
                path.display(),
                record.visit_id
            ))
        })?;
        let _ = writeln!(out);
        let _ = writeln!(
            out,
            "## Turn {} of {total} (command {command_id})",
            index + 1
        );
        let _ = writeln!(out);
        render_turn(&mut out, &path, &text)?;
    }
    Ok(out)
}

fn render_turn(out: &mut String, path: &Path, text: &str) -> Result<()> {
    let mut names_by_id: HashMap<String, String> = HashMap::new();
    let mut rendered_anything = false;
    for (index, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let event: Value = serde_json::from_str(line).map_err(|error| {
            Error::new(format!(
                "turn archive {} line {} is not stream JSON: {error}",
                path.display(),
                index + 1
            ))
        })?;
        match event.get("type").and_then(Value::as_str) {
            Some("assistant") => {
                for block in content_blocks(&event) {
                    match block.get("type").and_then(Value::as_str) {
                        Some("text") => {
                            if let Some(text) = block.get("text").and_then(Value::as_str) {
                                if !text.trim().is_empty() {
                                    rendered_anything = true;
                                    let _ = writeln!(out, "[you said]");
                                    let _ = writeln!(out, "{}", text.trim());
                                    let _ = writeln!(out);
                                }
                            }
                        }
                        Some("tool_use") => {
                            let name = block
                                .get("name")
                                .and_then(Value::as_str)
                                .unwrap_or("<unnamed tool>")
                                .to_string();
                            if let Some(id) = block.get("id").and_then(Value::as_str) {
                                names_by_id.insert(id.to_string(), name.clone());
                            }
                            rendered_anything = true;
                            let _ = writeln!(out, "[you called {name}]");
                            let input = block
                                .get("input")
                                .map(|input| input.to_string())
                                .unwrap_or_else(|| "{}".into());
                            let _ = writeln!(out, "{}", clip(&input));
                            let _ = writeln!(out);
                        }
                        _ => {}
                    }
                }
            }
            Some("user") => {
                for block in content_blocks(&event) {
                    if block.get("type").and_then(Value::as_str) != Some("tool_result") {
                        continue;
                    }
                    let name = block
                        .get("tool_use_id")
                        .and_then(Value::as_str)
                        .and_then(|id| names_by_id.get(id))
                        .map(String::as_str)
                        .unwrap_or("<unknown tool>");
                    let is_error = block.get("is_error").and_then(Value::as_bool) == Some(true);
                    rendered_anything = true;
                    let _ = writeln!(
                        out,
                        "[result of {name}{}]",
                        if is_error { ": error" } else { "" }
                    );
                    let _ = writeln!(out, "{}", clip(&result_text(block)));
                    let _ = writeln!(out);
                }
            }
            Some("result") => {
                let subtype = event.get("subtype").and_then(Value::as_str).unwrap_or("");
                if subtype != "success"
                    || event.get("is_error").and_then(Value::as_bool) == Some(true)
                {
                    let _ = writeln!(out, "[this turn ended abnormally: {subtype}]");
                    let _ = writeln!(out);
                }
            }
            _ => {}
        }
    }
    if !rendered_anything {
        let _ = writeln!(out, "(no words or tool calls were recorded for this turn)");
    }
    Ok(())
}

fn content_blocks(event: &Value) -> Vec<&Value> {
    event
        .get("message")
        .and_then(|message| message.get("content"))
        .and_then(Value::as_array)
        .map(|blocks| blocks.iter().collect())
        .unwrap_or_default()
}

fn result_text(block: &Value) -> String {
    match block.get("content") {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|part| part.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        Some(other) => other.to_string(),
        None => String::new(),
    }
}

fn clip(text: &str) -> String {
    let text = text.trim();
    if text.chars().count() <= RESULT_MAX_CHARS {
        return text.to_string();
    }
    let kept: String = text.chars().take(RESULT_MAX_CHARS).collect();
    format!("{kept}\n[… cut here; the full result was longer]")
}

/// Write the rendered transcript into the workspace, owner-only, where the
/// reader's Read grant can reach it and nothing else can.
pub fn write(workspace_dir: &Path, visit_id: &str, text: &str) -> Result<Transcript> {
    let dir = workspace_dir.join(TRANSCRIPT_DIR);
    create_private_dir(&dir)?;
    let file = format!("{}.md", sanitize_segment(visit_id));
    let path = dir.join(&file);
    write_atomic(&path, text.as_bytes(), 0o600)?;
    Ok(Transcript {
        path,
        relative: format!("{TRANSCRIPT_DIR}/{file}"),
        lines: text.lines().count(),
    })
}

/// Read back the transcript `write` left for this visit, for upload to the
/// platform at homecoming. `None` when no transcript was rendered.
pub fn read(workspace_dir: &Path, visit_id: &str) -> Result<Option<String>> {
    let path = workspace_dir
        .join(TRANSCRIPT_DIR)
        .join(format!("{}.md", sanitize_segment(visit_id)));
    if !path.exists() {
        return Ok(None);
    }
    Ok(Some(std::fs::read_to_string(&path)?))
}

/// The reader's prompt: a fresh session, told where the record is, told to
/// read all of it before it decides anything, then the ordinary homecoming
/// ask (`facts_and_reflection`: any match facts, then the reflection).
pub fn reader_message(transcript: &Transcript, facts_and_reflection: &str) -> String {
    format!(
        "Your visit is over and you are on your way home. This is a fresh session: you \
         do not carry the visit in your own memory, and nothing here asks you to trust a \
         summary of it. The complete record of the visit — every turn in order, what you \
         said, every tool you called, and what came back — is the file `{relative}` in \
         this workspace, {lines} lines long. Read all of it before you decide anything: \
         use Read in chunks of {chunk} lines (offset and limit), starting at line 1 and \
         continuing until you have read line {lines}. Do not stop early, do not skim, and \
         do not save anything until the last line is read. Keep three things apart as \
         you go: what you were asked to do, what you said you did, and what the record \
         shows actually happened — tool results are the record, and where your words \
         and the record differ, the record wins. Behind anything you keep, know the \
         evidence: the turn, the call, the result.\n\n{facts_and_reflection}",
        relative = transcript.relative,
        lines = transcript.lines,
        chunk = READ_CHUNK_LINES,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testdir::unique_path;
    use crate::visit::{Budget, VisitRecord};

    fn archive(lines: &[&str]) -> String {
        let mut text = lines.join("\n");
        text.push('\n');
        text
    }

    fn record(layout: &Layout, ids: &[&str]) -> VisitRecord {
        let mut record = VisitRecord::open(
            "visit-1",
            "identity-1",
            "Pip",
            Budget::default(),
            Some("try Debate League".into()),
            "2026-09-01T18:00:00Z",
        );
        record.turn_archives = ids.iter().map(|id| id.to_string()).collect();
        record.ledger.record_turn(true, None);
        for id in ids {
            let _ = layout.turn_file(id);
        }
        record
    }

    #[test]
    fn renders_words_calls_and_results_in_visit_order() {
        let layout = Layout::at(unique_path("daycare-homecoming-render"));
        create_private_dir(&layout.turns_dir()).unwrap();
        std::fs::write(
            layout.turn_file("turn-a"),
            archive(&[
                r#"{"type":"system","subtype":"init","session_id":"s1","tools":["ToolSearch"]}"#,
                r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Looking around."},{"type":"tool_use","id":"tu1","name":"mcp__daycare__daycare_match_join","input":{"match_id":"m9"}}]}}"#,
                r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"tu1","content":[{"type":"text","text":"{\"seat\":\"held\"}"}]}]}}"#,
                r#"{"type":"result","subtype":"success","session_id":"s1","result":"Looking around."}"#,
            ]),
        )
        .unwrap();
        std::fs::write(
            layout.turn_file("turn-b"),
            archive(&[
                r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Second turn words."}]}}"#,
                r#"{"type":"result","subtype":"error_max_turns","session_id":"s1"}"#,
            ]),
        )
        .unwrap();
        let record = record(&layout, &["turn-a", "turn-b"]);

        let text = render(&layout, &record).unwrap();

        assert!(text.contains("# Daycare visit visit-1 — Pip"), "{text}");
        assert!(text.contains("Your owner's instructions for the visit: try Debate League"));
        let a = text.find("## Turn 1 of 2 (command turn-a)").unwrap();
        let b = text.find("## Turn 2 of 2 (command turn-b)").unwrap();
        assert!(a < b);
        assert!(text.contains("[you said]\nLooking around."));
        assert!(
            text.contains("[you called mcp__daycare__daycare_match_join]\n{\"match_id\":\"m9\"}")
        );
        assert!(text.contains("[result of mcp__daycare__daycare_match_join]\n{\"seat\":\"held\"}"));
        assert!(text.contains("[you said]\nSecond turn words."));
        assert!(text.contains("[this turn ended abnormally: error_max_turns]"));
        // The init line and the duplicate `result` text are not turn content.
        assert!(!text.contains("ToolSearch"));
        assert_eq!(text.matches("Looking around.").count(), 1);
    }

    #[test]
    fn a_missing_archive_fails_by_name_instead_of_rendering_a_partial_visit() {
        let layout = Layout::at(unique_path("daycare-homecoming-missing"));
        create_private_dir(&layout.turns_dir()).unwrap();
        std::fs::write(
            layout.turn_file("turn-a"),
            archive(&[
                r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Hi."}]}}"#,
            ]),
        )
        .unwrap();
        let record = record(&layout, &["turn-a", "turn-gone"]);

        let error = render(&layout, &record).unwrap_err().to_string();

        assert!(error.contains("turn-gone.jsonl"), "{error}");
        assert!(error.contains("visit-1"), "{error}");
    }

    #[test]
    fn a_visit_with_no_recorded_archives_cannot_be_read_back() {
        let layout = Layout::at(unique_path("daycare-homecoming-empty"));
        let record = record(&layout, &[]);
        let error = render(&layout, &record).unwrap_err().to_string();
        assert!(error.contains("no recorded turn archives"), "{error}");
        assert!(error.contains("written from nothing"), "{error}");
    }

    #[test]
    fn a_corrupt_archive_line_is_named() {
        let layout = Layout::at(unique_path("daycare-homecoming-corrupt"));
        create_private_dir(&layout.turns_dir()).unwrap();
        std::fs::write(
            layout.turn_file("turn-a"),
            "{\"type\":\"assistant\"}\nnot json\n",
        )
        .unwrap();
        let record = record(&layout, &["turn-a"]);
        let error = render(&layout, &record).unwrap_err().to_string();
        assert!(error.contains("line 2"), "{error}");
    }

    #[test]
    fn long_results_are_cut_with_a_marker() {
        let long = "x".repeat(RESULT_MAX_CHARS + 10);
        let clipped = clip(&long);
        assert!(clipped.ends_with("[… cut here; the full result was longer]"));
        assert!(clipped.starts_with(&"x".repeat(RESULT_MAX_CHARS)));
        assert_eq!(clip("short"), "short");
    }

    #[test]
    fn the_transcript_lands_owner_only_under_the_read_grant() {
        let workspace = unique_path("daycare-homecoming-write");
        create_private_dir(&workspace).unwrap();
        let transcript = write(&workspace, "visit/../1", "a\nb\nc\n").unwrap();
        assert_eq!(transcript.lines, 3);
        assert!(transcript.relative.starts_with("homecoming/"));
        assert!(!transcript.relative.contains(".."));
        assert!(transcript.path.starts_with(workspace.join(TRANSCRIPT_DIR)));
        assert_eq!(
            std::fs::read_to_string(&transcript.path).unwrap(),
            "a\nb\nc\n"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&transcript.path)
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600);
            let dir_mode = std::fs::metadata(workspace.join(TRANSCRIPT_DIR))
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(dir_mode, 0o700);
        }
        assert_eq!(READ_RULE, format!("Read(./{TRANSCRIPT_DIR}/**)"));
    }

    #[test]
    fn the_reader_is_told_to_read_the_whole_record_before_saving() {
        let transcript = Transcript {
            path: PathBuf::from("/w/homecoming/v.md"),
            relative: "homecoming/v.md".into(),
            lines: 812,
        };
        let message = reader_message(&transcript, "Now look back. daycare_memory_save.");
        assert!(message.contains("fresh session"));
        assert!(message.contains("`homecoming/v.md`"));
        assert!(message.contains("812 lines long"));
        assert!(message.contains("until you have read line 812"));
        assert!(message.contains(&format!("chunks of {READ_CHUNK_LINES} lines")));
        assert!(message.contains("do not save anything until the last line is read"));
        assert!(message.contains("what you were asked to do, what you said you did, and what the record shows actually happened"));
        assert!(message.ends_with("Now look back. daycare_memory_save."));
        // Same register as the rest of the homecoming: an ask, not an order.
        assert!(!message.to_ascii_lowercase().contains("must"));
    }
}
