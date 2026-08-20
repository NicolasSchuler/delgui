//! Locating, probing and invoking the `delta` binary.

use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::{Duration, Instant};

#[cfg(unix)]
use std::os::unix::process::CommandExt;

/// Oldest delta we are willing to drive. 0.18 is where the flags this app
/// depends on (`--file-style=omit`, `--default-language`) are all present and
/// stable. Releases are infrequent -- 0.18.2 (Sep 2024) to 0.19.0 (Mar 2026) --
/// so pinning a floor costs little.
pub const MINIMUM_VERSION: (u32, u32) = (0, 18);

const PROCESS_LIMITS: ProcessLimits = ProcessLimits {
    timeout: Duration::from_secs(15),
    stdout_bytes: 128 * 1024 * 1024,
    stderr_bytes: 1024 * 1024,
};
const PROCESS_POLL_INTERVAL: Duration = Duration::from_millis(10);

#[derive(Debug)]
pub enum DeltaError {
    /// `delta` is not on PATH. Per the non-goals, this is fatal and loud: we do
    /// not fall back to rendering diffs ourselves.
    NotFound,
    TooOld {
        found: String,
    },
    UnreadableVersion {
        output: String,
    },
    /// delta ran and refused the patch-rendering invocation. The owned Git step
    /// is where exit 1 means "the inputs differ"; delta itself must exit 0 after
    /// consuming that patch.
    Refused {
        code: Option<i32>,
        message: String,
    },
    /// `git` is not on PATH. Every render owns its `git diff --no-index` step,
    /// so Git is a required part of the rendering pipeline.
    GitNotFound,
    /// `git diff --no-index` refused. Exit 1 with a produced patch means the
    /// inputs differ; exit 1 without one is still a failure such as a vanished
    /// input.
    GitRefused {
        code: Option<i32>,
        message: String,
    },
    TimedOut {
        program: &'static str,
        after: Duration,
    },
    OutputTooLarge {
        program: &'static str,
        stream: &'static str,
        limit: usize,
    },
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
            Self::Refused { code, message } => match code {
                Some(c) => write!(f, "delta exited with status {c}.\n{message}"),
                None => write!(f, "delta was killed by a signal.\n{message}"),
            },
            Self::GitNotFound => write!(
                f,
                "`git` was not found on PATH.\n\
                 deltapanes runs `git diff --no-index` before every delta render,\n\
                 so it needs Git too."
            ),
            Self::GitRefused { code, message } => match code {
                Some(c) => write!(f, "git diff exited with status {c}.\n{message}"),
                None => write!(f, "git diff was killed by a signal.\n{message}"),
            },
            Self::TimedOut { program, after } => write!(
                f,
                "{program} did not finish within {} seconds",
                after.as_secs()
            ),
            Self::OutputTooLarge {
                program,
                stream,
                limit,
            } => write!(f, "{program} produced more than {limit} bytes on {stream}"),
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
        let mut cmd = Command::new(path);
        cmd.arg("--version");
        let out = run_simple_command(cmd, "delta", PROCESS_LIMITS)
            .map_err(|e| map_run_error(e, DeltaError::NotFound))?;
        if !out.status.success() {
            return Err(DeltaError::Refused {
                code: out.status.code(),
                message: first_complaint(&out.stderr),
            });
        }
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
        let mut cmd = Command::new(&self.path);
        cmd.arg("--list-syntax-themes");
        let Ok(out) = run_simple_command(cmd, "delta", PROCESS_LIMITS) else {
            return Vec::new();
        };
        if !out.status.success() {
            return Vec::new();
        }
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
    pub fn render(
        &self,
        left: &Input,
        right: &Input,
        opts: &Options,
    ) -> Result<Vec<u8>, DeltaError> {
        let patch = self.diff(left, right, opts)?;
        self.render_patch(&patch, opts)
    }

