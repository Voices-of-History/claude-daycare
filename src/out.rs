//! Output discipline in one place, shared by `main` and the library's command
//! modules (`challenge`, `approval`). Human text stays the default so the CLI
//! is usable directly; `--json` makes every command a stable API.

use serde_json::Value;

#[derive(Clone, Copy, Debug)]
pub struct Out {
    pub json: bool,
    /// Set for the turns inside a visit. A visit is one command and therefore
    /// one result: a caller parsing stdout must not receive a JSON object per
    /// turn ahead of the one it asked for.
    pub quiet: bool,
}

impl Out {
    pub fn new(json: bool) -> Out {
        Out { json, quiet: false }
    }

    pub fn emit(self, value: Value, human: impl FnOnce()) {
        if self.quiet {
            return;
        }
        if self.json {
            println!("{value}");
        } else {
            human();
        }
    }

    pub fn say(self, line: impl std::fmt::Display) {
        if !self.json && !self.quiet {
            println!("{line}");
        }
    }

    /// The same destination, but silent — used for work that happens inside a
    /// command rather than as its result.
    pub fn inner(self) -> Out {
        Out {
            json: self.json,
            quiet: true,
        }
    }
}
