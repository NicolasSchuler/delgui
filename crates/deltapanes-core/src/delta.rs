//! Locating, probing and invoking the `delta` binary.

use std::io::Write;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Oldest delta we are willing to drive. 0.18 is where the flags this app
/// depends on (`--file-style=omit`, `--default-language`) are all present and
/// stable. Releases are infrequent -- 0.18.2 (Sep 2024) to 0.19.0 (Mar 2026) --
/// so pinning a floor costs little.
pub const MINIMUM_VERSION: (u32, u32) = (0, 18);

#[derive(Debug)]
pub enum DeltaError {
    /// `delta` is not on PATH. Per the non-goals, this is fatal and loud: we do
    /// not fall back to rendering diffs ourselves.
    NotFound,
    TooOld { found: String },
    UnreadableVersion { output: String },
    Io(std::io::Error),
}

impl std::fmt::Display for DeltaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound => write!(
                f,
                "`delta` was not found on PATH.\n\
                 deltapanes renders diffs with the real delta binary and does not\n\
                 substitute its own renderer.\n\n  brew install git-delta\n  cargo install git-delta"
            ),
            Self::TooOld { found } => write!(
                f,
                "found delta {found}, but deltapanes needs at least {}.{}",
                MINIMUM_VERSION.0, MINIMUM_VERSION.1
            ),
            Self::UnreadableVersion { output } => {
                write!(f, "could not parse `delta --version` output: {output:?}")
            }
            Self::Io(e) => write!(f, "failed to run delta: {e}"),
        }
    }
}

impl std::error::Error for DeltaError {}

#[derive(Clone, Debug)]
pub struct Delta {
    pub path: PathBuf,
    pub version: (u32, u32, u32),
    pub version_string: String,
}

impl Delta {
    /// Probe `delta --version` and check it against [`MINIMUM_VERSION`].
    pub fn discover() -> Result<Self, DeltaError> {
        Self::discover_at(Path::new("delta"))
    }

    pub fn discover_at(path: &Path) -> Result<Self, DeltaError> {
        let out = Command::new(path).arg("--version").output().map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                DeltaError::NotFound
            } else {
                DeltaError::Io(e)
            }
        })?;
        let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
        let version = parse_version(&text).ok_or_else(|| DeltaError::UnreadableVersion {
            output: text.clone(),
        })?;
        if (version.0, version.1) < MINIMUM_VERSION {
            return Err(DeltaError::TooOld { found: text });
        }
        Ok(Self {
            path: path.to_path_buf(),
            version,
            version_string: text,
        })
    }

    /// Syntax themes this delta build knows about, as `(name, is_dark)`.
    pub fn syntax_themes(&self) -> Vec<(String, bool)> {
        let Ok(out) = Command::new(&self.path).arg("--list-syntax-themes").output() else {
            return Vec::new();
        };
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter_map(|l| {
                let (kind, name) = l.split_once('\t')?;
                Some((name.trim().to_string(), kind.trim() == "dark"))
            })
            .filter(|(name, _)| !name.is_empty())
            .collect()
    }

    /// Render a comparison, returning delta's raw ANSI stdout.
    pub fn render(&self, left: &Input, right: &Input, opts: &Options) -> Result<Vec<u8>, DeltaError> {
        let mut cmd = Command::new(&self.path);
        cmd.args(opts.to_args());

        // delta shells out to `git diff --no-index` (falling back to `diff`) for
        // its two-file mode, so both must be reachable from the child.
        opts.apply_env(&mut cmd);

        let mut writers = Vec::new();
        let left_arg = left.as_argument(&mut writers).map_err(DeltaError::Io)?;
        let right_arg = right.as_argument(&mut writers).map_err(DeltaError::Io)?;
        cmd.arg(left_arg).arg(right_arg);

        let child = cmd
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| {
                if e.kind() == std::io::ErrorKind::NotFound {
                    DeltaError::NotFound
                } else {
                    DeltaError::Io(e)
                }
            })?;

        // Feed every in-memory panel concurrently: a pipe holds ~64 KiB, so
        // writing them in sequence would deadlock on anything larger.
        let pumps: Vec<_> = writers
            .into_iter()
            .map(|(mut w, buf)| std::thread::spawn(move || w.write_all(&buf)))
            .collect();

        let out = child.wait_with_output().map_err(DeltaError::Io)?;
        for p in pumps {
            // A closed pipe just means delta stopped reading early; that is not
            // an error we surface.
            let _ = p.join();
        }
        Ok(out.stdout)
    }
}