    /// Run the diff that [`Delta::render`] would have had delta run internally,
    /// and hand back the unified diff itself.
    ///
    /// Merge mode needs the comparison as *data* -- which lines of which buffer
    /// each hunk covers -- and taking that from delta's rendered output means
    /// reading program structure out of a presentation layer. Running the step
    /// here and piping the result to [`Delta::render_patch`] leaves exactly one
    /// diff in play instead of two that can disagree: `delta A B` is
    /// byte-identical to `git diff --no-index A B | delta` (research.md §7).
    pub fn diff(&self, left: &Input, right: &Input, opts: &Options) -> Result<Vec<u8>, DeltaError> {
        let mut cmd = Command::new("git");
        cmd.args(opts.git_diff_args());
        opts.apply_env(&mut cmd);

        let out = over_two_inputs(cmd, left, right, "git diff", PROCESS_LIMITS)
            .map_err(|e| map_run_error(e, DeltaError::GitNotFound))?;
        // Exit 1 plus a patch is "the files differ", which is the whole point.
        // Any higher status, or exit 1 without a patch, is a refusal. The
        // latter is how Git reports a missing input.
        let is_difference = out.status.code() == Some(1) && !out.stdout.is_empty();
        if !out.status.success() && !is_difference {
            return Err(DeltaError::GitRefused {
                code: out.status.code(),
                message: first_complaint(&out.stderr),
            });
        }
        if let Some(error) = out.input_error {
            return Err(DeltaError::Io(error));
        }
        Ok(out.stdout)
    }

    /// Render a diff we already hold, rather than one delta computes for itself.
    pub fn render_patch(&self, patch: &[u8], opts: &Options) -> Result<Vec<u8>, DeltaError> {
        let mut cmd = Command::new(&self.path);
        cmd.args(opts.to_args());
        opts.apply_env(&mut cmd);
        prepare_command(&mut cmd, Stdio::piped());
        let mut child = cmd.spawn().map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                DeltaError::NotFound
            } else {
                DeltaError::Io(e)
            }
        })?;
        // On a thread for the same reason the `/dev/fd` writers are: a pipe
        // holds ~64 KiB, and delta does not read all of its input before it
        // starts producing output. Dropping the handle at the end of the closure
        // is what tells delta the diff is over.
        let mut stdin = child.stdin.take().expect("stdin was piped");
        let bytes = patch.to_vec();
        let pump = std::thread::spawn(move || stdin.write_all(&bytes));
        let out = capture_child(child, vec![pump], Vec::new(), "delta", PROCESS_LIMITS)
            .map_err(|e| map_run_error(e, DeltaError::NotFound))?;
        if !out.status.success() {
            return Err(DeltaError::Refused {
                code: out.status.code(),
                message: first_complaint(&out.stderr),
            });
        }
        if let Some(error) = out.input_error {
            return Err(DeltaError::Io(error));
        }
        Ok(out.stdout)
    }
}

/// Confirm Git is present, before a render needs it.
///
/// Every render owns its `git diff --no-index` step, so a missing Git is as
/// fatal as a missing delta. Asking once at startup is the difference between a
/// message that names the problem and a diff area that fails identically on
/// every keystroke.
pub fn require_git() -> Result<(), DeltaError> {
    let mut cmd = Command::new("git");
    cmd.arg("--version");
    let out = run_simple_command(cmd, "git", PROCESS_LIMITS)
        .map_err(|e| map_run_error(e, DeltaError::GitNotFound))?;
    if !out.status.success() {
        return Err(DeltaError::GitRefused {
            code: out.status.code(),
            message: first_complaint(&out.stderr),
        });
    }
    Ok(())
}

/// Run a command over two inputs given as arguments, keeping the pipe discipline
/// the `/dev/fd` trick depends on.
fn over_two_inputs(
    mut cmd: Command,
    left: &Input,
    right: &Input,
    program: &'static str,
    limits: ProcessLimits,
) -> Result<Captured, RunError> {
    let mut pipes = Pipes::default();
    let left_arg = left.as_argument(&mut pipes)?;
    let right_arg = right.as_argument(&mut pipes)?;
    cmd.arg(left_arg).arg(right_arg);

    prepare_command(&mut cmd, Stdio::null());
    let child = cmd.spawn()?;

    // Feed every in-memory panel concurrently: a pipe holds ~64 KiB, so
    // writing them in sequence would deadlock on anything larger.
    let pumps: Vec<_> = pipes
        .writers
        .into_iter()
        .map(|(mut w, buf)| std::thread::spawn(move || w.write_all(&buf)))
        .collect();

    // `/dev/fd/N` is resolved by name by the child at whatever moment it gets
    // round to it. The descriptor has to still be ours for the whole run, or a
    // concurrent render can be handed the number in the meantime.
    //
    // Dropped, though, and not leaked as this once did. Leaking cost two
    // descriptors per render, and the GUI renders per keystroke -- a few
    // minutes of typing reached the 256-descriptor ceiling a Finder-launched
    // process gets, and everything failed from there on.
    capture_child(child, pumps, pipes.readers, program, limits)
}

