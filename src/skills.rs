//! The skills this binary carries and installs, in one table.
//!
//! Each skill is embedded with `include_str!` so it can never be a version
//! behind the CLI it calls. `skill install` writes every entry into every
//! agent's user skill library; self-update runs plain `skill install --json`,
//! so a new entry here reaches existing machines on their next update with no
//! other change.
//!
//! Ownership is decided by the frontmatter `name:` line: an edited copy of a
//! Daycare skill is still Daycare's and may be upgraded (with a `.bak`), while
//! a different skill living at the same path is never overwritten unasked.

use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BundledSkill {
    /// The frontmatter name, which is also the directory name.
    pub name: &'static str,
    pub markdown: &'static str,
}

/// The visit skill. Its name and paths predate the table and must not move:
/// installed copies, the README and the self-update refresh all point at them.
pub const DAYCARE: BundledSkill = BundledSkill {
    name: "daycare",
    markdown: include_str!("../skill/SKILL.md"),
};

/// The memory review an owner runs before a challenge (plan task N3).
pub const MEMORY_REVIEW: BundledSkill = BundledSkill {
    name: "daycare-memory-review",
    markdown: include_str!("../skill/daycare-memory-review/SKILL.md"),
};

pub const BUNDLED: [BundledSkill; 2] = [DAYCARE, MEMORY_REVIEW];

pub fn find(name: &str) -> Option<BundledSkill> {
    BUNDLED.iter().copied().find(|skill| skill.name == name)
}

/// Native user skill paths for one skill, verified against each agent's docs
/// (see README). All receive the same embedded source, including before an
/// agent is installed.
pub fn skill_paths(home: &Path, xdg_config_home: &Path, name: &str) -> Vec<PathBuf> {
    vec![
        home.join(".claude/skills").join(name).join("SKILL.md"),
        home.join(".agents/skills").join(name).join("SKILL.md"),
        xdg_config_home
            .join("opencode/skills")
            .join(name)
            .join("SKILL.md"),
    ]
}

/// The `name:` value in a skill file's frontmatter, if it has one.
pub fn frontmatter_name(contents: &str) -> Option<String> {
    frontmatter_value(contents, "name")
}

/// One `key: value` line from the leading `---` block. Quotes around the value
/// are dropped; nothing else is interpreted.
pub fn frontmatter_value(contents: &str, key: &str) -> Option<String> {
    let rest = contents.strip_prefix("---")?;
    let end = rest.find("\n---")?;
    rest[..end].lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        if name.trim() != key {
            return None;
        }
        let value = value.trim().trim_matches('"').trim_matches('\'').trim();
        (!value.is_empty()).then(|| value.to_string())
    })
}

/// Ours, by the frontmatter name the skill file carries. An edited copy is
/// still ours; a different skill that happens to live at the same path is
/// not, and is never overwritten unasked.
pub fn is_bundled(contents: &str, skill: &BundledSkill) -> bool {
    let Some(rest) = contents.strip_prefix("---") else {
        return false;
    };
    let Some(end) = rest.find("\n---") else {
        return false;
    };
    let wanted = format!("name: {}", skill.name);
    rest[..end].lines().any(|line| line.trim() == wanted)
}

/// Whether a skill file is one of Daycare's own, under any bundled name.
pub fn is_any_bundled(contents: &str) -> bool {
    BUNDLED.iter().any(|skill| is_bundled(contents, skill))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_bundled_skill_names_itself_in_its_own_frontmatter() {
        for skill in BUNDLED {
            assert!(is_bundled(skill.markdown, &skill), "{}", skill.name);
            assert_eq!(
                frontmatter_name(skill.markdown).as_deref(),
                Some(skill.name)
            );
            // A skill must not claim another bundled skill's name, or an
            // install would treat one as an edited copy of the other.
            for other in BUNDLED.iter().filter(|other| other.name != skill.name) {
                assert!(!is_bundled(skill.markdown, other));
            }
        }
    }

    #[test]
    fn ownership_is_by_exact_name_not_prefix() {
        let review = "---\nname: daycare-memory-review\n---\n";
        assert!(is_bundled(review, &MEMORY_REVIEW));
        assert!(!is_bundled(review, &DAYCARE));
        assert!(!is_bundled("---\nname: daycare-extra\n---\n", &DAYCARE));
        assert!(!is_bundled("name: daycare\n", &DAYCARE));
        assert!(is_any_bundled("---\nname: daycare\n---\nedited"));
        assert!(!is_any_bundled("---\nname: someone-elses\n---\n"));
    }

    #[test]
    fn frontmatter_values_are_read_without_quotes() {
        let text = "---\nname: \"my-skill\"\ndescription: Does a thing\n---\nbody";
        assert_eq!(frontmatter_name(text).as_deref(), Some("my-skill"));
        assert_eq!(
            frontmatter_value(text, "description").as_deref(),
            Some("Does a thing")
        );
        assert_eq!(frontmatter_name("no frontmatter"), None);
    }

    #[test]
    fn the_visit_skill_keeps_its_original_paths() {
        let paths = skill_paths(Path::new("/h"), Path::new("/h/.config"), DAYCARE.name);
        assert_eq!(
            paths,
            [
                PathBuf::from("/h/.claude/skills/daycare/SKILL.md"),
                PathBuf::from("/h/.agents/skills/daycare/SKILL.md"),
                PathBuf::from("/h/.config/opencode/skills/daycare/SKILL.md"),
            ]
        );
    }
}
