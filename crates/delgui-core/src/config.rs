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

/// View modes inherited when the corresponding command-line flag is omitted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InheritedViewModes {
    pub side_by_side: bool,
    pub line_numbers: bool,
    pub wrap: bool,
}

const BUILTIN_FEATURES: &[&str] = &[
    "color-only",
    "diff-highlight",
    "diff-so-fancy",
    "hyperlinks",
    "line-numbers",
    "navigate",
    "raw",
    "side-by-side",
];

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

    /// Resolve the view settings relevant to deciding whether a toolbar change
    /// needs `--no-gitconfig`. The selected list replaces `delta.features`:
    /// `[delta]` settings win, then later selected features and their children.
    pub fn inherited_view_modes<'a>(
        &'a self,
        selected: impl IntoIterator<Item = &'a str>,
    ) -> InheritedViewModes {
        let selected: Vec<_> = selected.into_iter().collect();
        let mut features = Vec::new();
        for name in selected.into_iter().rev() {
            self.collect_feature(name, &mut features);
        }
        self.collect_feature_flags(&self.settings, &mut features);

        let enabled = |key| {
            self.view_setting(key, &features)
                .and_then(parse_bool)
                .unwrap_or(false)
        };
        InheritedViewModes {
            side_by_side: enabled("side-by-side") && !enabled("color-only"),
            line_numbers: enabled("line-numbers"),
            wrap: self
                .view_setting("wrap-max-lines", &features)
                .and_then(|value| value.parse::<usize>().ok())
                != Some(0),
        }
    }

    fn view_setting<'a>(&'a self, key: &str, features: &[&str]) -> Option<&'a str> {
        self.settings.get(key).map(String::as_str).or_else(|| {
            features.iter().find_map(|name| {
                self.features
                    .get(*name)
                    .and_then(|settings| settings.get(key))
                    .map(String::as_str)
                    .or_else(|| {
                        matches!(
                            (key, *name),
                            ("side-by-side", "side-by-side") | ("line-numbers", "line-numbers")
                        )
                        .then_some("true")
                    })
            })
        })
    }

    fn collect_feature<'a>(&'a self, name: &'a str, features: &mut Vec<&'a str>) {
        if features.contains(&name) {
            return;
        }
        Self::collect_builtin(name, features);
        if let Some(settings) = self.features.get(name) {
            if let Some(children) = settings.get("features") {
                for child in children.split_whitespace().rev() {
                    self.collect_feature(child, features);
                }
            }
            self.collect_feature_flags(settings, features);
        }
    }

    fn collect_feature_flags(&self, settings: &Settings, features: &mut Vec<&str>) {
        for &name in BUILTIN_FEATURES {
            if settings.get(name).and_then(|value| parse_bool(value)) == Some(true) {
                Self::collect_builtin(name, features);
            }
        }
    }

    fn collect_builtin<'a>(name: &'a str, features: &mut Vec<&'a str>) {
        if !features.contains(&name) {
            features.push(name);
            if name == "side-by-side" {
                Self::collect_builtin("line-numbers", features);
            }
        }
    }
}

fn parse_bool(value: &str) -> Option<bool> {
    match value.to_ascii_lowercase().as_str() {
        "true" | "yes" | "on" => Some(true),
        "false" | "no" | "off" | "" => Some(false),
        value => value.parse::<i64>().ok().map(|number| number != 0),
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
            // Git distinguishes a valueless boolean (true) from an explicitly
            // empty value (false), which has a trailing space in this output.
            None => (entry, "true"),
        };
        let Some(rest) = key.strip_prefix("delta.") else {
            continue;
        };

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
    use super::{InheritedViewModes, parse_git_output};

    fn inherited(text: &str, selected: &[&str]) -> InheritedViewModes {
        parse_git_output(text).inherited_view_modes(selected.iter().copied())
    }

    #[test]
    fn themes_and_unselected_features_do_not_force_view_modes() {
        assert_eq!(
            inherited(
                "delta.syntax-theme GitHub\ndelta.features wide\ndelta.wide.side-by-side true\n",
                &[],
            ),
            InheritedViewModes {
                side_by_side: false,
                line_numbers: false,
                wrap: true,
            }
        );
    }

    #[test]
    fn selected_builtin_and_nested_features_enable_view_modes() {
        assert_eq!(
            inherited(
                "delta.review.features wide\ndelta.wide.side-by-side true\ndelta.wide.wrap-max-lines 0\n",
                &["review"],
            ),
            InheritedViewModes {
                side_by_side: true,
                line_numbers: true,
                wrap: false,
            }
        );
        assert!(inherited("", &["line-numbers"]).line_numbers);
        let builtin = inherited("", &["side-by-side"]);
        assert!(builtin.side_by_side && builtin.line_numbers);
    }

    #[test]
    fn main_settings_override_features_and_later_features_override_earlier_ones() {
        let config = "delta.a.side-by-side true\ndelta.b.side-by-side false\n";
        assert!(!inherited(config, &["a", "b"]).side_by_side);
        assert!(inherited(config, &["b", "a"]).side_by_side);
        let main_override = inherited(
            "delta.side-by-side false\ndelta.line-numbers false\ndelta.wrap-max-lines 3\n\
             delta.wide.side-by-side true\ndelta.wide.wrap-max-lines 0\n",
            &["wide"],
        );
        assert_eq!(
            main_override,
            InheritedViewModes {
                side_by_side: false,
                line_numbers: false,
                wrap: true,
            }
        );
    }

    #[test]
    fn parent_features_override_children_and_cycles_stop() {
        assert!(
            !inherited(
                "delta.parent.features child\ndelta.parent.side-by-side false\n\
             delta.child.features parent\ndelta.child.side-by-side true\n",
                &["parent"],
            )
            .side_by_side
        );
        // The builtin still activates line numbers even when its own
        // side-by-side default is overridden by the main section.
        let modes = inherited("delta.side-by-side false\n", &["side-by-side"]);
        assert!(!modes.side_by_side && modes.line_numbers);
    }

    #[test]
    fn git_boolean_spellings_and_empty_values_are_distinguished() {
        for value in ["true", "YES", "on", "1", "-2"] {
            assert!(inherited(&format!("delta.side-by-side {value}\n"), &[]).side_by_side);
        }
        for value in ["false", "NO", "off", "0", ""] {
            assert!(!inherited(&format!("delta.side-by-side {value}\n"), &[]).side_by_side);
        }
        assert!(inherited("delta.side-by-side\n", &[]).side_by_side);
    }

    #[test]
    fn separates_settings_from_named_features() {
        let cfg = parse_git_output(
            "file:/Users/x/.gitconfig\tdelta.features mypreset decorations\n\
             file:/Users/x/.gitconfig\tdelta.side-by-side true\n\
             file:/Users/x/.gitconfig\tdelta.mypreset.line-numbers true\n\
             file:/Users/x/.gitconfig\tdelta.mypreset.syntax-theme GitHub\n\
             file:/repo/.git/config\tdelta.decorations.hunk-header-style file line-number\n",
        );
        assert_eq!(
            cfg.settings.get("side-by-side").map(String::as_str),
            Some("true")
        );
        assert_eq!(cfg.active, vec!["mypreset", "decorations"]);
        assert_eq!(cfg.features["mypreset"]["syntax-theme"], "GitHub");
        assert_eq!(
            cfg.features["decorations"]["hunk-header-style"],
            "file line-number"
        );
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
        assert_eq!(
            cfg.selectable_features(),
            vec!["decorations", "line-numbers"]
        );
    }
}