#[derive(Clone, Copy)]
struct ProcessLimits {
    timeout: Duration,
    stdout_bytes: usize,
    stderr_bytes: usize,
}

#[derive(Debug)]
struct Captured {
    status: ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    input_error: Option<std::io::Error>,
}

enum RunError {
    Io(std::io::Error),
    TimedOut {
        program: &'static str,
        after: Duration,
    },
    OutputTooLarge {
        program: &'static str,
        stream: &'static str,
        limit: usize,
    },
}

impl From<std::io::Error> for RunError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

fn map_run_error(error: RunError, not_found: DeltaError) -> DeltaError {
    match error {
        RunError::Io(error) if error.kind() == std::io::ErrorKind::NotFound => not_found,
        RunError::Io(error) => DeltaError::Io(error),
        RunError::TimedOut { program, after } => DeltaError::TimedOut { program, after },
        RunError::OutputTooLarge {
            program,
            stream,
            limit,
        } => DeltaError::OutputTooLarge {
            program,
            stream,
            limit,
        },
    }
}

fn run_simple_command(
    mut cmd: Command,
    program: &'static str,
    limits: ProcessLimits,
) -> Result<Captured, RunError> {
    prepare_command(&mut cmd, Stdio::null());
    let child = cmd.spawn()?;
    capture_child(child, Vec::new(), Vec::new(), program, limits)
}

fn prepare_command(cmd: &mut Command, stdin: Stdio) {
    cmd.stdin(stdin)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    cmd.process_group(0);
}

enum StreamRead {
    Data(Vec<u8>),
    TooLarge,
    Io(std::io::Error),
}

fn read_bounded<R: Read>(
    mut stream: R,
    limit: usize,
    signal: Arc<AtomicU8>,
    signal_value: u8,
) -> StreamRead {
    let mut output = Vec::with_capacity(limit.min(64 * 1024));
    let mut chunk = [0_u8; 16 * 1024];
    loop {
        match stream.read(&mut chunk) {
            Ok(0) => return StreamRead::Data(output),
            Ok(read) if read > limit.saturating_sub(output.len()) => {
                signal.store(signal_value, Ordering::Release);
                return StreamRead::TooLarge;
            }
            Ok(read) => output.extend_from_slice(&chunk[..read]),
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => {
                signal.store(3, Ordering::Release);
                return StreamRead::Io(error);
            }
        }
    }
}

fn capture_child(
    mut child: Child,
    pumps: Vec<std::thread::JoinHandle<std::io::Result<()>>>,
    inherited_readers: Vec<std::io::PipeReader>,
    program: &'static str,
    limits: ProcessLimits,
) -> Result<Captured, RunError> {
    let stdout = child.stdout.take().expect("stdout was piped");
    let stderr = child.stderr.take().expect("stderr was piped");
    let signal = Arc::new(AtomicU8::new(0));
    let stdout_signal = Arc::clone(&signal);
    let stderr_signal = Arc::clone(&signal);
    let stdout_reader =
        std::thread::spawn(move || read_bounded(stdout, limits.stdout_bytes, stdout_signal, 1));
    let stderr_reader =
        std::thread::spawn(move || read_bounded(stderr, limits.stderr_bytes, stderr_signal, 2));

    let started = Instant::now();
    let mut timed_out = false;
    let mut poll_error = None;
    let status = loop {
        if signal.load(Ordering::Acquire) != 0 {
            terminate_child(&mut child);
            break None;
        }
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if started.elapsed() >= limits.timeout => {
                timed_out = true;
                terminate_child(&mut child);
                break None;
            }
            Ok(None) => std::thread::sleep(PROCESS_POLL_INTERVAL),
            Err(error) => {
                poll_error = Some(error);
                terminate_child(&mut child);
                break None;
            }
        }
    };

    // The parent-held read ends must survive until the child exits, but must be
    // closed before joining writers or an early-exiting child can leave those
    // writers blocked forever.
    drop(inherited_readers);
    let mut input_error = None;
    for pump in pumps {
        match pump.join() {
            Ok(Ok(())) => {}
            Ok(Err(error)) if input_error.is_none() => input_error = Some(error),
            Err(_) if input_error.is_none() => {
                input_error = Some(std::io::Error::other("input pump thread panicked"));
            }
            Ok(Err(_)) | Err(_) => {}
        }
    }

    let stdout = stdout_reader
        .join()
        .map_err(|_| RunError::Io(std::io::Error::other("stdout reader thread panicked")))?;
    let stderr = stderr_reader
        .join()
        .map_err(|_| RunError::Io(std::io::Error::other("stderr reader thread panicked")))?;

    if timed_out {
        return Err(RunError::TimedOut {
            program,
            after: limits.timeout,
        });
    }
    if let Some(error) = poll_error {
        return Err(RunError::Io(error));
    }
    let stdout = match stdout {
        StreamRead::Data(output) => output,
        StreamRead::TooLarge => {
            return Err(RunError::OutputTooLarge {
                program,
                stream: "stdout",
                limit: limits.stdout_bytes,
            });
        }
        StreamRead::Io(error) => return Err(RunError::Io(error)),
    };
    let stderr = match stderr {
        StreamRead::Data(output) => output,
        StreamRead::TooLarge => {
            return Err(RunError::OutputTooLarge {
                program,
                stream: "stderr",
                limit: limits.stderr_bytes,
            });
        }
        StreamRead::Io(error) => return Err(RunError::Io(error)),
    };
    Ok(Captured {
        status: status.expect("child status is present after a normal exit"),
        stdout,
        stderr,
        input_error,
    })
}

