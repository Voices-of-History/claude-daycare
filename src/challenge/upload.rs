//! Delivering an entry (plan task N16): one `POST` that starts a submission,
//! one signed `PUT` per file straight to storage, then the confirm.
//!
//! Every resubmission is a new version on the platform; only a confirmed
//! version counts, and only the newest can be confirmed. So a failure at any
//! step leaves the entry as it was, and the honest report is "not delivered".
//! In particular a closed challenge (409 `{error:"closed"}`, or 410) is shown
//! as not delivered and never as success — unlike the day report, where a 409
//! means a report already stands.
//!
//! Before any byte is uploaded, the platform's upload list must name exactly
//! the files in the listing the owner said yes to, each once, and the entry
//! must be the one this challenge folder was started for.

use crate::challenge::submit::{Declaration, OutputFile};
use crate::platform::{is_entries_closed, PlatformClient, SignedUploader};
use crate::self_update::sha256_hex;
use crate::{Error, Result};
use serde_json::{json, Value};
use std::collections::BTreeMap;

/// What the platform recorded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Delivered {
    pub submission_id: String,
    pub version: u64,
    pub entry_id: Option<String>,
    pub manifest_sha256: String,
}

/// The platform's canonical manifest hash (`manifestSha256` in the
/// platform's `uploads.ts`): sha256 of the JSON of the sorted
/// `[{path, bytes, sha256}]`, keys in that order, no whitespace.
pub fn manifest_sha256(files: &[OutputFile]) -> String {
    let mut sorted: Vec<&OutputFile> = files.iter().collect();
    sorted.sort_by(|a, b| a.path.as_bytes().cmp(b.path.as_bytes()));
    let items: Vec<String> = sorted
        .iter()
        .map(|file| {
            format!(
                "{{\"path\":{},\"bytes\":{},\"sha256\":{}}}",
                Value::String(file.path.clone()),
                file.bytes.len(),
                Value::String(file.sha256.clone())
            )
        })
        .collect();
    sha256_hex(format!("[{}]", items.join(",")).as_bytes())
}

/// The request body: the listing the owner saw, and their declaration.
pub fn submit_body(files: &[OutputFile], declaration: &Declaration) -> Value {
    json!({
        "files": files.iter().map(|file| json!({
            "path": file.path,
            "bytes": file.bytes.len(),
            "sha256": file.sha256,
        })).collect::<Vec<_>>(),
        "declaration": {
            "direction": declaration.direction,
            "tools": declaration.tools,
        },
    })
}

/// Wrap a failed step. The HTTP status is kept, so a 426 still updates the
/// runner and runs the command again.
pub fn not_delivered(step: &str, error: Error) -> Error {
    let status = error.http_status();
    let message = if is_entries_closed(&error) {
        format!(
            "Not delivered: this challenge has closed to entries, so the platform refused {step} \
             (HTTP {}). Nothing you sent counts as your entry",
            status.unwrap_or(409)
        )
    } else {
        format!(
            "Not delivered: {step} failed: {error}. Nothing was confirmed, so your entry is as it \
             was; run `daycare-runner challenge submit` again to send a new version"
        )
    };
    let wrapped = if error.is_transport() {
        Error::transport(message)
    } else {
        Error::new(message)
    };
    match status {
        Some(code) => wrapped.with_status(code),
        None => wrapped,
    }
}

/// The Content-Type a file is uploaded with. Storage keeps it; the platform
/// renders by extension, not by this.
pub fn content_type(path: &str) -> &'static str {
    let extension = path
        .rsplit_once('.')
        .map(|(_, ext)| ext.to_ascii_lowercase())
        .unwrap_or_default();
    match extension.as_str() {
        "html" | "htm" => "text/html; charset=utf-8",
        "csv" => "text/csv; charset=utf-8",
        "md" | "markdown" => "text/markdown; charset=utf-8",
        "txt" => "text/plain; charset=utf-8",
        "json" => "application/json",
        "css" => "text/css; charset=utf-8",
        "js" => "text/javascript; charset=utf-8",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "pdf" => "application/pdf",
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        _ => "application/octet-stream",
    }
}

