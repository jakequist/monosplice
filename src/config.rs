//! Port of the corresponding TypeScript module — see docs/rust-port.md.
//!
//! `monosplice.toml` discovery, load and validation. The TS original loaded a JavaScript
//! module through jiti; the Rust port reads one TOML file with serde, so "the config
//! computes its subrepos" is no longer a shape that exists. Everything else — defaults,
//! refusals, wording — ports 1:1 from `src/config.ts`.

use std::fmt;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::core::paths::normalize_subrepo_path;

pub const CONFIG_FILENAME: &str = "monosplice.toml";

/// Config files from the JavaScript era. Finding one of these with no `monosplice.toml`
/// beside it is not "no config": it is a repo that has not been migrated yet, and every
/// command would otherwise walk past it and report the wrong root.
pub const LEGACY_CONFIG_FILENAMES: [&str; 5] = [
    "monosplice.config.ts",
    "monosplice.config.mts",
    "monosplice.config.js",
    "monosplice.config.mjs",
    "monosplice.config.cjs",
];

/// A subrepo with every default filled in.
#[derive(Debug, Clone)]
pub struct ResolvedSubrepo {
    pub name: String,
    pub path: String,
    pub remote: String,
    pub upstream: Option<String>,
    pub branch: String,
    /// Equals `branch` unless the config says otherwise; only meaningful with `upstream`.
    pub push_branch: String,
    pub exclude: Vec<String>,
    /// Shell command; see the hooks section of docs/rust-port.md.
    pub rewrite_message: Option<String>,
    pub transform: Option<String>,
    pub scan: Option<String>,
    /// The monorepo's top-level `id`, the same on every subrepo of one config. Written after
    /// every `Monosplice-Source` this monorepo exports, so a standalone branch that another
    /// monorepo also publishes says whose claim is whose.
    pub monorepo_id: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Project {
    /// Directory containing monosplice.toml (treated as the monorepo root).
    pub root: PathBuf,
    pub config_path: PathBuf,
    pub subrepos: Vec<ResolvedSubrepo>,
    /// The config's top-level `id` (also on every entry as `monorepo_id`).
    pub id: Option<String>,
}

/// A config monosplice will not act on. The message is the whole user-facing text.
#[derive(Debug)]
pub struct ConfigError(pub String);

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ConfigError {}

impl ConfigError {
    /// `Invalid config at <path>:\n<detail>` — detail carries its own two-space indent, so a
    /// multi-issue report reads as a list under the header.
    fn at(config_path: &Path, detail: &str) -> Self {
        ConfigError(format!(
            "Invalid config at {}:\n{}",
            config_path.display(),
            detail
        ))
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
struct RawSubrepo {
    name: Option<String>,
    path: String,
    remote: String,
    upstream: Option<String>,
    branch: Option<String>,
    push_branch: Option<String>,
    exclude: Option<Vec<String>>,
    rewrite_message: Option<String>,
    transform: Option<String>,
    scan: Option<String>,
}

/// `subrepos` defaults to empty rather than being required: `init` writes a template whose
/// only entry is commented out, and an array-of-tables cannot be appended to a statically
/// defined `subrepos = []`. A file with no subrepos is a valid "nothing attached yet" state.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    id: Option<String>,
    #[serde(default)]
    subrepos: Vec<RawSubrepo>,
}

/// Longest `id` accepted: room for a UUID or a readable name, never a paragraph.
pub const MAX_ID_LEN: usize = 64;

/// Can this string be a monorepo `id`? It is written as a trailer value on public commits and
/// compared byte for byte, so it is one token of letters, digits, `.`, `_` and `-`.
pub fn is_valid_monorepo_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_ID_LEN
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// The `id` of a config's text, or `None` when it has none or cannot be read as TOML at all.
/// Lenient on purpose: this reads *past* versions of the file from history, which may predate
/// keys the current schema requires or forbids; only the `id` matters here.
pub fn monorepo_id_of(raw: &str) -> Option<String> {
    let value: toml::Value = toml::from_str(raw).ok()?;
    value.get("id")?.as_str().map(str::to_string)
}

fn indent(text: &str) -> String {
    text.trim_end()
        .lines()
        .map(|line| format!("  {line}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Walk up from `start_dir` looking for `monosplice.toml`. A directory holding a legacy
/// JavaScript config and no TOML stops the walk with migration guidance; a directory holding
/// both is mid-migration, and the TOML wins silently.
pub fn find_config(start_dir: &Path) -> Result<Option<PathBuf>, ConfigError> {
    let mut dir = if start_dir.is_absolute() {
        start_dir.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(start_dir)
    };
    loop {
        let candidate = dir.join(CONFIG_FILENAME);
        if candidate.is_file() {
            return Ok(Some(candidate));
        }
        if let Some(legacy) = LEGACY_CONFIG_FILENAMES
            .iter()
            .map(|name| dir.join(name))
            .find(|p| p.is_file())
        {
            return Err(ConfigError(format!(
                "Found {} but no {CONFIG_FILENAME} beside it.\nmonosplice is now configured by {CONFIG_FILENAME}, not by JavaScript or TypeScript config files. Nothing was changed. Translate that file into a {CONFIG_FILENAME} in {} — the migration section of docs/reference.md shows the key-by-key equivalent — then run the command again.",
                legacy.display(),
                dir.display(),
            )));
        }
        match dir.parent() {
            Some(parent) => dir = parent.to_path_buf(),
            None => return Ok(None),
        }
    }
}

/// A config describes the git repository whose top level holds it, and no other. Refuses a
/// config found anywhere else: below the top level it belongs to a monorepo vendored inside
/// this one; above it, to some other tree. Either way every git command would run against
/// `git_toplevel` with the wrong subrepos — the nested case publishes this repository's
/// commits straight to the vendored monorepo's remotes.
///
/// Paths are compared canonicalized, so a symlinked cwd reaches the same decision as the real
/// path.
pub fn check_config_placement(config_path: &Path, git_toplevel: &Path) -> Result<(), ConfigError> {
    let config_dir = config_path.parent().unwrap_or(Path::new("."));
    let canonical = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    let (dir, top) = (canonical(config_dir), canonical(git_toplevel));
    if dir == top {
        return Ok(());
    }

    let top_config = git_toplevel.join(CONFIG_FILENAME);
    if dir.starts_with(&top) {
        let next = if top_config.is_file() {
            format!(
                "The config for this repository is {}. Run the command from {} to use it, or from a clone of the vendored monorepo to work on that one.",
                top_config.display(),
                git_toplevel.display(),
            )
        } else {
            format!(
                "{} has no {CONFIG_FILENAME} of its own. To work on the vendored monorepo, run the command from a clone of it; to configure this repository, run `monosplice init` in {}.",
                git_toplevel.display(),
                git_toplevel.display(),
            )
        };
        return Err(ConfigError(format!(
            "Skipped {}: {} is not the top of the git repository you are in ({}).\nThat file describes a different monorepo, typically one vendored inside this one; using it here would run its subrepos against {} instead. Nothing was changed.\n{next}",
            config_path.display(),
            config_dir.display(),
            git_toplevel.display(),
            git_toplevel.display(),
        )));
    }

    Err(ConfigError(format!(
        "Skipped {}: it is outside the git repository you are in ({}), so it does not describe this repository. Nothing was changed.\n{} has no {CONFIG_FILENAME} of its own. Run the command from the repository that file belongs to, or run `monosplice init` in {} to configure this one.",
        config_path.display(),
        git_toplevel.display(),
        git_toplevel.display(),
        git_toplevel.display(),
    )))
}

/// Validate raw TOML text. Exported for unit tests and for the attach/detach reload check.
pub fn resolve_config(raw: &str, config_path: &Path) -> Result<Vec<ResolvedSubrepo>, ConfigError> {
    let parsed: RawConfig = match toml::from_str(raw) {
        Ok(parsed) => parsed,
        Err(err) => return Err(ConfigError::at(config_path, &indent(&err.to_string()))),
    };

    // Empty strings are a config typo, not a value; serde cannot see them, so they are
    // collected here and reported together the way zod reported its issue list.
    let mut issues: Vec<String> = Vec::new();
    if let Some(id) = &parsed.id {
        if !is_valid_monorepo_id(id) {
            issues.push(format!(
                "  id — {id:?} is not a valid monorepo id: use 1 to {MAX_ID_LEN} letters, digits, `.`, `_` or `-` (it is written on every exported commit as `Monosplice-Monorepo: <id>`)"
            ));
        }
    }
    for (idx, s) in parsed.subrepos.iter().enumerate() {
        let mut require = |field: &str, value: &str| {
            if value.is_empty() {
                issues.push(format!(
                    "  subrepos[{idx}].{field} — {field} may not be empty"
                ));
            }
        };
        if let Some(name) = &s.name {
            require("name", name);
        }
        require("path", &s.path);
        require("remote", &s.remote);
        if let Some(upstream) = &s.upstream {
            require("upstream", upstream);
        }
        if let Some(branch) = &s.branch {
            require("branch", branch);
        }
        if let Some(push_branch) = &s.push_branch {
            require("push-branch", push_branch);
        }
        for (i, pattern) in s.exclude.iter().flatten().enumerate() {
            if pattern.is_empty() {
                issues.push(format!(
                    "  subrepos[{idx}].exclude[{i}] — exclude patterns may not be empty"
                ));
            }
        }
    }
    if !issues.is_empty() {
        return Err(ConfigError::at(config_path, &issues.join("\n")));
    }

    let mut resolved: Vec<ResolvedSubrepo> = Vec::with_capacity(parsed.subrepos.len());
    for (idx, s) in parsed.subrepos.into_iter().enumerate() {
        let norm_path = match normalize_subrepo_path(&s.path) {
            Ok(p) => p,
            Err(message) => {
                return Err(ConfigError::at(
                    config_path,
                    &format!("  subrepos[{idx}].path — {message}"),
                ))
            }
        };
        if s.push_branch.is_some() && s.upstream.is_none() {
            return Err(ConfigError::at(
                config_path,
                &format!("  subrepos[{idx}].push-branch — push-branch requires upstream: it names the branch monosplice pushes on your fork, and without `upstream` there is no fork/upstream split. Drop push-branch, or set `upstream` to the repository you pull from."),
            ));
        }
        if s.upstream.as_deref() == Some(s.remote.as_str()) {
            return Err(ConfigError::at(
                config_path,
                &format!("  subrepos[{idx}].upstream — upstream and remote are the same repository ({}), so there is no triangle. Drop `upstream`, or point it at the repository you pull from.", s.remote),
            ));
        }
        let branch = s.branch.unwrap_or_else(|| "main".to_string());
        let name = s.name.unwrap_or_else(|| basename(&norm_path).to_string());
        resolved.push(ResolvedSubrepo {
            name,
            path: norm_path,
            remote: s.remote,
            upstream: s.upstream,
            push_branch: s.push_branch.unwrap_or_else(|| branch.clone()),
            branch,
            exclude: s.exclude.unwrap_or_default(),
            rewrite_message: s.rewrite_message,
            transform: s.transform,
            scan: s.scan,
            monorepo_id: parsed.id.clone(),
        });
    }

    let mut seen_names: Vec<&str> = Vec::new();
    let mut seen_paths: Vec<&str> = Vec::new();
    for s in &resolved {
        if seen_names.contains(&s.name.as_str()) {
            return Err(ConfigError::at(
                config_path,
                &format!("  duplicate subrepo name: {}", s.name),
            ));
        }
        if seen_paths.contains(&s.path.as_str()) {
            return Err(ConfigError::at(
                config_path,
                &format!("  duplicate subrepo path: {}", s.path),
            ));
        }
        for other in &seen_paths {
            if s.path.starts_with(&format!("{other}/"))
                || other.starts_with(&format!("{}/", s.path))
            {
                return Err(ConfigError::at(
                    config_path,
                    &format!("  subrepo paths may not nest: {other} vs {}", s.path),
                ));
            }
        }
        seen_names.push(&s.name);
        seen_paths.push(&s.path);
    }
    Ok(resolved)
}

/// Last segment of a normalized (always `/`-separated, never empty) subrepo path.
fn basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// Load and validate the project config starting from `start_dir` (cwd).
pub fn load_project(start_dir: &Path) -> Result<Option<Project>, ConfigError> {
    let Some(config_path) = find_config(start_dir)? else {
        return Ok(None);
    };
    let raw = match std::fs::read_to_string(&config_path) {
        Ok(raw) => raw,
        Err(err) => {
            return Err(ConfigError::at(
                &config_path,
                &format!("  failed to read: {err}"),
            ))
        }
    };
    let subrepos = resolve_config(&raw, &config_path)?;
    let id = monorepo_id_of(&raw);
    Ok(Some(Project {
        root: config_path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from(".")),
        config_path,
        subrepos,
        id,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    const CONFIG_PATH: &str = "/repo/monosplice.toml";

    fn resolve(raw: &str) -> Result<Vec<ResolvedSubrepo>, ConfigError> {
        resolve_config(raw, Path::new(CONFIG_PATH))
    }

    fn err(raw: &str) -> String {
        resolve(raw).expect_err("expected a config error").0
    }

    /// A throwaway directory that removes itself when the test ends.
    pub(crate) struct TempDir(pub PathBuf);

    impl TempDir {
        pub(crate) fn new(tag: &str) -> Self {
            let unique = format!(
                "monosplice-{tag}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            );
            let dir = std::env::temp_dir().join(unique.replace(['(', ')', ' '], ""));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).expect("create temp dir");
            TempDir(dir)
        }

        pub(crate) fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn find_config_finds_the_toml_and_walks_up() {
        let dir = TempDir::new("find-walk");
        let nested = dir.path().join("a/b");
        fs::create_dir_all(&nested).unwrap();
        let config = dir.path().join(CONFIG_FILENAME);
        fs::write(&config, "").unwrap();

        assert_eq!(find_config(dir.path()).unwrap(), Some(config.clone()));
        assert_eq!(find_config(&nested).unwrap(), Some(config));
    }

    #[test]
    fn find_config_returns_none_when_there_is_nothing_to_find() {
        let dir = TempDir::new("find-none");
        // A temp dir with no config anywhere above it in this tree; the walk ends at /.
        assert_eq!(find_config(dir.path()).unwrap(), None);
    }

    // S165: the JS config extensions are all gone; finding one is a migration error.
    #[test]
    fn find_config_reports_every_legacy_extension() {
        for name in LEGACY_CONFIG_FILENAMES {
            let dir = TempDir::new(&format!("legacy-{name}"));
            fs::write(dir.path().join(name), "export default {subrepos: []}\n").unwrap();

            let message = find_config(dir.path())
                .expect_err("expected migration error")
                .0;
            assert!(message.contains(name), "names the legacy file: {message}");
            assert!(message.contains("monosplice.toml"), "{message}");
            assert!(message.contains("docs/reference.md"), "{message}");
        }
    }

    #[test]
    fn toml_wins_silently_over_a_legacy_config_in_the_same_directory() {
        let dir = TempDir::new("both");
        fs::write(
            dir.path().join("monosplice.config.ts"),
            "export default {}\n",
        )
        .unwrap();
        fs::write(dir.path().join(CONFIG_FILENAME), "").unwrap();

        assert_eq!(
            find_config(dir.path()).unwrap(),
            Some(dir.path().join(CONFIG_FILENAME))
        );
    }

    // S86: a config only describes the git repository whose top level holds it.
    #[test]
    fn config_placement_accepts_a_config_at_the_git_top_level() {
        let dir = TempDir::new("placement-top");
        let config = dir.path().join(CONFIG_FILENAME);
        fs::write(&config, "").unwrap();
        assert!(check_config_placement(&config, dir.path()).is_ok());
    }

    #[test]
    fn config_placement_refuses_a_nested_config_and_names_the_one_that_applies() {
        let dir = TempDir::new("placement-nested");
        let nested = dir.path().join("vendor/middle");
        fs::create_dir_all(&nested).unwrap();
        let skipped = nested.join(CONFIG_FILENAME);
        fs::write(&skipped, "").unwrap();

        let without = check_config_placement(&skipped, dir.path())
            .expect_err("nested config refused")
            .0;
        assert!(
            without.contains(&skipped.display().to_string()),
            "{without}"
        );
        assert!(
            without.contains("is not the top of the git repository"),
            "{without}"
        );
        assert!(
            without.contains(&format!(
                "{} has no {CONFIG_FILENAME}",
                dir.path().display()
            )),
            "{without}"
        );

        let applies = dir.path().join(CONFIG_FILENAME);
        fs::write(&applies, "").unwrap();
        let with = check_config_placement(&skipped, dir.path())
            .expect_err("still refused when the top level has a config")
            .0;
        assert!(
            with.contains(&format!(
                "The config for this repository is {}",
                applies.display()
            )),
            "{with}"
        );
    }

    #[test]
    fn config_placement_refuses_a_config_above_the_git_top_level() {
        let dir = TempDir::new("placement-above");
        let repo = dir.path().join("repo");
        fs::create_dir_all(&repo).unwrap();
        let above = dir.path().join(CONFIG_FILENAME);
        fs::write(&above, "").unwrap();

        let message = check_config_placement(&above, &repo)
            .expect_err("config above the top level refused")
            .0;
        assert!(message.contains(&above.display().to_string()), "{message}");
        assert!(message.contains("outside the git repository"), "{message}");
    }

    #[cfg(unix)]
    #[test]
    fn config_placement_sees_through_symlinks() {
        let dir = TempDir::new("placement-symlink");
        let real = dir.path().join("real");
        fs::create_dir_all(real.join("sub")).unwrap();
        fs::write(real.join(CONFIG_FILENAME), "").unwrap();
        fs::write(real.join("sub").join(CONFIG_FILENAME), "").unwrap();
        let alias = dir.path().join("alias");
        std::os::unix::fs::symlink(&real, &alias).unwrap();

        assert!(check_config_placement(&alias.join(CONFIG_FILENAME), &real).is_ok());
        assert!(check_config_placement(&real.join(CONFIG_FILENAME), &alias).is_ok());
        assert!(check_config_placement(&alias.join("sub").join(CONFIG_FILENAME), &real).is_err());
    }

    #[test]
    fn load_project_reads_root_and_subrepos() {
        let dir = TempDir::new("load");
        fs::write(
            dir.path().join(CONFIG_FILENAME),
            "[[subrepos]]\npath = \"core\"\nremote = \"r\"\n",
        )
        .unwrap();

        let project = load_project(dir.path()).unwrap().expect("a project");
        assert_eq!(project.root, dir.path());
        assert_eq!(project.config_path, dir.path().join(CONFIG_FILENAME));
        assert_eq!(project.subrepos.len(), 1);
        assert_eq!(project.subrepos[0].name, "core");
    }

    #[test]
    fn applies_defaults_name_from_path_basename_and_branch_main() {
        let subrepos =
            resolve("[[subrepos]]\npath = \"packages/taka-core\"\nremote = \"git@github.com:me/taka-core.git\"\n")
                .unwrap();
        let s = &subrepos[0];
        assert_eq!(s.name, "taka-core");
        assert_eq!(s.path, "packages/taka-core");
        assert_eq!(s.branch, "main");
        assert_eq!(s.push_branch, "main");
        assert_eq!(s.upstream, None);
        assert!(s.exclude.is_empty());
    }

    #[test]
    fn an_empty_config_resolves_to_no_subrepos() {
        assert!(resolve("").unwrap().is_empty());
        assert!(resolve("# nothing attached yet\n").unwrap().is_empty());
    }

    #[test]
    fn normalizes_the_path_and_keeps_hook_commands() {
        let subrepos = resolve(
            "[[subrepos]]\npath = \"./core/\"\nremote = \"r\"\nexclude = [\"**/*.secret\"]\nrewrite-message = \"cat\"\ntransform = \"true\"\nscan = \"grep -q x\"\n",
        )
        .unwrap();
        let s = &subrepos[0];
        assert_eq!(s.path, "core");
        assert_eq!(s.exclude, vec!["**/*.secret".to_string()]);
        assert_eq!(s.rewrite_message.as_deref(), Some("cat"));
        assert_eq!(s.transform.as_deref(), Some("true"));
        assert_eq!(s.scan.as_deref(), Some("grep -q x"));
    }

    #[test]
    fn names_the_offending_field_on_validation_errors() {
        let missing_remote = err("[[subrepos]]\npath = \"core\"\n");
        assert!(missing_remote.starts_with(&format!("Invalid config at {CONFIG_PATH}:")));
        assert!(missing_remote.contains("remote"), "{missing_remote}");

        let empty_path = err("[[subrepos]]\npath = \"\"\nremote = \"x\"\n");
        assert!(
            empty_path.contains("subrepos[0].path — path may not be empty"),
            "{empty_path}"
        );

        let escaping = err("[[subrepos]]\npath = \"../outside\"\nremote = \"x\"\n");
        assert!(escaping.contains("subrepos[0].path"), "{escaping}");
        assert!(escaping.contains("'..'"), "{escaping}");
    }

    #[test]
    fn rejects_unknown_keys_and_syntax_errors_under_the_same_header() {
        let typo = err("[[subrepos]]\npath = \"core\"\nremote = \"r\"\nbrunch = \"main\"\n");
        assert!(typo.starts_with(&format!("Invalid config at {CONFIG_PATH}:")));
        assert!(typo.contains("unknown field"), "{typo}");
        assert!(typo.contains("brunch"), "{typo}");

        // A camelCase leftover from the JS config is an unknown key, not a silent no-op.
        let camel = err("[[subrepos]]\npath = \"core\"\nremote = \"r\"\npushBranch = \"p\"\n");
        assert!(camel.contains("unknown field"), "{camel}");

        let syntax = err("[[subrepos]\npath = \"core\"\n");
        assert!(syntax.starts_with(&format!("Invalid config at {CONFIG_PATH}:")));
        assert!(syntax.contains("TOML parse error"), "{syntax}");

        let top_level = err("subrepo = []\n");
        assert!(top_level.contains("unknown field"), "{top_level}");
    }

    #[test]
    fn rejects_duplicates_and_nested_subrepo_paths() {
        let dup_path =
            err("[[subrepos]]\npath = \"core\"\nremote = \"a\"\n\n[[subrepos]]\npath = \"core\"\nremote = \"b\"\n");
        assert!(dup_path.contains("duplicate"), "{dup_path}");

        let dup_name = err(
            "[[subrepos]]\nname = \"core\"\npath = \"a\"\nremote = \"a\"\n\n[[subrepos]]\nname = \"core\"\npath = \"b\"\nremote = \"b\"\n",
        );
        assert!(
            dup_name.contains("duplicate subrepo name: core"),
            "{dup_name}"
        );

        let nested =
            err("[[subrepos]]\npath = \"core\"\nremote = \"a\"\n\n[[subrepos]]\npath = \"core/sub\"\nremote = \"b\"\n");
        assert!(nested.contains("nest"), "{nested}");

        // A shared prefix is not nesting.
        assert_eq!(
            resolve("[[subrepos]]\npath = \"core\"\nremote = \"a\"\n\n[[subrepos]]\npath = \"core-tools\"\nremote = \"b\"\n")
                .unwrap()
                .len(),
            2
        );
    }

    // --- triangular config validation (port of test/unit/triangular.test.ts) ---

    #[test]
    fn defaults_push_branch_to_branch_and_leaves_upstream_unset() {
        let plain = resolve("[[subrepos]]\npath = \"core\"\nremote = \"fork\"\n").unwrap();
        assert_eq!(plain[0].branch, "main");
        assert_eq!(plain[0].push_branch, "main");
        assert_eq!(plain[0].upstream, None);

        let tri = resolve(
            "[[subrepos]]\npath = \"vendor/lodash\"\nremote = \"fork\"\nupstream = \"upstream\"\nbranch = \"4.x\"\n",
        )
        .unwrap();
        assert_eq!(tri[0].upstream.as_deref(), Some("upstream"));
        assert_eq!(tri[0].branch, "4.x");
        assert_eq!(tri[0].push_branch, "4.x");
    }

    #[test]
    fn keeps_an_explicit_push_branch() {
        let s = resolve(
            "[[subrepos]]\npath = \"vendor/lodash\"\nremote = \"fork\"\nupstream = \"up\"\npush-branch = \"patches\"\n",
        )
        .unwrap();
        assert_eq!(s[0].branch, "main");
        assert_eq!(s[0].push_branch, "patches");
    }

    #[test]
    fn rejects_push_branch_without_upstream() {
        let message =
            err("[[subrepos]]\npath = \"core\"\nremote = \"fork\"\npush-branch = \"patches\"\n");
        assert!(
            message.contains("push-branch requires upstream"),
            "{message}"
        );
        assert!(message.contains("subrepos[0].push-branch"), "{message}");
    }

    #[test]
    fn rejects_an_upstream_equal_to_remote() {
        let message =
            err("[[subrepos]]\npath = \"core\"\nremote = \"same\"\nupstream = \"same\"\n");
        assert!(message.contains("subrepos[0].upstream"), "{message}");
        assert!(message.contains("there is no triangle"), "{message}");
    }

    #[test]
    fn rejects_an_empty_upstream() {
        let message = err("[[subrepos]]\npath = \"core\"\nremote = \"fork\"\nupstream = \"\"\n");
        assert!(message.contains("upstream"), "{message}");
    }

    #[test]
    fn an_id_is_carried_on_every_entry() {
        let s = resolve_config(
            "id = \"mono-1.a_b\"\n\n[[subrepos]]\npath = \"core\"\nremote = \"r\"\n\n[[subrepos]]\npath = \"lib\"\nremote = \"l\"\n",
            Path::new(CONFIG_PATH),
        )
        .unwrap();
        assert!(s
            .iter()
            .all(|e| e.monorepo_id.as_deref() == Some("mono-1.a_b")));
        let none = resolve_config(
            "[[subrepos]]\npath = \"core\"\nremote = \"r\"\n",
            Path::new(CONFIG_PATH),
        )
        .unwrap();
        assert_eq!(none[0].monorepo_id, None);
    }

    #[test]
    fn rejects_an_id_that_cannot_be_one_trailer_value() {
        for bad in ["", "two words", "new\\nline", "semi;colon"] {
            let message = err(&format!("id = \"{bad}\"\n"));
            assert!(message.contains("  id — "), "{bad}: {message}");
        }
        let long = "x".repeat(MAX_ID_LEN + 1);
        assert!(err(&format!("id = \"{long}\"\n")).contains("  id — "));
    }

    #[test]
    fn an_id_is_read_leniently_from_any_version_of_the_file() {
        assert_eq!(
            monorepo_id_of("id = \"x\"\nunknown = 1\n").as_deref(),
            Some("x")
        );
        assert_eq!(monorepo_id_of("[[subrepos]]\npath = \"a\"\n"), None);
        assert_eq!(monorepo_id_of("not toml ["), None);
    }
}