fn terminate_child(child: &mut Child) {
    #[cfg(unix)]
    unsafe {
        // Every child is placed in its own process group by `prepare_command`,
        // so terminate helpers it may have launched as well as the immediate
        // process. A negative pid addresses that group.
        libc::kill(-(child.id() as i32), libc::SIGKILL);
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// The useful part of delta's stderr: clap prints a one-line error followed by
/// a usage block naming every flag it knows, which is not something to put in
/// front of a user.
fn first_complaint(stderr: &[u8]) -> String {
    let text = String::from_utf8_lossy(stderr);
    let plain = strip_sgr(&text);
    plain
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("delta printed nothing to explain itself.")
        .to_string()
}

/// delta colours its own error messages, and they are quoted back into a GUI
/// that has no terminal to interpret them.
fn strip_sgr(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        for c in chars.by_ref() {
            if c.is_ascii_alphabetic() {
                break;
            }
        }
    }
    out
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
            .and_then(|p| {
                p.trim_end_matches(|c: char| !c.is_ascii_digit())
                    .parse()
                    .ok()
            })
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

/// The pipes standing in for temp files, held together so their lifetimes are
/// obvious: the read ends must outlive `spawn`, the write ends must outlive it
/// by longer still, since they are what feeds the child.
#[derive(Default)]
struct Pipes {
    readers: Vec<std::io::PipeReader>,
    writers: Vec<(std::io::PipeWriter, Vec<u8>)>,
}

impl Input {
    /// The bytes this side will hand delta, when they are in memory.
    ///
    /// Merge mode requires both sides to be buffers, so that the line ranges it
    /// splices provably describe the same text that was diffed -- a `Path` side
    /// is re-read by git at diff time and can have changed since.
    pub fn as_bytes(&self) -> Option<&[u8]> {
        match self {
            Self::Buffer(bytes) => Some(bytes),
            Self::Path(_) => None,
        }
    }

    fn as_argument(&self, pipes: &mut Pipes) -> std::io::Result<PathBuf> {
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
                pipes.readers.push(reader);
                pipes.writers.push((writer, bytes.clone()));
                Ok(PathBuf::from(format!("/dev/fd/{fd}")))
            }
        }
    }
}

/// Which of delta's two colour schemes to render in.
///
/// delta normally works this out by querying the terminal for its background,
/// which a GUI process has none of -- it falls back to dark. Stating it matters
/// because it decides the plus and minus backgrounds (`#002800`/`#3f0001` dark,
/// `#d0ffd0`/`#ffe0e0` light) *independently* of `syntax_theme`, so a light diff
/// on dark chrome is otherwise easy to produce by accident.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Appearance {
    Dark,
    Light,
}

impl Appearance {
    pub fn is_dark(self) -> bool {
        self == Self::Dark
    }
}

