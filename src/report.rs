//! Port of the corresponding TypeScript module — see docs/rust-port.md.
//!
//! The reporter half of `src/lib/base.ts` and `src/lib/ops.ts`: how a command stops, how a
//! multi-subrepo walk collects refusals instead of dying on the first one, and the two
//! project/subrepo lookups every command starts with.

use std::borrow::Borrow;
use std::path::Path;

use crate::config::{check_config_placement, load_project, Project, ResolvedSubrepo};
use crate::core::git::{git, git_ok};

/// A command refusing to continue. `message` is the whole user-facing text: `main` prints it
/// as `Error: <message>` on stderr, newlines and all, and exits with `exit_code`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    pub message: String,
    pub exit_code: i32,
}

impl Failure {
    /// `this.error(msg)` — exit 2, the oclif default every behavior test relies on.
    pub fn error(message: impl Into<String>) -> Self {
        Failure {
            message: message.into(),
            exit_code: 2,
        }
    }

    /// The places the TS passed `{exit: 1}`: `doctor` with problems, `status --check`, and the
    /// collected-failures report at the end of [`each_subrepo`].
    pub fn exit1(message: impl Into<String>) -> Self {
        Failure {
            message: message.into(),
            exit_code: 1,
        }
    }
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for Failure {}

/// A single subrepo refused to proceed. Every multi-subrepo command collects these so one
/// unpublished subrepo cannot stop the others (S90, S155) and reports them together at the
/// end; `halt` is the exception that stops the run where it stands — the conflict case, where
/// a sequencer now sits on disk and only one may exist.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubrepoFailure {
    pub message: String,
    pub halt: bool,
}

impl SubrepoFailure {
    pub fn new(message: impl Into<String>) -> Self {
        SubrepoFailure {
            message: message.into(),
            halt: false,
        }
    }

    /// This failure cannot be walked past: stop the run where it stands.
    pub fn halting(message: impl Into<String>) -> Self {
        SubrepoFailure {
            message: message.into(),
            halt: true,
        }
    }
}

impl std::fmt::Display for SubrepoFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

/// What every subrepo-selecting command says when the config has nothing to work on.
pub const NO_SUBREPOS_CONFIGURED: &str =
    "no subrepos configured — run `monosplice attach <folder> <git-url>` to connect one";

/// Non-fatal notice; goes to stderr so stdout stays pipeable.
pub fn warn(message: &str) {
    eprintln!("{message}");
}

/// Run `body` for each subrepo, collecting per-subrepo failures and reporting them together at
/// the end with exit 1. A failure marked `halt` stops the walk where it stands.
///
/// Generic over the element type so both `project.subrepos` (owned) and the borrowed slice
/// [`select_subrepos`] hands back can be walked without cloning.
pub fn each_subrepo<S, F>(subrepos: &[S], mut body: F) -> Result<(), Failure>
where
    S: Borrow<ResolvedSubrepo>,
    F: FnMut(&ResolvedSubrepo) -> Result<(), SubrepoFailure>,
{
    if subrepos.is_empty() {
        println!("{NO_SUBREPOS_CONFIGURED}");
        return Ok(());
    }

    let mut failures: Vec<String> = Vec::new();
    for subrepo in subrepos {
        if let Err(failure) = body(subrepo.borrow()) {
            let halt = failure.halt;
            failures.push(failure.message);
            if halt {
                break;
            }
        }
    }

    if failures.is_empty() {
        Ok(())
    } else {
        Err(Failure::exit1(failures.join("\n\n")))
    }
}

/// Load the config walking up from cwd; fails with a helpful error when absent or invalid.
pub fn require_project() -> Result<Project, Failure> {
    let cwd = std::env::current_dir()
        .map_err(|err| Failure::error(format!("Cannot read the current directory: {err}")))?;
    require_project_from(&cwd)
}

/// [`require_project`] from an explicit directory (the seam unit tests use).
pub fn require_project_from(start_dir: &Path) -> Result<Project, Failure> {
    let project = load_project(start_dir).map_err(|err| Failure::error(err.0))?;
    let Some(project) = project else {
        return Err(Failure::error(
            "No monosplice config found. Run this inside a repo containing monosplice.toml, or run `monosplice init` to create one.",
        ));
    };
    // No top level (cwd outside any work tree) leaves the not-a-repository check below to
    // speak, as it always has.
    if let Ok(toplevel) = git(start_dir, &["rev-parse", "--show-toplevel"]) {
        check_config_placement(&project.config_path, Path::new(&toplevel))
            .map_err(|err| Failure::error(err.0))?;
    }
    if !git_ok(&project.root, &["rev-parse", "--is-inside-work-tree"]) {
        return Err(Failure::error(format!(
            "{} is not a git repository.",
            project.root.display()
        )));
    }
    Ok(project)
}

/// Pick subrepos by optional name argument; fails if the name is unknown.
pub fn select_subrepos<'a>(
    project: &'a Project,
    name: Option<&str>,
) -> Result<Vec<&'a ResolvedSubrepo>, Failure> {
    let Some(name) = name else {
        return Ok(project.subrepos.iter().collect());
    };
    match project.subrepos.iter().find(|s| s.name == name) {
        Some(found) => Ok(vec![found]),
        None => Err(Failure::error(format!(
            "Unknown subrepo {}. Configured subrepos: {}",
            json_quote(name),
            configured_names(project)
        ))),
    }
}

/// `s.map(name).join(', ') || '(none)'` — the same fallback every "unknown subrepo" says.
pub fn configured_names(project: &Project) -> String {
    let names: Vec<&str> = project.subrepos.iter().map(|s| s.name.as_str()).collect();
    if names.is_empty() {
        "(none)".to_string()
    } else {
        names.join(", ")
    }
}