fn text(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .filter(|text| !text.is_empty())
}

/// Check the platform's answer to the POST against the files the owner said
/// yes to: one upload per file, no more, no fewer. Returns url by path.
fn upload_urls(response: &Value, files: &[OutputFile]) -> Result<BTreeMap<String, String>> {
    let uploads = response
        .get("uploads")
        .and_then(Value::as_array)
        .ok_or_else(|| Error::new("the platform's answer had no upload list"))?;
    let mut urls = BTreeMap::new();
    for upload in uploads {
        let path = text(upload, "path")
            .ok_or_else(|| Error::new("an upload in the platform's answer had no path"))?;
        let url = text(upload, "signed_url")
            .or_else(|| text(upload, "url"))
            .ok_or_else(|| Error::new(format!("the upload for {path} had no link")))?;
        if urls.insert(path.clone(), url).is_some() {
            return Err(Error::new(format!(
                "the platform's upload list names {path} twice"
            )));
        }
    }
    let wanted: Vec<&str> = files.iter().map(|file| file.path.as_str()).collect();
    let given: Vec<&str> = urls.keys().map(String::as_str).collect();
    let mut sorted = wanted.clone();
    sorted.sort_unstable_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
    let mut given_sorted = given.clone();
    given_sorted.sort_unstable_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
    if sorted != given_sorted {
        return Err(Error::new(
            "the platform's upload list does not match the files you said yes to",
        ));
    }
    Ok(urls)
}

/// Everything `deliver` needs.
pub struct Delivery<'a> {
    pub platform: &'a PlatformClient,
    pub uploader: &'a SignedUploader,
    pub token: &'a str,
    pub challenge_id: &'a str,
    /// The entry this folder was started for, from `ENTRY.json`.
    pub expected_entry_id: Option<&'a str>,
    pub files: &'a [OutputFile],
    pub declaration: &'a Declaration,
}