/// How much of a whitespace change counts as a difference.
///
/// Every comparable tool offers some form of this, and it is the difference
/// between re-indenting a file being one difference or forty. Git spells it
/// three ways and they are not interchangeable: `-b` still reports a line that
/// gained indentation, where `-w` does not.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum Whitespace {
    /// Every space is a difference. Git's own default.
    #[default]
    Exact,
    /// `-b`: changes in the *amount* of whitespace, including at end of line.
    Amount,
    /// `-w`: whitespace anywhere, including indentation.
    All,
}

impl Whitespace {
    fn flag(self) -> Option<&'static str> {
        match self {
            Self::Exact => None,
            Self::Amount => Some("--ignore-space-change"),
            Self::All => Some("--ignore-all-space"),
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
    /// delta's `@@ …` replacement: a boxed line number above each hunk. Useful
    /// on a long diff and pure noise on a two-buffer comparison of ten lines,
    /// where the app's own panel headers already say what is being compared.
    pub hunk_headers: bool,
    /// Label every hunk header, so the caller can say which rows belong to which
    /// hunk. `--hunk-label` is the only way to ask.
    ///
    /// It applies to whatever header style is in force, so this does not decide
    /// whether a header is drawn -- `hunk_headers` still does. With headers off,
    /// the cheapest markable header is drawn instead of none, and the caller
    /// drops the row once it has been found (`merge::prepare_rows`); the result
    /// is byte-identical to `--hunk-header-style=omit`, which
    /// `dropping_the_marked_rows_restores_the_unmarked_rendering` pins.
    ///
    /// Merge mode therefore asks for headers off as well as marks on: it draws
    /// its own control row, and delta's decoration rules would land inside the
    /// hunk rather than above it.
    pub marked_hunks: bool,
    /// Pin the diff's shape, so a hunk is the same hunk whatever git is
    /// configured to do.
    ///
    /// Merge mode describes every take as a pair of line ranges, so it owns the
    /// structure it operates on. A plain render must not: `diff.indentHeuristic`
    /// is on by default and moves hunk boundaries in indented code -- an added
    /// function comes out whole with it and split mid-comment without it -- and
    /// delta's own two-file mode reads it. Forcing it off everywhere would make
    /// every ordinary diff worse than the one delta would have drawn.
    pub pin_hunk_structure: bool,
    /// What to stop calling a difference.
    ///
    /// These reach `git diff` and nothing else, which is why they sit apart from
    /// the delta options: `delta -w a b` is not an ignore, it is delta parsing
    /// `-w` as `--width` and then refusing the filename. Owning the diff step is
    /// what makes them reachable at all.
    pub whitespace: Whitespace,
    pub ignore_blank_lines: bool,
    /// `--ignore-cr-at-eol`. The cheap half of the CRLF problem: a file saved on
    /// Windows against one saved anywhere else differs on every single line.
    pub ignore_cr_at_eol: bool,
    /// `--ignore-matching-lines`: changes where every changed line matches are
    /// not differences. A timestamp or a build number is the case for it.
    pub ignore_matching: Option<String>,
    /// Lines of unchanged context each hunk carries, for the diff step.
    ///
    /// Three is git's default and what delta uses when it runs the diff itself.
    /// Merge mode drops it to zero: at three, `examples/config_before.rs` and
    /// `config_after.rs` -- thirteen lines with four independent changes --
    /// arrive as a *single* hunk, which is one take for the whole file.
    /// Wide enough to mean "the whole file": `u8` would cap the offer at 255
    /// lines of context, which is not the same offer.
    pub context: u32,
    pub syntax_theme: Option<String>,
    /// Colour scheme to pin delta to. `None` leaves the choice to delta and to
    /// gitconfig, which is what the tests want; the GUI always states it so the
    /// diff cannot disagree with the window around it.
    pub appearance: Option<Appearance>,
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
            hunk_headers: true,
            marked_hunks: false,
            pin_hunk_structure: false,
            whitespace: Whitespace::Exact,
            ignore_blank_lines: false,
            ignore_cr_at_eol: false,
            ignore_matching: None,
            context: 3,
            syntax_theme: None,
            appearance: None,
            default_language: None,
            features: Vec::new(),
            inherit_gitconfig: true,
            working_dir: None,
            extra_args: Vec::new(),
        }
    }
}

