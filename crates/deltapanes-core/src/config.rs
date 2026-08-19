//! Discovering what the user's gitconfig already tells delta.
//!
//! delta reads `[delta]` from `$HOME/.gitconfig` and, when run inside a repo,
//! from that repo's `.git/config`; it also honours `DELTA_FEATURES`. It does
//! *not* honour `GIT_CONFIG_GLOBAL`. We ask `git` rather than parsing the files
//! ourselves so include directives, conditional includes and precedence all
//! behave the way delta will see them.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

pub type Settings = BTreeMap<String, String>;

#[derive(Debug, Default, Clone)]
pub struct DeltaConfig {
    /// Plain `[delta]` entries, keyed without the `delta.` prefix.
    pub settings: Settings,
    /// Named presets from `[delta "name"]` sections.
    pub features: BTreeMap<String, Settings>,
    /// Features delta will activate on its own, from `delta.features` and then
    /// `DELTA_FEATURES`.
    pub active: Vec<String>,
    /// Config files that contributed, for showing the user where this came from.
    pub sources: Vec<PathBuf>,
}

impl DeltaConfig {
    pub fn is_empty(&self) -> bool {
        self.settings.is_empty() && self.features.is_empty() && self.active.is_empty()
    }

    /// Feature names offered in the UI: those defined as sections, plus any
    /// named in `delta.features` that has no section of its own (delta ships
    /// several built-ins, such as `line-numbers` and `decorations`).
    pub fn selectable_features(&self) -> Vec<String> {
        let mut names: Vec<String> = self.features.keys().cloned().collect();
        for a in &self.active {
            if !names.contains(a) {
                names.push(a.clone());
            }
        }
        names.sort();
        names
    }
}

/// Ask git what delta would see. `cwd` decides whether a repo-local `[delta]`
/// section is included, which is why the caller has to choose it explicitly.
pub fn discover(cwd: Option<&Path>) -> DeltaConfig {
    let mut cmd = Command::new("git");
    cmd.args(["config", "--show-origin", "--get-regexp", r"^delta\."]);
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    let Ok(out) = cmd.output() else {
        return DeltaConfig::default();
    };
    let mut cfg = parse_git_output(&String::from_utf8_lossy(&out.stdout));

    // DELTA_FEATURES is read after delta.features and can add to it. A leading
    // '+' means "append to what gitconfig said"; without it, it replaces.
    if let Ok(env) = std::env::var("DELTA_FEATURES") {
        let env = env.trim();
        match env.strip_prefix('+') {
            Some(rest) => cfg.active.extend(rest.split_whitespace().map(String::from)),
            None if !env.is_empty() => {
                cfg.active = env.split_whitespace().map(String::from).collect();
            }
            None => {}
        }
    }
    cfg
}

fn parse_git_output(text: &str) -> DeltaConfig {
    let mut cfg = DeltaConfig::default();
    for line in text.lines() {
        // `file:/path/to/config\tdelta.key value`
        let (origin, entry) = match line.split_once('\t') {
            Some((o, e)) => (Some(o), e),
            None => (None, line),
        };
        if let Some(path) = origin.and_then(|o| o.strip_prefix("file:")) {
            let path = PathBuf::from(path);
            if !cfg.sources.contains(&path) {
                cfg.sources.push(path);
            }
        }
        let (key, value) = match entry.split_once(' ') {
            Some((k, v)) => (k, v.trim()),
            None => (entry, ""),
        };
        let Some(rest) = key.strip_prefix("delta.") else { continue };

        // `delta.side-by-side` is a setting; `delta.mypreset.line-numbers` is an
        // entry inside the named feature `mypreset`.
        match rest.split_once('.') {
            Some((feature, option)) => {
                cfg.features
                    .entry(feature.to_string())
                    .or_default()
                    .insert(option.to_string(), value.to_string());
            }
            None if rest == "features" => {
                cfg.active = value.split_whitespace().map(String::from).collect();
            }
            None => {
                cfg.settings.insert(rest.to_string(), value.to_string());
            }
        }
    }
    cfg
}

#[cfg(test)]
mod tests {
    use super::parse_git_output;

    #[test]
    fn separates_settings_from_named_features() {
        let cfg = parse_git_output(
            "file:/Users/x/.gitconfig\tdelta.features mypreset decorations\n\
             file:/Users/x/.gitconfig\tdelta.side-by-side true\n\
             file:/Users/x/.gitconfig\tdelta.mypreset.line-numbers true\n\
             file:/Users/x/.gitconfig\tdelta.mypreset.syntax-theme GitHub\n\
             file:/repo/.git/config\tdelta.decorations.hunk-header-style file line-number\n",
        );
        assert_eq!(cfg.settings.get("side-by-side").map(String::as_str), Some("true"));
        assert_eq!(cfg.active, vec!["mypreset", "decorations"]);
        assert_eq!(cfg.features["mypreset"]["syntax-theme"], "GitHub");
        assert_eq!(cfg.features["decorations"]["hunk-header-style"], "file line-number");
        assert_eq!(cfg.sources.len(), 2);
    }

    #[test]
    fn an_absent_delta_section_is_empty_not_an_error() {
        assert!(parse_git_output("").is_empty());
    }

    /// The common case on a machine where delta is installed but never tuned.
    #[test]
    fn selectable_features_include_builtins_named_without_a_section() {
        let cfg = parse_git_output("file:/x\tdelta.features line-numbers decorations\n");
        assert_eq!(cfg.selectable_features(), vec!["decorations", "line-numbers"]);
    }
}