/// `JSON.stringify` of a string, so quoted names read exactly as they did in the TS.
pub fn json_quote(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for c in value.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn subrepo(name: &str) -> ResolvedSubrepo {
        ResolvedSubrepo {
            name: name.to_string(),
            path: name.to_string(),
            remote: format!("git@example.test:{name}.git"),
            upstream: None,
            branch: "main".to_string(),
            push_branch: "main".to_string(),
            exclude: Vec::new(),
            rewrite_message: None,
            transform: None,
            scan: None,
            monorepo_id: None,
        }
    }

    fn project(names: &[&str]) -> Project {
        Project {
            root: PathBuf::from("/repo"),
            config_path: PathBuf::from("/repo/monosplice.toml"),
            subrepos: names.iter().map(|n| subrepo(n)).collect(),
            id: None,
        }
    }

    #[test]
    fn exit_codes_match_the_error_contract() {
        assert_eq!(Failure::error("boom").exit_code, 2);
        assert_eq!(Failure::exit1("boom").exit_code, 1);
        assert_eq!(Failure::error("boom").message, "boom");
    }

    #[test]
    fn each_subrepo_runs_every_entry_when_nothing_fails() {
        let subrepos = vec![subrepo("a"), subrepo("b"), subrepo("c")];
        let mut seen: Vec<String> = Vec::new();
        let result = each_subrepo(&subrepos, |s| {
            seen.push(s.name.clone());
            Ok(())
        });
        assert!(result.is_ok());
        assert_eq!(seen, vec!["a", "b", "c"]);
    }

    #[test]
    fn each_subrepo_collects_failures_and_reports_them_together_with_exit_1() {
        let subrepos = vec![subrepo("a"), subrepo("b"), subrepo("c")];
        let mut seen: Vec<String> = Vec::new();
        let failure = each_subrepo(&subrepos, |s| {
            seen.push(s.name.clone());
            if s.name == "b" {
                return Err(SubrepoFailure::new("b: refused"));
            }
            if s.name == "c" {
                return Err(SubrepoFailure::new("c: refused"));
            }
            Ok(())
        })
        .expect_err("collected failures must fail the command");

        // One refusal never silences the rest of the walk (S90, S155).
        assert_eq!(seen, vec!["a", "b", "c"]);
        assert_eq!(failure.exit_code, 1);
        assert_eq!(failure.message, "b: refused\n\nc: refused");
    }

    #[test]
    fn each_subrepo_stops_the_walk_on_a_halting_failure() {
        let subrepos = vec![subrepo("a"), subrepo("b"), subrepo("c")];
        let mut seen: Vec<String> = Vec::new();
        let failure = each_subrepo(&subrepos, |s| {
            seen.push(s.name.clone());
            if s.name == "b" {
                return Err(SubrepoFailure::halting("b: conflict"));
            }
            Ok(())
        })
        .expect_err("a halting failure still fails the command");

        assert_eq!(seen, vec!["a", "b"], "the walk stops where it halted");
        assert_eq!(failure.exit_code, 1);
        assert_eq!(failure.message, "b: conflict");
    }

    #[test]
    fn each_subrepo_says_so_when_there_is_nothing_configured() {
        let empty: Vec<ResolvedSubrepo> = Vec::new();
        let mut ran = false;
        let result = each_subrepo(&empty, |_| {
            ran = true;
            Ok(())
        });
        assert!(result.is_ok());
        assert!(!ran, "an empty config runs no body");
    }

    #[test]
    fn each_subrepo_accepts_borrowed_selections() {
        let project = project(&["a", "b"]);
        let selected = select_subrepos(&project, Some("b")).unwrap();
        let mut seen: Vec<String> = Vec::new();
        each_subrepo(&selected, |s| {
            seen.push(s.name.clone());
            Ok(())
        })
        .unwrap();
        assert_eq!(seen, vec!["b"]);
    }

    #[test]
    fn select_subrepos_defaults_to_every_configured_subrepo() {
        let project = project(&["a", "b"]);
        let all = select_subrepos(&project, None).unwrap();
        assert_eq!(
            all.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
            vec!["a", "b"]
        );
    }

    #[test]
    fn select_subrepos_names_the_configured_ones_when_the_name_is_unknown() {
        let project = project(&["core", "lib"]);
        let failure = select_subrepos(&project, Some("nope")).expect_err("unknown name");
        assert_eq!(failure.exit_code, 2);
        assert_eq!(
            failure.message,
            "Unknown subrepo \"nope\". Configured subrepos: core, lib"
        );
    }

    #[test]
    fn select_subrepos_falls_back_to_none_with_an_empty_config() {
        let project = project(&[]);
        let failure = select_subrepos(&project, Some("core")).expect_err("unknown name");
        assert_eq!(
            failure.message,
            "Unknown subrepo \"core\". Configured subrepos: (none)"
        );
    }

    #[test]
    fn require_project_reports_a_missing_config_by_name() {
        let dir = std::env::temp_dir().join(format!(
            "monosplice-report-no-config-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let dir = PathBuf::from(dir.to_string_lossy().replace(['(', ')', ' '], ""));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let failure = require_project_from(&dir).expect_err("no config anywhere above a temp dir");
        let _ = std::fs::remove_dir_all(&dir);

        assert_eq!(failure.exit_code, 2);
        assert!(
            failure.message.contains("monosplice.toml"),
            "the message names the TOML config, not the JS one: {}",
            failure.message
        );
        assert!(
            failure.message.contains("monosplice init"),
            "{}",
            failure.message
        );
    }
}