impl Options {
    /// The owned Git half of the render pipeline.
    ///
    /// This is public so a frontend can show the pipeline it actually runs
    /// without duplicating safety-critical flags in presentation code.
    pub fn git_diff_args(&self) -> Vec<String> {
        let mut a = vec![
            "--no-pager".into(),
            "diff".into(),
            "--no-index".into(),
            // Colour would arrive inside the text we are about to parse. Git
            // suppresses it when not writing to a terminal, but `color.diff =
            // always` in a gitconfig overrides that.
            "--no-color".into(),
            // Never execute configured helpers against private panel content.
            "--no-ext-diff".into(),
            "--no-textconv".into(),
        ];
        // Only where a hunk is a pair of line ranges something will be spliced
        // at; see the field. A plain render leaves git's own heuristics alone.
        if self.pin_hunk_structure {
            a.push("--diff-algorithm=myers".into());
            a.push("--no-indent-heuristic".into());
        }
        if let Some(flag) = self.whitespace.flag() {
            a.push(flag.into());
        }
        if self.ignore_blank_lines {
            a.push("--ignore-blank-lines".into());
        }
        if self.ignore_cr_at_eol {
            a.push("--ignore-cr-at-eol".into());
        }
        // One argument rather than `-I` and its value, so that a pattern
        // beginning with a dash stays a pattern and does not become an option.
        if let Some(pattern) = &self.ignore_matching {
            a.push(format!("--ignore-matching-lines={pattern}"));
        }
        // Stated rather than inherited from `diff.context`, because the width is
        // a control the app offers and merge mode depends on it being zero.
        a.push(format!("--unified={}", self.context));
        a
    }

