//! Which OpenCode releases Daycare runs, for visits and challenges alike.
//!
//! The adapter and the challenge launch were written and checked against
//! OpenCode 1.18.33 (`tests/fixtures/opencode-1.18.33/`). OpenCode 2 is a
//! rewrite of the parts the seal depends on: it drops `--pure` and the
//! `OPENCODE_DISABLE_EXTERNAL_SKILLS`, `OPENCODE_DISABLE_SHARE`,
//! `OPENCODE_DISABLE_LSP_DOWNLOAD` and `OPENCODE_DISABLE_DEFAULT_PLUGINS`
//! switches, replaces `debug config` and `debug agent <name>` with differently
//! shaped commands, and routes most commands through a shared background
//! service. Until the seal is proven again on 2.x, Daycare refuses it before
//! anything starts, instead of failing every turn on `Unrecognized flag:
//! --pure`.

use crate::{Error, Result};

/// The oldest release the seal was checked against.
pub const MIN_VERSION: (u64, u64, u64) = (1, 18, 33);

/// The first major release Daycare does not run yet.
pub const UNSUPPORTED_MAJOR: u64 = 2;

/// What a person can do about an unsupported OpenCode.
pub const INSTALL_HINT: &str = "Install OpenCode 1 (for example `npm install -g opencode-ai@1`) \
     so that `opencode --version` shows 1.x, or use Claude Code or Codex";

/// `1.18.33`, `opencode 1.19.0-beta.2` and `opencode v2.3.2` all read as a
/// version; anything else is `None`.
pub fn parse_version(text: &str) -> Option<(u64, u64, u64)> {
    let token = text.split_whitespace().find_map(|word| {
        let word = word.strip_prefix(['v', 'V']).unwrap_or(word);
        word.starts_with(|c: char| c.is_ascii_digit())
            .then_some(word)
    })?;
    let core = token.split(['-', '+']).next()?;
    let mut parts = core.split('.').map(|part| part.parse::<u64>().ok());
    Some((
        parts.next()??,
        parts.next()??,
        parts.next().flatten().unwrap_or(0),
    ))
}

/// Refuse anything but OpenCode 1.18.33 or a later 1.x release, from the
/// output of `opencode --version`.
pub fn check_version_output(text: &str) -> Result<()> {
    let (major, minor, patch) = MIN_VERSION;
    let supported =
        format!("Daycare needs OpenCode {major}.{minor}.{patch} or a later 1.x release");
    match parse_version(text) {
        Some(version) if version.0 >= UNSUPPORTED_MAJOR => {
            let (a, b, c) = version;
            Err(Error::new(format!(
                "{supported}, and this is {a}.{b}.{c}. OpenCode {UNSUPPORTED_MAJOR} changed the \
                 settings Daycare uses to keep a session sealed, so it is not supported yet. \
                 {INSTALL_HINT}"
            )))
        }
        Some(version) if version >= MIN_VERSION => Ok(()),
        Some((a, b, c)) => Err(Error::new(format!(
            "{supported}, and this is {a}.{b}.{c}. {INSTALL_HINT}; `opencode upgrade` may install \
             OpenCode {UNSUPPORTED_MAJOR}, which Daycare does not run yet"
        ))),
        None => Err(Error::new(format!(
            "could not read OpenCode's version. {supported}. {INSTALL_HINT}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_parse_with_or_without_a_name_or_a_v() {
        assert_eq!(parse_version("1.18.33\n"), Some((1, 18, 33)));
        assert_eq!(parse_version("opencode 1.19.0-beta.2"), Some((1, 19, 0)));
        assert_eq!(parse_version("opencode v2.3.2\n"), Some((2, 3, 2)));
        assert_eq!(parse_version("v1.20"), Some((1, 20, 0)));
        assert_eq!(parse_version("nothing"), None);
        assert_eq!(parse_version(""), None);
    }

    #[test]
    fn only_1_18_33_through_the_last_1_x_runs() {
        for ok in ["1.18.33", "opencode 1.19.6", "1.99.0"] {
            check_version_output(ok).unwrap();
        }
        let old = check_version_output("1.18.32").unwrap_err().to_string();
        assert!(old.contains("1.18.33 or a later 1.x release"), "{old}");
        assert!(old.contains("this is 1.18.32"), "{old}");
        for two in ["opencode v2.3.2", "2.0.0", "3.1.0"] {
            let error = check_version_output(two).unwrap_err().to_string();
            assert!(error.contains("is not supported yet"), "{error}");
            assert!(error.contains("npm install -g opencode-ai@1"), "{error}");
        }
        let unreadable = check_version_output("opencode").unwrap_err().to_string();
        assert!(
            unreadable.contains("could not read OpenCode's version"),
            "{unreadable}"
        );
    }
}