/// POST, one PUT per file, confirm. `progress` hears one line per step.
pub fn deliver(delivery: Delivery<'_>, progress: &mut dyn FnMut(&str)) -> Result<Delivered> {
    let Delivery {
        platform,
        uploader,
        token,
        challenge_id,
        expected_entry_id,
        files,
        declaration,
    } = delivery;
    let manifest_sha256 = manifest_sha256(files);

    let started = platform
        .submit_entry(token, challenge_id, &submit_body(files, declaration))
        .map_err(|error| not_delivered("starting the upload", error))?;
    let submission_id = text(&started, "submission_id").ok_or_else(|| {
        not_delivered(
            "starting the upload",
            Error::new("the platform's answer had no submission id"),
        )
    })?;
    // The id goes into a URL path.
    if crate::paths::sanitize_segment(&submission_id) != submission_id {
        return Err(not_delivered(
            "starting the upload",
            Error::new("the platform returned a submission id this runner cannot use"),
        ));
    }
    let version = started.get("version").and_then(Value::as_u64).unwrap_or(0);
    let entry_id = text(&started, "entry_id");
    if let (Some(expected), Some(got)) = (expected_entry_id, entry_id.as_deref()) {
        if expected != got {
            return Err(not_delivered(
                "starting the upload",
                Error::new(format!(
                    "the platform put this upload in entry {got}, but this folder belongs to \
                     entry {expected}; check which identity you are submitting as (--identity)"
                )),
            ));
        }
    }
    let urls = upload_urls(&started, files)
        .map_err(|error| not_delivered("starting the upload", error))?;
    progress(&format!(
        "Started version {version}. Uploading {} file(s)...",
        files.len()
    ));

    for (index, file) in files.iter().enumerate() {
        let url = &urls[&file.path];
        uploader
            .put(url, &file.bytes, content_type(&file.path), &file.path)
            .map_err(|error| not_delivered(&format!("uploading {}", file.path), error))?;
        progress(&format!(
            "  uploaded {} ({}/{})",
            file.path,
            index + 1,
            files.len()
        ));
    }

    let confirmed = platform
        .confirm_submission(token, challenge_id, &submission_id)
        .map_err(|error| not_delivered("confirming the upload", error))?;
    if confirmed.get("ok").and_then(Value::as_bool) != Some(true) {
        return Err(not_delivered(
            "confirming the upload",
            Error::new("the platform did not say ok"),
        ));
    }
    Ok(Delivered {
        submission_id,
        version: confirmed
            .get("version")
            .and_then(Value::as_u64)
            .unwrap_or(version),
        entry_id: text(&confirmed, "entry_id").or(entry_id),
        manifest_sha256,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(path: &str, body: &str) -> OutputFile {
        OutputFile {
            path: path.into(),
            sha256: sha256_hex(body.as_bytes()),
            bytes: body.as_bytes().to_vec(),
        }
    }

    #[test]
    fn the_manifest_hash_matches_the_platforms_canonical_form() {
        // Computed with the platform's manifestSha256 for this exact list:
        // JSON.stringify([{path:"a.csv",bytes:2,sha256:"d"*64},{path:"z.md",bytes:3,sha256:"c"*64}]).
        let files = [
            OutputFile {
                path: "z.md".into(),
                bytes: vec![0; 3],
                sha256: "c".repeat(64),
            },
            OutputFile {
                path: "a.csv".into(),
                bytes: vec![0; 2],
                sha256: "d".repeat(64),
            },
        ];
        let canonical = format!(
            "[{{\"path\":\"a.csv\",\"bytes\":2,\"sha256\":\"{}\"}},{{\"path\":\"z.md\",\"bytes\":3,\"sha256\":\"{}\"}}]",
            "d".repeat(64),
            "c".repeat(64)
        );
        assert_eq!(manifest_sha256(&files), sha256_hex(canonical.as_bytes()));
    }

    #[test]
    fn the_body_carries_the_listing_and_the_declaration_and_no_agent() {
        let body = submit_body(
            &[file("entry.html", "<p>hi</p>")],
            &Declaration {
                direction: "light".into(),
                tools: vec!["Claude Code".into()],
            },
        );
        assert_eq!(body["files"][0]["path"], "entry.html");
        assert_eq!(body["files"][0]["bytes"], 9);
        assert_eq!(body["declaration"]["direction"], "light");
        assert!(body.get("actor_id").is_none());
    }

    #[test]
    fn the_upload_list_must_name_exactly_the_files() {
        let files = [file("a.md", "a"), file("b.md", "b")];
        let ok = json!({"uploads": [
            {"path": "b.md", "signed_url": "http://127.0.0.1/b"},
            {"path": "a.md", "signed_url": "http://127.0.0.1/a"},
        ]});
        assert_eq!(upload_urls(&ok, &files).unwrap().len(), 2);
        for bad in [
            json!({"uploads": [{"path": "a.md", "signed_url": "http://127.0.0.1/a"}]}),
            json!({"uploads": [
                {"path": "a.md", "signed_url": "http://127.0.0.1/a"},
                {"path": "a.md", "signed_url": "http://127.0.0.1/a"},
            ]}),
            json!({"uploads": [
                {"path": "a.md", "signed_url": "http://127.0.0.1/a"},
                {"path": "c.md", "signed_url": "http://127.0.0.1/c"},
            ]}),
            json!({}),
        ] {
            assert!(upload_urls(&bad, &files).is_err(), "{bad}");
        }
    }

    #[test]
    fn a_closed_challenge_reads_as_not_delivered_and_keeps_its_status() {
        let closed = Error::new(crate::platform::ENTRIES_CLOSED).with_status(410);
        let error = not_delivered("confirming the upload", closed);
        assert!(error.message().starts_with("Not delivered:"), "{error}");
        assert!(error.message().contains("closed"), "{error}");
        assert_eq!(error.http_status(), Some(410));
        // A 426 still reads as "update required" to main's retry.
        let stale = not_delivered(
            "starting the upload",
            Error::new("challenge submit failed: runner_update_required (HTTP 426)")
                .with_status(426),
        );
        assert!(crate::self_update::is_update_required(&stale));
    }
}