fn parse_version(text: &str) -> Option<(u32, u32, u32)> {
    let token = text.split_whitespace().find(|t| {
        t.split('.').count() >= 2 && t.chars().next().is_some_and(|c| c.is_ascii_digit())
    })?;
    let mut parts = token.split('.');
    Some((
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
        parts
            .next()
            .and_then(|p| p.trim_end_matches(|c: char| !c.is_ascii_digit()).parse().ok())
            .unwrap_or(0),
    ))
}

/// One side of a comparison.
pub enum Input {
    /// A real path. delta infers the syntax from its extension.
    Path(PathBuf),
    /// An in-memory buffer, handed to delta as `/dev/fd/N` -- exactly what the
    /// shell does for `delta <(pbpaste) file.rs`.
    Buffer(Vec<u8>),
}

type PendingWrite = (std::io::PipeWriter, Vec<u8>);

impl Input {
    fn as_argument(&self, writers: &mut Vec<PendingWrite>) -> std::io::Result<PathBuf> {
        match self {
            Self::Path(p) => Ok(p.clone()),
            Self::Buffer(bytes) => {
                let (reader, writer) = std::io::pipe()?;
                let fd = reader.as_raw_fd();
                // Rust marks its file descriptors close-on-exec; clear that so
                // the fd survives into delta at the same number we name here.
                if unsafe { libc::fcntl(fd, libc::F_SETFD, 0) } < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                // Deliberately leaked: the fd must stay open until the child has
                // exec'd. The process is short-lived per render, and each render
                // leaks at most two descriptors.
                std::mem::forget(reader);
                writers.push((writer, bytes.clone()));
                Ok(PathBuf::from(format!("/dev/fd/{fd}")))
            }
        }
    }
}

#[derive(Clone, Debug)]
pub struct Options {
    /// Terminal columns. delta's whole layout keys off this, so the GUI must
    /// translate its pixel width into a column count and re-render on resize.
    pub width: u16,
    pub side_by_side: bool,
    pub line_numbers: bool,
    pub wrap: bool,
    pub syntax_theme: Option<String>,
    /// Fallback syntax used when delta cannot infer one from a filename --
    /// which is the case for every pasted panel. Note delta infers from the
    /// *right-hand* path only, so a paste on the right silently loses
    /// highlighting without this.
    pub default_language: Option<String>,
    /// Named feature presets to activate, passed as `--features`.
    pub features: Vec<String>,
    /// Inherit `[delta]` from gitconfig and `DELTA_FEATURES`. When false we
    /// pass `--no-gitconfig`, which is the only way to get reproducible output
    /// for tests (`GIT_CONFIG_GLOBAL` is *not* honoured by delta).
    pub inherit_gitconfig: bool,
    /// Directory delta runs in. This is load-bearing: a repo-local `[delta]`
    /// section is picked up from here when `inherit_gitconfig` is set.
    pub working_dir: Option<PathBuf>,
    pub extra_args: Vec<String>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            width: 120,
            side_by_side: false,
            line_numbers: true,
            wrap: true,
            syntax_theme: None,
            default_language: None,
            features: Vec::new(),
            inherit_gitconfig: true,
            working_dir: None,
            extra_args: Vec::new(),
        }
    }
}

impl Options {
    pub fn to_args(&self) -> Vec<String> {
        let mut a = vec![
            "--paging=never".into(),
            format!("--width={}", self.width),
            // The app draws its own panel headers, so delta's
            // "dev/fd/63 ⟶ b.rs" line would be noise.
            "--file-style=omit".into(),
        ];
        if self.side_by_side {
            a.push("--side-by-side".into());
        }
        if self.line_numbers {
            a.push("--line-numbers".into());
        }
        if !self.wrap {
            a.push("--wrap-max-lines=0".into());
        }
        if let Some(t) = &self.syntax_theme {
            a.push(format!("--syntax-theme={t}"));
        }
        if let Some(l) = &self.default_language {
            a.push(format!("--default-language={l}"));
        }
        if !self.features.is_empty() {
            a.push(format!("--features={}", self.features.join(" ")));
        }
        if !self.inherit_gitconfig {
            a.push("--no-gitconfig".into());
        }
        a.extend(self.extra_args.iter().cloned());
        a
    }

    fn apply_env(&self, cmd: &mut Command) {
        // A GUI launched from Finder or a hotkey inherits no COLORTERM, and
        // delta silently degrades from 24-bit to 256-colour without it. That
        // single missing variable is the likeliest cause of "this doesn't look
        // like my terminal".
        cmd.env("COLORTERM", "truecolor");
        if std::env::var_os("TERM").is_none() {
            cmd.env("TERM", "xterm-256color");
        }
        if let Some(dir) = &self.working_dir {
            cmd.current_dir(dir);
        }
        if !self.inherit_gitconfig {
            cmd.env_remove("DELTA_FEATURES");
        }
    }
}