    /// What is being ignored, for saying so on screen.
    ///
    /// An ignore that suppresses a difference silently is the one way this does
    /// harm, so the app never leaves it invisible.
    pub fn ignoring(&self) -> Option<String> {
        let mut what: Vec<&str> = Vec::new();
        match self.whitespace {
            Whitespace::Exact => {}
            Whitespace::Amount => what.push("whitespace changes"),
            Whitespace::All => what.push("whitespace"),
        }
        if self.ignore_blank_lines {
            what.push("blank lines");
        }
        if self.ignore_cr_at_eol {
            what.push("line endings");
        }
        if self.ignore_matching.is_some() {
            what.push("matching lines");
        }
        (!what.is_empty()).then(|| format!("ignoring {}", what.join(", ")))
    }

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
        } else if self.side_by_side {
            // delta's side-by-side layout switches line numbers on for itself
            // and offers no way to say no: there is no `--no-line-numbers`, and
            // `--line-numbers=false` exits 2. Emptying both column formats is
            // the only lever that works, and it takes the column separators
            // with it. Without this the checkbox is dead in the app's default
            // mode, which is exactly how it was found.
            a.push("--line-numbers-left-format=".into());
            a.push("--line-numbers-right-format=".into());
        }
        if !self.wrap {
            a.push("--wrap-max-lines=0".into());
        }
        // One `--hunk-header-style` or none: delta exits 2 on a repeated flag,
        // and the app emits `=omit` by default, so an out-of-band `=raw` on top
        // of it would kill the whole diff rather than just the take controls.
        if self.marked_hunks && !self.hunk_headers {
            // A row to find, and as little of one as possible, because it is
            // about to be dropped. Stated even though `line-number` is delta's
            // default, because a `[delta]` section can set it to `omit` -- and
            // then there is no row to find at all.
            a.push("--hunk-header-style=line-number".into());
            a.push("--hunk-header-decoration-style=none".into());
        } else if !self.marked_hunks && !self.hunk_headers {
            a.push("--hunk-header-style=omit".into());
        }
        if self.marked_hunks {
            // `raw` (git's own `@@ …` line) would carry the ranges too, but
            // `--hunk-label` does not apply to it, and a line of file content
            // can look exactly like a header. The ranges come from the diff
            // itself instead; this row only has to be findable.
            a.push(format!("--hunk-label={}", crate::merge::HUNK_LABEL));
        }
        if let Some(t) = &self.syntax_theme {
            a.push(format!("--syntax-theme={t}"));
        }
        match self.appearance {
            Some(Appearance::Dark) => a.push("--dark".into()),
            Some(Appearance::Light) => a.push("--light".into()),
            None => {}
        }
        if let Some(l) = &self.default_language {
            a.push(format!("--default-language={l}"));
        }
        // Emitted even when empty, and only then: an omitted `--features` lets
        // `delta.features` and `DELTA_FEATURES` apply, so unticking every box in
        // the UI would otherwise silently leave them all on.
        if self.inherit_gitconfig || !self.features.is_empty() {
            a.push(format!("--features={}", self.features.join(" ")));
        }
        if !self.inherit_gitconfig {
            a.push("--no-gitconfig".into());
        }
        a.extend(self.extra_args.iter().cloned());
        a
    }

    /// Everything that changes what comes back, including what reaches the diff
    /// step rather than delta's argv.
    ///
    /// The frontend keys its render cache off this and not off [`to_args`],
    /// which mentions nothing that shapes the diff: a setting that moves only
    /// that step would otherwise leave the cache serving the previous render
    /// forever. Both argument lists whole, rather than the diff-only settings
    /// spelled out again here, so that adding one cannot forget this.
    ///
    /// [`to_args`]: Options::to_args
    pub fn fingerprint(&self) -> Vec<String> {
        let mut a = self.to_args();
        a.extend(self.git_diff_args());
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

        // Do not let the launching environment replace the owned diff command
        // with an executable of its choice. `--no-ext-diff` is the primary
        // guard; clearing the environment also protects delta subcommands and
        // old Git versions from inheriting an injected configuration layer.
        cmd.env_remove("GIT_EXTERNAL_DIFF");
        cmd.env_remove("GIT_CONFIG_PARAMETERS");
        cmd.env_remove("GIT_CONFIG_COUNT");
        for (key, _) in std::env::vars_os() {
            let key_text = key.to_string_lossy();
            if key_text.starts_with("GIT_CONFIG_KEY_") || key_text.starts_with("GIT_CONFIG_VALUE_")
            {
                cmd.env_remove(key);
            }
        }
        if !self.inherit_gitconfig {
            cmd.env_remove("DELTA_FEATURES");
            // delta ignores `GIT_CONFIG_GLOBAL` (research.md §4) but the `git
            // diff` step honours it. Without this, "output that does not depend
            // on your gitconfig" would still leave the hunk boundaries to
            // whatever `diff.algorithm` the user has set.
            cmd.env("GIT_CONFIG_GLOBAL", "/dev/null");
            cmd.env("GIT_CONFIG_SYSTEM", "/dev/null");
            cmd.env("GIT_CONFIG_NOSYSTEM", "1");
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn test_limits(timeout: Duration, stdout_bytes: usize) -> ProcessLimits {
        ProcessLimits {
            timeout,
            stdout_bytes,
            stderr_bytes: 1024,
        }
    }

    #[test]
    fn git_diff_args_state_every_safety_and_hunk_option() {
        let options = Options {
            context: 7,
            pin_hunk_structure: true,
            ..Options::default()
        };
        assert_eq!(
            options.git_diff_args(),
            [
                "--no-pager",
                "diff",
                "--no-index",
                "--no-color",
                "--no-ext-diff",
                "--no-textconv",
                "--diff-algorithm=myers",
                "--no-indent-heuristic",
                "--unified=7",
            ]
        );
    }

    /// These reach `git diff` and nothing else. Spelled out because they are
    /// the difference between re-indenting a file being one difference or forty,
    /// and because `-I` takes its pattern joined to it: split into two argv
    /// entries, a pattern starting with a dash becomes the next option.
    #[test]
    fn ignore_settings_reach_the_diff_step() {
        let options = Options {
            whitespace: Whitespace::All,
            ignore_blank_lines: true,
            ignore_cr_at_eol: true,
            ignore_matching: Some("--version".into()),
            ..Options::default()
        };
        let args = options.git_diff_args();
        assert!(args.contains(&"--ignore-all-space".to_string()));
        assert!(args.contains(&"--ignore-blank-lines".to_string()));
        assert!(args.contains(&"--ignore-cr-at-eol".to_string()));
        assert!(args.contains(&"--ignore-matching-lines=--version".to_string()));
        assert!(!args.iter().any(|a| a == "--version"), "split into two");

        // `-b` and `-w` are not the same offer, and neither is the default.
        let amount = Options {
            whitespace: Whitespace::Amount,
            ..Options::default()
        };
        assert!(
            amount
                .git_diff_args()
                .contains(&"--ignore-space-change".to_string())
        );
        assert!(
            !Options::default()
                .git_diff_args()
                .iter()
                .any(|a| a.starts_with("--ignore"))
        );
    }

    /// None of these reach delta's argv, so the cache key has to carry them or
    /// switching one on would leave the previous diff on screen forever.
    #[test]
    fn the_fingerprint_covers_every_ignore() {
        let base = Options::default();
        for changed in [
            Options {
                whitespace: Whitespace::All,
                ..Options::default()
            },
            Options {
                ignore_blank_lines: true,
                ..Options::default()
            },
            Options {
                ignore_cr_at_eol: true,
                ..Options::default()
            },
            Options {
                ignore_matching: Some("x".into()),
                ..Options::default()
            },
        ] {
            assert_eq!(base.to_args(), changed.to_args(), "not a delta flag");
            assert_ne!(base.fingerprint(), changed.fingerprint());
        }
    }

    /// An ignore that quietly removes a difference is the one way this feature
    /// does harm, so the app can always say what is being left out.
    #[test]
    fn what_is_being_ignored_can_be_said_out_loud() {
        assert_eq!(Options::default().ignoring(), None);
        let options = Options {
            whitespace: Whitespace::All,
            ignore_blank_lines: true,
            ..Options::default()
        };
        assert_eq!(
            options.ignoring().as_deref(),
            Some("ignoring whitespace, blank lines")
        );
    }

    /// Git is now asked for at startup, so the check has to be right about the
    /// binary it is checking: a wrong program name would turn every launch on a
    /// perfectly good machine into "`git` was not found on PATH".
    #[test]
    fn git_is_found_where_every_render_will_need_it() {
        super::require_git().expect("these tests already require git on PATH");
    }

    /// The safety flags are not negotiable; the two that shape the diff are.
    /// `diff.indentHeuristic` is on by default and decides whether an added
    /// function arrives whole or split mid-comment, and delta's own two-file
    /// mode honours it -- so a plain render has to as well.
    #[test]
    fn a_plain_render_leaves_git_s_own_heuristics_alone() {
        let args = Options::default().git_diff_args();
        assert!(args.contains(&"--no-ext-diff".to_string()));
        assert!(args.contains(&"--no-textconv".to_string()));
        assert!(args.contains(&"--no-color".to_string()));
        assert!(!args.iter().any(|a| a.starts_with("--diff-algorithm")));
        assert!(!args.iter().any(|a| a == "--no-indent-heuristic"));
    }

    /// The cache key is built from `fingerprint`, and a setting that never
    /// reaches delta's argv would otherwise leave it serving the previous
    /// render forever.
    #[test]
    fn the_fingerprint_covers_settings_that_only_reach_the_diff() {
        let base = Options::default();
        let wider_context = Options {
            context: 0,
            ..Options::default()
        };
        assert_eq!(base.to_args(), wider_context.to_args(), "not a delta flag");
        assert_ne!(base.fingerprint(), wider_context.fingerprint());

        let pinned = Options {
            pin_hunk_structure: true,
            ..Options::default()
        };
        assert_eq!(base.to_args(), pinned.to_args(), "not a delta flag either");
        assert_ne!(base.fingerprint(), pinned.fingerprint());
    }

    #[test]
    fn capture_times_out_and_reaps_a_stuck_child() {
        let mut command = Command::new("sh");
        command.args(["-c", "sleep 30"]);
        let started = Instant::now();
        let error = run_simple_command(
            command,
            "test child",
            test_limits(Duration::from_millis(50), 1024),
        )
        .expect_err("sleeping child should time out");
        assert!(
            matches!(error, RunError::TimedOut { .. }),
            "unexpected timeout result"
        );
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "timeout did not promptly cancel and reap the child"
        );
    }

    #[test]
    fn capture_stops_an_unbounded_stdout_stream() {
        let mut command = Command::new("sh");
        command.args(["-c", "while :; do printf 0123456789abcdef; done"]);
        let error = run_simple_command(
            command,
            "test child",
            test_limits(Duration::from_secs(2), 1024),
        )
        .expect_err("unbounded stdout should hit the cap");
        assert!(
            matches!(
                error,
                RunError::OutputTooLarge {
                    stream: "stdout",
                    limit: 1024,
                    ..
                }
            ),
            "unexpected output-limit result"
        );
    }
}
