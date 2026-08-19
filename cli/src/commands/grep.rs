//! `nashcode grep`: ripgrep's surface, the code index's answers.
//!
//! An agent reaches for `rg` without thinking. This command is what makes that reflex
//! land on the index instead: the flags are rg's, the output is grep's, and the extra
//! knowledge — where a name is *defined*, how many things reference it, what the
//! embeddings think it means — rides in `#` comment lines that no grep parser reads.
//!
//! Four rules run through it.
//!
//! **An unknown flag is never an error, and never changes the meaning of the search.**
//! Whatever an agent types out of rg habit is accepted; a flag this command does not
//! model is *forwarded to the local rg run*, because rg does model it. Dropping `-F`
//! or `-v` on the floor would answer a different question than the one asked, in
//! silence, which is worse than refusing the flag.
//!
//! **Freshness is hybrid.** Text hits come from a real `rg` run over the working tree
//! whenever the command runs inside a checkout with rg on PATH, because the tree an
//! agent is editing is always fresher than the index. Definitions, counts, and semantic
//! hits come from `GET /:repo/code/find`, and the `-i`, `-t`, `-g` and path narrowing
//! goes *with* the request so the server spends its row budget inside the filter.
//!
//! **Nothing hangs.** The viewer gets ten seconds and so does the local rg, which is
//! reaped and killed rather than waited on: one FIFO in a path argument is otherwise a
//! search that never returns.
//!
//! **Every degradation says so in one `#` line.** A dead viewer, a rejected rg flag, a
//! capped answer — all of them print, because a search that silently answers a smaller
//! question is the one failure an agent cannot detect.

use super::Ctx;
use crate::api::Client;
use crate::output::Out;
use crate::timefmt::ago;
use crate::vcs;
use serde_json::{Value, json};
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// An agent waits on this. The default 60s client would turn a dead viewer into a
/// minute of silence in the middle of a search.
const TIMEOUT: Duration = Duration::from_secs(10);

/// The local rg gets the same ten seconds, and is killed at the end of them. A path
/// argument naming a FIFO, or a filesystem that has gone away, must not become a
/// command that never returns.
const RG_TIMEOUT: Duration = Duration::from_secs(10);

/// The most rg output this will read. Past it the child is killed and the answer says
/// it was capped: one minified bundle can otherwise be gigabytes of "matches".
const RG_MAX_BYTES: usize = 8 * 1024 * 1024;

/// How many rows per layer to ask the index for. The viewer clamps at 100 anyway, and
/// grep output is expected to be long.
const LIMIT: usize = 100;

/// Characters of the indexed commit in the header. Enough to paste into `git show`.
const TIP_LEN: usize = 7;

/// The rg binary, behind an env seam so the tests can supply their own.
fn rg_bin() -> String {
    std::env::var("NASHCODE_RG_BIN").unwrap_or_else(|_| "rg".to_string())
}

// ---- flags -------------------------------------------------------------------------

/// Long rg flags that take a value. A flag this command does not model still has to be
/// read correctly, or its value is mistaken for the pattern: `--color never retry`
/// searched for `never` before this table existed.
const LONG_WITH_VALUE: &[&str] = &[
    "color",
    "colors",
    "sort",
    "sortr",
    "threads",
    "replace",
    "iglob",
    "type-not",
    "type-add",
    "type-clear",
    "pre",
    "pre-glob",
    "max-count",
    "max-columns",
    "max-depth",
    "max-filesize",
    "context-separator",
    "field-context-separator",
    "field-match-separator",
    "encoding",
    "engine",
    "file",
    "ignore-file",
    "path-separator",
    "regex-size-limit",
    "dfa-size-limit",
    "hostname-bin",
    "hyperlink-format",
];

/// The same for short flags this command does not model: `-m3`, `-m 3`, `-j 4`.
const SHORT_WITH_VALUE: &[char] = &['m', 'M', 'j', 'r', 'T', 'f', 'E'];

/// What the argument list asked for.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Flags {
    pub pattern: Option<String>,
    pub paths: Vec<String>,
    pub ignore_case: bool,
    /// `-l`: paths only, one per line.
    pub files_only: bool,
    pub after: Option<String>,
    pub before: Option<String>,
    pub types: Vec<String>,
    pub globs: Vec<String>,
    pub repo: Option<String>,
    /// `--profile`: nashcode's, not rg's. Extracted here rather than forwarded,
    /// because rg would reject it and the profile decides which viewer to ask.
    pub profile: Option<String>,
    pub json: bool,
    pub help: bool,
    /// `--quiet`: agcli's reserved flag. Silences the `#` notes on stderr.
    pub quiet: bool,
    /// Flags this command does not model, in the order they were typed and with their
    /// values. Forwarded verbatim to the local rg, which does model them.
    pub ignored: Vec<String>,
}

/// Read one value: `--flag=v`, `--flag v`, `-Cv`, or `-C v`.
fn value(inline: Option<String>, args: &[String], next: &mut usize) -> Option<String> {
    if let Some(v) = inline {
        return Some(v);
    }
    let v = args.get(*next).cloned();
    if v.is_some() {
        *next += 1;
    }
    v
}

/// Read an argument list the way rg would; keep whatever is left over.
///
/// Nothing is discarded. A flag this command does not model goes into `ignored`,
/// together with its value when [`LONG_WITH_VALUE`] or [`SHORT_WITH_VALUE`] says it
/// takes one, and the whole list is handed to the local rg run. A flag that is neither
/// modelled nor known to take a value consumes only itself — guessing otherwise would
/// swallow the pattern of anyone who typed an unknown *boolean* flag.
pub fn parse(args: &[String]) -> Flags {
    let mut flags = Flags::default();
    let mut rest: Vec<String> = Vec::new();
    let mut i = 0;
    let mut literal = false;

    while i < args.len() {
        let arg = args[i].clone();
        i += 1;

        if literal || arg == "-" || !arg.starts_with('-') {
            rest.push(arg);
            continue;
        }
        if arg == "--" {
            literal = true;
            continue;
        }

        if let Some(long) = arg.strip_prefix("--") {
            let (name, inline) = match long.split_once('=') {
                Some((name, v)) => (name, Some(v.to_owned())),
                None => (long, None),
            };
            match name {
                "json" => flags.json = true,
                "help" => flags.help = true,
                "quiet" => flags.quiet = true,
                "repo" => flags.repo = value(inline, args, &mut i),
                // nashcode's own, wherever it lands. rg has no --profile, so
                // forwarding it would fail the local pass and lose the flag.
                "profile" => flags.profile = value(inline, args, &mut i),
                "ignore-case" => flags.ignore_case = true,
                "case-sensitive" => flags.ignore_case = false,
                "files-with-matches" => flags.files_only = true,
                // Already the contract: every hit carries its line number.
                "line-number" | "with-filename" | "no-heading" => {}
                "context" => {
                    let v = value(inline, args, &mut i);
                    flags.after = v.clone();
                    flags.before = v;
                }
                "after-context" => flags.after = value(inline, args, &mut i),
                "before-context" => flags.before = value(inline, args, &mut i),
                "type" => flags.types.extend(value(inline, args, &mut i)),
                "glob" => flags.globs.extend(value(inline, args, &mut i)),
                "regexp" => flags.pattern = value(inline, args, &mut i),
                _ => {
                    let takes_value = inline.is_none() && LONG_WITH_VALUE.contains(&name);
                    flags.ignored.push(arg.clone());
                    if takes_value {
                        // Its value is the next token: record it, and keep it out of
                        // the running for the pattern.
                        flags.ignored.extend(value(None, args, &mut i));
                    }
                }
            }
            continue;
        }

        // A short cluster: `-il`, `-C3`, `-trust`, `-g*.rs`, `-m5`.
        let chars: Vec<char> = arg.chars().skip(1).collect();
        let mut k = 0;
        while k < chars.len() {
            let letter = chars[k];
            k += 1;
            match letter {
                'i' => flags.ignore_case = true,
                's' => flags.ignore_case = false,
                'n' | 'H' => {}
                'l' => flags.files_only = true,
                'h' => flags.help = true,
                'C' | 'A' | 'B' | 't' | 'g' | 'e' => {
                    let tail: String = chars[k..].iter().collect();
                    k = chars.len();
                    let inline = (!tail.is_empty())
                        .then(|| tail.strip_prefix('=').unwrap_or(&tail).to_owned());
                    let v = value(inline, args, &mut i);
                    match letter {
                        'C' => {
                            flags.after = v.clone();
                            flags.before = v;
                        }
                        'A' => flags.after = v,
                        'B' => flags.before = v,
                        't' => flags.types.extend(v),
                        'g' => flags.globs.extend(v),
                        _ => flags.pattern = v,
                    }
                }
                unknown if SHORT_WITH_VALUE.contains(&unknown) => {
                    let tail: String = chars[k..].iter().collect();
                    k = chars.len();
                    flags.ignored.push(format!("-{unknown}"));
                    if tail.is_empty() {
                        flags.ignored.extend(value(None, args, &mut i));
                    } else {
                        flags.ignored.push(tail);
                    }
                }
                unknown => flags.ignored.push(format!("-{unknown}")),
            }
        }
    }

    let mut rest = rest.into_iter();
    if flags.pattern.is_none() {
        flags.pattern = rest.next();
    }
    flags.paths = rest.collect();
    flags
}

/// nashcode's own flags, typed *before* the subcommand.
///
/// `grep` is a raw passthrough command: agcli hands it every token after
/// `grep`, verbatim, and nothing before it. That is exactly right for the
/// pattern — `nashcode grep -- -Zthreads` arrives with its `--` intact — but the
/// flags that belong to nashcode rather than to rg can be typed on either side
/// of the command name, and only one side reaches [`parse`]. So the other side
/// is read back off the process's own argument list, scanning as far as the
/// first non-flag token, which is the subcommand.
///
/// [`parse`] handles the same three flags after the command name. Either
/// spelling works, and `nashcode grep --profile work retry` no longer searches
/// for the word `work`.
#[derive(Debug, Default)]
pub struct Preceding {
    pub json: bool,
    pub quiet: bool,
    pub profile: Option<String>,
}

pub fn preceding_flags() -> Preceding {
    read_preceding(std::env::args().skip(1))
}

fn read_preceding(argv: impl Iterator<Item = String>) -> Preceding {
    let mut argv = argv;
    let mut found = Preceding::default();
    while let Some(arg) = argv.next() {
        if let Some(value) = arg.strip_prefix("--profile=") {
            found.profile = Some(value.to_owned());
            continue;
        }
        match arg.as_str() {
            "--profile" => found.profile = argv.next(),
            "--json" => found.json = true,
            "--quiet" => found.quiet = true,
            _ if arg.starts_with('-') => {}
            // The subcommand: everything after it is grep's own.
            _ => break,
        }
    }
    found
}

// ---- path filters ------------------------------------------------------------------

/// Extensions each `-t` name covers on the index side.
///
/// This mirrors `code::type_extensions` in the viewer, which is the copy that matters:
/// the server filters before it spends its row budget. This one is the belt to that
/// braces, for an older viewer that does not know the parameters yet.
fn type_extensions(name: &str) -> &'static [&'static str] {
    match name {
        "rust" => &["rs"],
        "py" | "python" => &["py", "pyi"],
        "ts" | "typescript" => &["ts", "tsx", "mts", "cts"],
        "js" | "javascript" => &["js", "jsx", "mjs", "cjs"],
        "md" | "markdown" => &["md", "markdown"],
        "toml" => &["toml"],
        "json" => &["json"],
        "yaml" | "yml" => &["yaml", "yml"],
        "go" => &["go"],
        "c" => &["c", "h"],
        "cpp" | "c++" => &["cc", "cpp", "cxx", "hpp", "hh"],
        "java" => &["java"],
        "rb" | "ruby" => &["rb"],
        "sh" | "bash" => &["sh", "bash"],
        "html" => &["html", "htm"],
        "css" => &["css"],
        "sql" => &["sql"],
        _ => &[],
    }
}

/// Which index-side paths survive `-t`, `-g`, and the path arguments.
///
/// Globs go through `globset`, which is ripgrep's own glob engine, so `{a,b}` and
/// `[abc]` mean here what they mean in rg and the two halves of a hybrid search cannot
/// disagree about what `-g` selected.
#[derive(Debug, Default)]
pub struct PathFilter {
    extensions: Vec<String>,
    allow: Option<globset::GlobSet>,
    deny: Option<globset::GlobSet>,
    prefixes: Vec<String>,
}

impl PathFilter {
    pub fn new(flags: &Flags, prefixes: Vec<String>) -> Self {
        let mut extensions = Vec::new();
        for name in &flags.types {
            extensions.extend(type_extensions(name).iter().map(|e| (*e).to_owned()));
        }
        let mut allow = globset::GlobSetBuilder::new();
        let mut deny = globset::GlobSetBuilder::new();
        let (mut allows, mut denies) = (false, false);
        for pattern in &flags.globs {
            match pattern.strip_prefix('!') {
                Some(negated) => {
                    if let Ok(glob) = globset::Glob::new(negated) {
                        deny.add(glob);
                        denies = true;
                    }
                }
                None => {
                    if let Ok(glob) = globset::Glob::new(pattern) {
                        allow.add(glob);
                        allows = true;
                    }
                }
            }
        }
        Self {
            extensions,
            allow: allows.then(|| allow.build().ok()).flatten(),
            deny: denies.then(|| deny.build().ok()).flatten(),
            prefixes,
        }
    }

    pub fn keeps(&self, path: &str) -> bool {
        if !self.extensions.is_empty() {
            let name = path.rsplit('/').next().unwrap_or(path);
            let extension = name.rsplit_once('.').map(|(_, e)| e).unwrap_or("");
            if !self.extensions.iter().any(|want| want == extension) {
                return false;
            }
        }
        if self.allow.as_ref().is_some_and(|set| !set.is_match(path)) {
            return false;
        }
        if self.deny.as_ref().is_some_and(|set| set.is_match(path)) {
            return false;
        }
        if !self.prefixes.is_empty()
            && !self
                .prefixes
                .iter()
                .any(|prefix| path == prefix || path.starts_with(&format!("{prefix}/")))
        {
            return false;
        }
        true
    }
}

/// Resolve `..` and `.` without touching the filesystem.
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for part in path.components() {
        match part {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Path arguments as repo-relative paths, and the ones that had to be dropped.
///
/// Everything this command prints is repo-relative, because that is the only frame the
/// index shares. A path argument that resolves outside the repository — `../other`, or
/// an absolute path elsewhere — has no repo-relative spelling at all, so it is dropped
/// and named rather than quietly searched under a root it does not belong to.
pub fn relative_paths(root: &Path, cwd: &Path, paths: &[String]) -> (Vec<String>, Vec<String>) {
    let root = normalize(root);
    let mut kept = Vec::new();
    let mut dropped = Vec::new();
    for path in paths {
        let full = normalize(&cwd.join(path));
        match full.strip_prefix(&root) {
            Ok(relative) if !relative.as_os_str().is_empty() => {
                kept.push(relative.to_string_lossy().into_owned());
            }
            // The repository root itself: the same as naming no path at all.
            Ok(_) => {}
            Err(_) => dropped.push(path.clone()),
        }
    }
    (kept, dropped)
}

// ---- the local rg pass -------------------------------------------------------------

/// One line of output: a match, or a context line around one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Line {
    pub path: String,
    pub line: i64,
    pub text: String,
    pub context: bool,
}

/// What a local rg run produced.
#[derive(Debug, Clone, Default)]
pub struct Local {
    pub lines: Vec<Line>,
    pub files: Vec<String>,
    /// Files rg could only say "binary file matches" about. They are hits — dropping
    /// them turns a found match into exit 1.
    pub binary: Vec<String>,
    /// Why rg could not be used, when it could not.
    pub failed: Option<String>,
    /// Flags rg refused, which the retry then ran without.
    pub rejected: Vec<String>,
    /// True when the output hit [`RG_MAX_BYTES`].
    pub capped: bool,
}

/// The argument list handed to the real rg.
///
/// `--null` is what makes the answer parseable: without it a path containing a colon
/// is indistinguishable from the `path:line:` separator, and paths like that exist.
/// Unmodelled flags go in ahead of `--`, so `-F`, `-v` and `-w` keep meaning what rg
/// says they mean.
pub fn rg_args(flags: &Flags, paths: &[String], forward: bool) -> Vec<String> {
    let mut args: Vec<String> = vec!["--null".into(), "--color=never".into()];
    if flags.files_only {
        args.push("--files-with-matches".into());
    } else {
        args.push("--no-heading".into());
        args.push("--line-number".into());
        args.push("--with-filename".into());
    }
    if flags.ignore_case {
        args.push("--ignore-case".into());
    }
    if let Some(n) = &flags.after {
        args.push(format!("--after-context={n}"));
    }
    if let Some(n) = &flags.before {
        args.push(format!("--before-context={n}"));
    }
    for name in &flags.types {
        args.push(format!("--type={name}"));
    }
    for glob in &flags.globs {
        args.push(format!("--glob={glob}"));
    }
    if forward {
        args.extend(flags.ignored.iter().cloned());
    }
    args.push("--".into());
    args.push(flags.pattern.clone().unwrap_or_default());
    args.extend(paths.iter().cloned());
    args
}

/// Read rg's `--null` output.
pub fn parse_rg(stdout: &str, files_only: bool) -> (Vec<Line>, Vec<String>, Vec<String>) {
    if files_only {
        let mut files = Vec::new();
        let mut binary = Vec::new();
        for entry in stdout.split('\0') {
            let entry = entry.trim_matches('\n');
            if entry.is_empty() {
                continue;
            }
            match binary_match(entry) {
                Some(path) => binary.push(path),
                None => files.push(entry.to_owned()),
            }
        }
        return (Vec::new(), files, binary);
    }
    let mut lines = Vec::new();
    let mut binary = Vec::new();
    for record in stdout.lines() {
        // rg separates context groups with a bare `--`.
        if record == "--" {
            continue;
        }
        let Some((path, rest)) = record.split_once('\0') else {
            // No NUL: rg's "binary file matches" note, which carries no line number.
            if let Some(path) = binary_match(record) {
                binary.push(path);
            }
            continue;
        };
        let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
        if digits.is_empty() || rest.len() <= digits.len() {
            continue;
        }
        let Ok(number) = digits.parse::<i64>() else { continue };
        let separator = rest[digits.len()..].chars().next();
        lines.push(Line {
            path: path.to_owned(),
            line: number,
            text: rest[digits.len() + 1..].to_owned(),
            context: separator == Some('-'),
        });
    }
    (lines, Vec::new(), binary)
}

/// `<path>: binary file matches (…)` — a hit with no line to show for it.
fn binary_match(record: &str) -> Option<String> {
    let (path, _) = record.split_once(": binary file matches")?;
    Some(path.to_owned())
}

/// One rg process, read to a byte cap and killed at a deadline.
struct RgRun {
    stdout: String,
    stderr: String,
    code: i32,
    capped: bool,
    timed_out: bool,
}

/// Read a pipe up to `cap` bytes, saying whether there was more.
fn read_capped(mut pipe: impl Read, cap: usize, hit: &AtomicBool) -> (Vec<u8>, bool) {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 64 * 1024];
    loop {
        match pipe.read(&mut chunk) {
            Ok(0) | Err(_) => return (buffer, false),
            Ok(n) => {
                buffer.extend_from_slice(&chunk[..n]);
                if buffer.len() >= cap {
                    buffer.truncate(cap);
                    hit.store(true, Ordering::SeqCst);
                    return (buffer, true);
                }
            }
        }
    }
}

/// Spawn rg, read both pipes on threads, and never wait longer than [`RG_TIMEOUT`].
fn spawn_rg(root: &Path, args: &[String]) -> std::io::Result<RgRun> {
    let mut child = Command::new(rg_bin())
        .args(args)
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let out = child.stdout.take().expect("stdout is piped");
    let err = child.stderr.take().expect("stderr is piped");
    // Both pipes are drained on their own threads: a child that fills one while we
    // wait on the other deadlocks, and that is the classic way to hang a CLI.
    let capped = Arc::new(AtomicBool::new(false));
    let reader = capped.clone();
    let out_thread = std::thread::spawn(move || read_capped(out, RG_MAX_BYTES, &reader));
    let idle = Arc::new(AtomicBool::new(false));
    let err_thread = std::thread::spawn(move || read_capped(err, 64 * 1024, &idle));

    let deadline = Instant::now() + RG_TIMEOUT;
    let mut timed_out = false;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        // Two reasons to stop early: we have all the output we will read, or we have
        // waited long enough. Both end the same way — kill the child, keep what we
        // have — because a pipe nobody is draining will never close on its own.
        if capped.load(Ordering::SeqCst) || Instant::now() >= deadline {
            timed_out = !capped.load(Ordering::SeqCst);
            let _ = child.kill();
            break child.wait()?;
        }
        std::thread::sleep(Duration::from_millis(10));
    };

    let (stdout, capped_out) = out_thread.join().unwrap_or_default();
    let (stderr, _) = err_thread.join().unwrap_or_default();
    Ok(RgRun {
        stdout: String::from_utf8_lossy(&stdout).into_owned(),
        stderr: String::from_utf8_lossy(&stderr).into_owned(),
        code: status.code().unwrap_or(if timed_out { 2 } else { 1 }),
        capped: capped_out,
        timed_out,
    })
}

/// Run the real rg inside a checkout.
///
/// The working directory is the repository root, always, and the path arguments have
/// already been rewritten to match. Every path in the answer is then repo-relative,
/// which is the only frame the index shares — one search must not print two kinds of
/// path.
fn run_rg(root: &Path, flags: &Flags, paths: &[String]) -> Local {
    let forward = !flags.ignored.is_empty();
    let run = match spawn_rg(root, &rg_args(flags, paths, forward)) {
        Ok(run) => run,
        Err(error) => {
            return Local { failed: Some(format!("rg did not run: {error}")), ..Default::default() };
        }
    };
    // rg exits 0 with matches, 1 with none, 2 and up on a real failure. A failure
    // while carrying flags this command does not model is most likely one of those
    // flags, so try once more without them rather than losing the whole local pass.
    let (run, rejected) = match (run.code >= 2, forward, run.timed_out) {
        (true, true, false) => match spawn_rg(root, &rg_args(flags, paths, false)) {
            Ok(second) if second.code < 2 => (second, flags.ignored.clone()),
            _ => (run, Vec::new()),
        },
        _ => (run, Vec::new()),
    };

    if run.timed_out {
        return Local {
            failed: Some(format!("rg did not finish within {}s", RG_TIMEOUT.as_secs())),
            ..Default::default()
        };
    }
    if run.code >= 2 {
        let why = run.stderr.lines().next().unwrap_or("rg failed").to_owned();
        return Local { failed: Some(why), ..Default::default() };
    }
    let (lines, files, binary) = parse_rg(&run.stdout, flags.files_only);
    Local { lines, files, binary, failed: None, rejected, capped: run.capped }
}

// ---- the index pass ----------------------------------------------------------------

/// A definition, with what the graph knows about its use.
#[derive(Debug, Clone, PartialEq)]
pub struct Definition {
    pub path: String,
    pub line: i64,
    pub text: String,
    pub name: String,
    pub kind: String,
    pub references: i64,
    pub callers: i64,
}

/// One semantic hit.
#[derive(Debug, Clone, PartialEq)]
pub struct Semantic {
    pub path: String,
    pub line: i64,
    pub text: String,
    pub score: f64,
}

/// What `GET /:repo/code/find` answered.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Index {
    pub indexed: bool,
    pub commit: Option<String>,
    pub age_seconds: Option<i64>,
    pub definitions: Vec<Definition>,
    pub text: Vec<Line>,
    pub semantic: Vec<Semantic>,
    /// The server hit its own row cap.
    pub truncated: bool,
    /// The server's word on an empty answer, and on a missing model.
    pub hint: Option<String>,
    pub semantic_note: Option<String>,
    /// Why there is no index answer at all, when there is none.
    pub unreachable: Option<String>,
}

/// Read the viewer's answer into the layers this command prints.
///
/// The filter is applied again here, not because the server did not — it did, before
/// spending its budget — but because an older viewer will not know the parameters, and
/// a hit outside `-t rust` must not appear either way.
pub fn read_find(answer: &Value, filter: &PathFilter) -> Index {
    let mut index = Index {
        indexed: answer["indexed"].as_bool().unwrap_or(false),
        commit: answer["commit"].as_str().map(str::to_owned),
        age_seconds: answer["age_seconds"].as_i64(),
        truncated: answer["truncated"].as_bool().unwrap_or(false),
        hint: answer["hint"].as_str().map(str::to_owned),
        semantic_note: answer["semantic_note"].as_str().map(str::to_owned),
        ..Default::default()
    };
    for hit in answer["hits"].as_array().map(Vec::as_slice).unwrap_or_default() {
        let path = hit["path"].as_str().unwrap_or_default().to_owned();
        if path.is_empty() || !filter.keeps(&path) {
            continue;
        }
        let line = hit["line"].as_i64().unwrap_or(0);
        let text = hit["text"].as_str().unwrap_or_default().to_owned();
        match hit["layer"].as_str().unwrap_or_default() {
            "definition" => index.definitions.push(Definition {
                path,
                line,
                text,
                name: hit["name"].as_str().unwrap_or_default().to_owned(),
                kind: hit["kind"].as_str().unwrap_or("symbol").to_owned(),
                references: hit["references"].as_i64().unwrap_or(0),
                callers: hit["callers"].as_i64().unwrap_or(0),
            }),
            "text" => index.text.push(Line { path, line, text, context: false }),
            "semantic" => index.semantic.push(Semantic {
                path,
                line,
                text,
                score: hit["score"].as_f64().unwrap_or(0.0),
            }),
            // References are already counted on the definitions they belong to;
            // printing them again would be the same fact twice.
            _ => {}
        }
    }
    index
}

/// `GET <viewer>/<repo>/code/find?…` — the query, the cap, and the same narrowing the
/// local pass runs under, so the server spends its row budget inside the filter.
pub fn find_url(viewer: &str, repo: &str, flags: &Flags, paths: &[String]) -> String {
    let pct = crate::commands::plan::pct;
    let mut url = format!(
        "{}/{}/code/find?q={}&limit={LIMIT}",
        viewer.trim_end_matches('/'),
        pct(repo),
        pct(flags.pattern.as_deref().unwrap_or_default()),
    );
    if flags.ignore_case {
        url.push_str("&case=insensitive");
    }
    // Newline-separated, because a glob may contain a comma (`{a,b}` is one pattern)
    // and the viewer's query parser cannot decode a repeated key into a list.
    let list = |entries: &[String]| pct(&entries.join("\n"));
    if !flags.types.is_empty() {
        url.push_str(&format!("&types={}", list(&flags.types)));
    }
    if !flags.globs.is_empty() {
        url.push_str(&format!("&globs={}", list(&flags.globs)));
    }
    if !paths.is_empty() {
        url.push_str(&format!("&paths={}", list(paths)));
    }
    url
}

// ---- the merged answer -------------------------------------------------------------

/// Everything one search found, and where each half came from.
#[derive(Debug, Clone, Default)]
pub struct Report {
    pub pattern: String,
    pub repo: Option<String>,
    pub index: Index,
    pub text: Vec<Line>,
    pub binary: Vec<String>,
    pub files_only: bool,
    /// `rg`, `index`, or `none`.
    pub text_source: &'static str,
    pub ignored: Vec<String>,
    pub dropped_paths: Vec<String>,
    /// Whatever the local pass has to say for itself, already in `#` form.
    pub local_notes: Vec<String>,
}

impl Report {
    /// The semantic layer, which is printed only when the text pass found nothing.
    ///
    /// The index offers semantic hits whenever *its* text pass came back thin; the
    /// command is stricter, because a search that found the word does not need a guess
    /// at what the word means.
    pub fn semantic(&self) -> &[Semantic] {
        if self.text.is_empty() && self.binary.is_empty() {
            &self.index.semantic
        } else {
            &[]
        }
    }

    pub fn hits(&self) -> usize {
        self.index.definitions.len() + self.text.len() + self.binary.len() + self.semantic().len()
    }

    /// Every distinct path, definitions first: what `-l` prints.
    pub fn files(&self) -> Vec<String> {
        let mut seen: Vec<String> = Vec::new();
        let mut push = |path: &str| {
            if !seen.iter().any(|p| p == path) {
                seen.push(path.to_owned());
            }
        };
        for definition in &self.index.definitions {
            push(&definition.path);
        }
        for line in &self.text {
            push(&line.path);
        }
        for path in &self.binary {
            push(path);
        }
        for hit in self.semantic() {
            push(&hit.path);
        }
        seen
    }
}

/// `fn`, not `function`: this is a comment on a grep line, not a type system.
fn short_kind(kind: &str) -> &str {
    match kind {
        "function" => "fn",
        other => other,
    }
}

fn plural(n: i64, word: &str) -> String {
    if n == 1 { format!("{n} {word}") } else { format!("{n} {word}s") }
}

/// A path safe to print at the start of a grep line.
///
/// A file really called `#notes.md` would otherwise print a line every reader takes
/// for a comment. `./` in front settles it, and is still a path that resolves.
fn safe_path(path: &str) -> String {
    if path.starts_with('#') { format!("./{path}") } else { path.to_owned() }
}

/// What one search prints, split by stream.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Rendered {
    /// stdout: hits, and the `#` comments that belong beside them.
    pub out: Vec<String>,
    /// stderr: the same comments, when `-l` means stdout must be paths and nothing
    /// else, so `nashcode grep -l x | xargs …` works.
    pub notes: Vec<String>,
}

/// The whole answer as text: grep's lines, with the index's extras in `#` comments.
pub fn render(report: &Report) -> Rendered {
    let mut comments = Vec::new();

    match &report.index.unreachable {
        Some(why) => comments.push(format!("# index unreachable: {why}")),
        None if report.index.indexed => {
            let commit: String = report
                .index
                .commit
                .as_deref()
                .unwrap_or("unknown")
                .chars()
                .take(TIP_LEN)
                .collect();
            let age = report
                .index
                .age_seconds
                .map(ago)
                .unwrap_or_else(|| "age unknown".to_owned());
            comments.push(format!("# index: {commit} ({age})"));
        }
        None => comments.push("# index: not built yet".to_owned()),
    }
    comments.extend(report.local_notes.iter().cloned());
    if report.index.truncated {
        comments.push("# index answer truncated at the row cap; narrow with -t/-g or a path".into());
    }
    for path in &report.binary {
        comments.push(format!("# binary file matches: {}", safe_path(path)));
    }
    if !report.dropped_paths.is_empty() {
        comments.push(format!(
            "# path outside the repository, not searched: {}",
            report.dropped_paths.join(" ")
        ));
    }

    let mut out = Vec::new();
    let mut notes = Vec::new();

    if report.files_only {
        // stdout is a pure path list here, so every comment moves to stderr.
        notes.extend(comments);
        if !report.index.definitions.is_empty() {
            notes.push("# definitions:".to_owned());
        }
        if !report.semantic().is_empty() {
            notes.push("# semantic (no exact match):".to_owned());
        }
        out.extend(report.files().iter().map(|path| safe_path(path)));
        if report.hits() == 0 {
            notes.extend(empty_notes(report));
        }
        return Rendered { out, notes };
    }

    out.extend(comments);
    if !report.index.definitions.is_empty() {
        out.push("# definitions:".to_owned());
        for definition in &report.index.definitions {
            // The one annotated hit format in the contract: a parser that wants raw
            // content strips from the last ` # `.
            out.push(format!(
                "{}:{}:{} # {}, {}, {}",
                safe_path(&definition.path),
                definition.line,
                definition.text,
                short_kind(&definition.kind),
                plural(definition.references, "ref"),
                plural(definition.callers, "caller"),
            ));
        }
    }

    for line in &report.text {
        // grep's own two shapes: `:` for a match, `-` for a context line.
        let separator = if line.context { '-' } else { ':' };
        out.push(format!(
            "{}{separator}{}{separator}{}",
            safe_path(&line.path),
            line.line,
            line.text
        ));
    }

    if !report.semantic().is_empty() {
        out.push("# semantic (no exact match):".to_owned());
        for hit in report.semantic() {
            out.push(format!("{}:{}:{}", safe_path(&hit.path), hit.line, hit.text));
        }
    }

    if report.hits() == 0 {
        out.extend(empty_notes(report));
    }
    Rendered { out, notes }
}

/// What the server had to say about an answer with nothing in it. Only printed when
/// there is nothing else, where it is the difference between "not there" and "the
/// model never loaded".
fn empty_notes(report: &Report) -> Vec<String> {
    let mut notes = Vec::new();
    if let Some(hint) = &report.index.hint {
        notes.push(format!("# {hint}"));
    }
    if let Some(note) = &report.index.semantic_note {
        notes.push(format!("# semantic search unavailable: {note}"));
    }
    notes
}

/// The same answer as one JSON object.
pub fn as_json(report: &Report) -> Value {
    let mut index = json!({ "indexed": report.index.indexed, "source": report.text_source });
    if let Some(why) = &report.index.unreachable {
        index["reachable"] = json!(false);
        index["error"] = json!(why);
    } else {
        index["reachable"] = json!(true);
    }
    if let Some(commit) = &report.index.commit {
        index["commit"] = json!(commit);
        index["commit_short"] = json!(commit.chars().take(TIP_LEN).collect::<String>());
    }
    if let Some(age) = report.index.age_seconds {
        index["age_seconds"] = json!(age);
        index["age"] = json!(ago(age));
    }
    if report.index.truncated {
        index["truncated"] = json!(true);
    }
    if let Some(hint) = &report.index.hint {
        index["hint"] = json!(hint);
    }
    if let Some(note) = &report.index.semantic_note {
        index["semantic_note"] = json!(note);
    }

    let mut value = json!({
        "ok": true,
        "pattern": report.pattern,
        "repo": report.repo,
        "index": index,
        "definitions": report
            .index
            .definitions
            .iter()
            .map(|d| json!({
                "path": d.path,
                "line": d.line,
                "text": d.text,
                "name": d.name,
                "kind": d.kind,
                "references": d.references,
                "callers": d.callers,
            }))
            .collect::<Vec<_>>(),
        "semantic": report
            .semantic()
            .iter()
            .map(|s| json!({
                "path": s.path,
                "line": s.line,
                "text": s.text,
                "score": s.score,
            }))
            .collect::<Vec<_>>(),
        "hits": report.hits(),
        "ignored_flags": report.ignored,
    });

    // `-l` asked for paths, so the machine shape is paths too — not text rows with a
    // line number of zero and nothing in them.
    if report.files_only {
        value["files"] = json!(report.files());
    } else {
        value["text"] = json!(
            report
                .text
                .iter()
                .map(|l| json!({
                    "path": l.path,
                    "line": l.line,
                    "text": l.text,
                    "context": l.context,
                }))
                .collect::<Vec<_>>()
        );
    }
    if !report.binary.is_empty() {
        value["binary"] = json!(report.binary);
    }
    if !report.dropped_paths.is_empty() {
        value["dropped_paths"] = json!(report.dropped_paths);
    }
    if !report.local_notes.is_empty() {
        value["notes"] = json!(report.local_notes);
    }
    value
}

// ---- the command -------------------------------------------------------------------

/// `nashcode grep [flags] PATTERN [path...]`.
///
/// Exits the way grep does — 0 with hits, 1 without — and 2 only for a usage mistake or
/// for having neither a working tree to search nor an index to ask.
pub fn run(args: &[String]) -> i32 {
    let before = preceding_flags();
    let flags = parse(args);
    // Either side of the command name works for all three. A flag typed after
    // it is the more specific spelling, so it wins.
    let wants_json = flags.json || before.json;
    let quiet = flags.quiet || before.quiet;
    let ctx = Ctx {
        out: Out::new(quiet),
        profile_name: flags.profile.clone().or(before.profile),
    };

    if flags.help {
        print_help(&mut std::io::stdout());
        return 0;
    }
    if flags.pattern.is_none() {
        return usage(
            wants_json,
            "no pattern: nashcode grep [flags] PATTERN [path...]",
            "nashcode grep --help",
        );
    }
    let pattern = flags.pattern.clone().unwrap_or_default();

    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let workspace = vcs::detect_cwd().ok().flatten();
    let root = workspace.as_ref().map(|ws| ws.root.clone());

    // Path arguments become repo-relative once, for both halves.
    let (paths, dropped_paths) = match &root {
        Some(root) => relative_paths(root, &cwd, &flags.paths),
        None => (Vec::new(), Vec::new()),
    };

    let local = root.as_ref().map(|root| run_rg(root, &flags, &paths));
    let filter = PathFilter::new(&flags, paths.clone());
    let repo = flags.repo.clone().or_else(|| {
        workspace
            .as_ref()
            .and_then(|ws| ws.origin_repo_name().ok().flatten().or_else(|| ws.default_repo_name()))
    });
    let index = ask_index(&ctx, repo.as_deref(), &flags, &paths, &filter);

    // Text comes from the tree when there is one, and from the index otherwise. Never
    // both: the same line from two sources is the same line twice.
    let usable = local.as_ref().filter(|local| local.failed.is_none());
    let (text, binary, text_source) = match usable {
        Some(local) if flags.files_only => (
            local
                .files
                .iter()
                .map(|path| Line {
                    path: path.clone(),
                    line: 0,
                    text: String::new(),
                    context: false,
                })
                .collect(),
            local.binary.clone(),
            "rg",
        ),
        Some(local) => (local.lines.clone(), local.binary.clone(), "rg"),
        None if index.indexed => (index.text.clone(), Vec::new(), "index"),
        None => (Vec::new(), Vec::new(), "none"),
    };

    // Whatever the local pass had to say, said once, wherever the answer came from.
    let mut local_notes = Vec::new();
    if let Some(local) = &local {
        if let Some(why) = &local.failed {
            local_notes.push(format!("# local rg failed: {}", one_line(why)));
        }
        if !local.rejected.is_empty() {
            local_notes.push(format!(
                "# local rg rejected {}; searched without them",
                local.rejected.join(" ")
            ));
        }
        if local.capped {
            local_notes.push(format!("# local rg output capped at {RG_MAX_BYTES} bytes"));
        }
    }

    let report = Report {
        pattern,
        repo,
        index,
        text,
        binary,
        files_only: flags.files_only,
        text_source,
        ignored: flags.ignored.clone(),
        dropped_paths,
        local_notes,
    };

    // Neither half available is the one real failure. Everything else is an answer.
    if report.text_source == "none" && !report.index.indexed {
        let why = local
            .and_then(|local| local.failed)
            .unwrap_or_else(|| "not inside a checkout, and no rg to search one".to_owned());
        let index_why = report
            .index
            .unreachable
            .clone()
            .unwrap_or_else(|| "the repository has no index".to_owned());
        return fail(
            wants_json,
            &format!("{why}; {index_why}"),
            "nashcode index <repo>",
        );
    }

    let rendered = render(&report);
    if !quiet {
        for note in &rendered.notes {
            eprintln!("{note}");
        }
    }
    if wants_json {
        print_value(&as_json(&report));
    } else {
        for line in &rendered.out {
            println!("{line}");
        }
    }
    i32::from(report.hits() == 0)
}

/// One JSON value on stdout, the way every other nashcode command answers —
/// except that grep builds it itself, because it owns its stdout.
fn print_value(value: &Value) {
    println!("{}", serde_json::to_string_pretty(value).unwrap_or_default());
}

/// A usage mistake: help where a person can read it, an envelope where a machine can.
fn usage(wants_json: bool, why: &str, fix: &str) -> i32 {
    if wants_json {
        print_value(&json!({ "ok": false, "error": why, "fix": fix }));
    } else {
        eprintln!("nashcode grep: {why}");
        print_help(&mut std::io::stderr());
    }
    2
}

/// The one real failure: nothing to search and nothing to ask.
fn fail(wants_json: bool, why: &str, fix: &str) -> i32 {
    if wants_json {
        print_value(&json!({ "ok": false, "error": why, "fix": fix }));
    } else {
        eprintln!("nashcode grep: {why}");
        eprintln!("nashcode grep: fix: {fix}");
    }
    2
}

/// Ask the viewer, and turn every way that can fail into one comment line.
fn ask_index(
    ctx: &Ctx,
    repo: Option<&str>,
    flags: &Flags,
    paths: &[String],
    filter: &PathFilter,
) -> Index {
    let unreachable = |why: String| Index { unreachable: Some(why), ..Default::default() };

    let (viewer, token) = match crate::commands::brain::viewer_url(ctx) {
        Ok(pair) => pair,
        Err(why) => return unreachable(why),
    };
    let Some(repo) = repo else {
        return unreachable("cannot tell which repository this is; name one with --repo".to_owned());
    };

    let url = find_url(&viewer, repo, flags, paths);
    let client = Client::with_timeout(&viewer, &token, TIMEOUT);
    let reply = match client.get_json(&url) {
        Ok(reply) => reply,
        Err(error) => return unreachable(one_line(&format!("{error:#}"))),
    };
    if !reply.ok() {
        return unreachable(format!("{url} returned HTTP {}", reply.status));
    }
    match serde_json::from_str::<Value>(&reply.body) {
        Ok(answer) => read_find(&answer, filter),
        Err(error) => unreachable(format!("the viewer's answer is not JSON: {error}")),
    }
}

fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The framework never parses grep's arguments, so it can never print grep's
/// help either: `--help` reaches this handler like any other flag.
fn print_help(to: &mut impl std::io::Write) {
    let _ = writeln!(
        to,
        "Usage: nashcode grep [rg-flags...] <pattern> [path...]\n\n{}",
        crate::cli::GREP_DOC
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(args: &[&str]) -> Flags {
        parse(&args.iter().map(|a| (*a).to_string()).collect::<Vec<_>>())
    }

    #[test]
    fn the_pattern_is_the_first_thing_that_is_not_a_flag() {
        let flags = parsed(&["-i", "retry", "src/", "docs/"]);
        assert_eq!(flags.pattern.as_deref(), Some("retry"));
        assert_eq!(flags.paths, ["src/", "docs/"]);
        assert!(flags.ignore_case);
    }

    #[test]
    fn unknown_flags_are_kept_and_never_refused() {
        let flags = parsed(&["--no-heading", "-S", "--hidden", "retry"]);
        assert_eq!(flags.pattern.as_deref(), Some("retry"));
        // `--no-heading` already describes what this command does, so it is honoured
        // rather than kept; the rest are kept, in order, for the local rg run.
        assert_eq!(flags.ignored, ["-S", "--hidden"]);
        assert!(flags.paths.is_empty());
    }

    #[test]
    fn an_unknown_flag_that_takes_a_value_does_not_steal_the_pattern() {
        for spelling in [
            vec!["--color", "never", "retry"],
            vec!["--color=never", "retry"],
            vec!["--max-count", "3", "retry"],
            vec!["-m", "3", "retry"],
            vec!["-m3", "retry"],
            vec!["-j", "4", "retry"],
        ] {
            let flags = parsed(&spelling);
            assert_eq!(flags.pattern.as_deref(), Some("retry"), "{spelling:?}");
            assert!(flags.paths.is_empty(), "{spelling:?} left {:?}", flags.paths);
        }
        // The value travels with the flag, so rg still sees the pair.
        assert_eq!(parsed(&["--color", "never", "x"]).ignored, ["--color", "never"]);
        assert_eq!(parsed(&["-m3", "x"]).ignored, ["-m", "3"]);
    }

    #[test]
    fn a_flag_that_changes_the_meaning_of_the_search_is_forwarded_to_rg() {
        let flags = parsed(&["-F", "-v", "retry"]);
        assert_eq!(flags.ignored, ["-F", "-v"]);
        let args = rg_args(&flags, &[], true);
        let end = args.iter().position(|a| a == "--").expect("a -- separator");
        assert!(args[..end].contains(&"-F".to_string()), "{args:?}");
        assert!(args[..end].contains(&"-v".to_string()), "{args:?}");
        // And the retry runs without them.
        let plain = rg_args(&flags, &[], false);
        assert!(!plain.contains(&"-F".to_string()), "{plain:?}");
    }

    #[test]
    fn values_attach_the_way_rg_attaches_them() {
        for spelling in [
            vec!["-C3", "x"],
            vec!["-C", "3", "x"],
            vec!["-C=3", "x"],
            vec!["--context=3", "x"],
            vec!["--context", "3", "x"],
        ] {
            let flags = parsed(&spelling);
            assert_eq!(flags.after.as_deref(), Some("3"), "{spelling:?}");
            assert_eq!(flags.before.as_deref(), Some("3"), "{spelling:?}");
            assert_eq!(flags.pattern.as_deref(), Some("x"), "{spelling:?}");
        }
        assert_eq!(parsed(&["-trust", "x"]).types, ["rust"]);
        assert_eq!(parsed(&["-t", "rust", "x"]).types, ["rust"]);
        assert_eq!(parsed(&["-g", "*.rs", "x"]).globs, ["*.rs"]);
        assert_eq!(parsed(&["-A", "2", "-B", "1", "x"]).after.as_deref(), Some("2"));
        assert_eq!(parsed(&["-A", "2", "-B", "1", "x"]).before.as_deref(), Some("1"));
    }

    #[test]
    fn a_cluster_of_short_flags_is_read_letter_by_letter() {
        let flags = parsed(&["-il", "retry"]);
        assert!(flags.ignore_case);
        assert!(flags.files_only);
        assert_eq!(flags.pattern.as_deref(), Some("retry"));
    }

    #[test]
    fn a_double_dash_makes_everything_after_it_a_pattern_or_a_path() {
        let flags = parsed(&["--", "-Zthreads", "src/"]);
        assert_eq!(flags.pattern.as_deref(), Some("-Zthreads"));
        assert_eq!(flags.paths, ["src/"]);
        assert!(flags.ignored.is_empty());
    }

    #[test]
    fn nashcodes_own_flags_are_read_here_not_forwarded_to_rg() {
        // The framework hands this command its argv unparsed, so the three
        // flags that are nashcode's rather than rg's are read right here.
        let flags = parsed(&["--json", "retry"]);
        assert!(flags.json);
        assert_eq!(flags.pattern.as_deref(), Some("retry"));

        // --profile takes a value: without this arm `work` became the pattern
        // and `--profile` was forwarded to an rg that has no such flag.
        for spelling in [
            vec!["--profile", "work", "retry"],
            vec!["--profile=work", "retry"],
        ] {
            let flags = parsed(&spelling);
            assert_eq!(flags.profile.as_deref(), Some("work"), "{spelling:?}");
            assert_eq!(flags.pattern.as_deref(), Some("retry"), "{spelling:?}");
            assert!(flags.ignored.is_empty(), "{:?}", flags.ignored);
        }

        assert!(parsed(&["--quiet", "retry"]).quiet);
    }

    #[test]
    fn the_same_flags_are_read_when_typed_before_the_subcommand() {
        let argv = ["--profile", "work", "--json", "--quiet", "grep", "--profile", "x"];
        let before = read_preceding(argv.iter().map(|a| (*a).to_string()));
        assert_eq!(before.profile.as_deref(), Some("work"));
        assert!(before.json && before.quiet);

        // The scan stops at the subcommand, so grep's own tokens are not eaten.
        let argv = ["grep", "--profile", "after"];
        let before = read_preceding(argv.iter().map(|a| (*a).to_string()));
        assert_eq!(before.profile, None);
    }

    #[test]
    fn rg_gets_the_flags_that_matter_and_a_parseable_output_shape() {
        let flags = parsed(&["-i", "-C2", "-t", "rust", "-g", "!*.lock", "retry"]);
        let args = rg_args(&flags, &["src".to_string()], true);
        assert!(args.contains(&"--null".to_string()), "{args:?}");
        assert!(args.contains(&"--line-number".to_string()), "{args:?}");
        assert!(args.contains(&"--ignore-case".to_string()), "{args:?}");
        assert!(args.contains(&"--after-context=2".to_string()), "{args:?}");
        assert!(args.contains(&"--before-context=2".to_string()), "{args:?}");
        assert!(args.contains(&"--type=rust".to_string()), "{args:?}");
        assert!(args.contains(&"--glob=!*.lock".to_string()), "{args:?}");
        // The pattern always sits behind `--`, so a pattern starting with `-` works.
        let end = args.iter().position(|a| a == "--").expect("a -- separator");
        assert_eq!(args[end + 1], "retry");
        assert_eq!(args[end + 2], "src");
    }

    #[test]
    fn rgs_null_output_parses_into_matches_and_context_lines() {
        let stdout = "src/net.rs\u{0}7-\nsrc/net.rs\u{0}8:pub fn retry() {\n--\ndocs/a:b.md\u{0}2:retry\n";
        let (lines, files, binary) = parse_rg(stdout, false);
        assert!(files.is_empty() && binary.is_empty());
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0], Line { path: "src/net.rs".into(), line: 7, text: String::new(), context: true });
        assert_eq!(lines[1].text, "pub fn retry() {");
        assert!(!lines[1].context);
        // A colon in the path is why `--null` is there at all.
        assert_eq!(lines[2].path, "docs/a:b.md");
        assert_eq!(lines[2].line, 2);
    }

    #[test]
    fn a_binary_file_that_matches_is_a_hit_not_a_dropped_line() {
        let stdout = "bin.dat: binary file matches (found \"\\0\" byte around offset 6)\n";
        let (lines, _, binary) = parse_rg(stdout, false);
        assert!(lines.is_empty());
        assert_eq!(binary, ["bin.dat"]);

        let report = Report { binary: vec!["bin.dat".into()], ..Default::default() };
        assert_eq!(report.hits(), 1, "a binary match must not become exit 1");
        assert!(
            render(&report).out.iter().any(|l| l == "# binary file matches: bin.dat"),
            "{:?}",
            render(&report).out
        );
    }

    #[test]
    fn files_only_output_is_nul_separated_paths() {
        let (lines, files, _) = parse_rg("src/net.rs\u{0}docs/notes.md\u{0}", true);
        assert!(lines.is_empty());
        assert_eq!(files, ["src/net.rs", "docs/notes.md"]);
    }

    #[test]
    fn the_index_answer_becomes_three_layers_and_drops_the_fourth() {
        let answer = json!({
            "indexed": true,
            "commit": "a1b2c3d4e5f6",
            "age_seconds": 60,
            "truncated": true,
            "hits": [
                { "layer": "definition", "path": "src/net.rs", "line": 8, "text": "pub fn retry() {",
                  "name": "retry", "kind": "function", "references": 12, "callers": 3 },
                { "layer": "reference", "path": "src/app.rs", "line": 4, "text": "retry" },
                { "layer": "text", "path": "docs/notes.md", "line": 2, "text": "retry here" },
                { "layer": "semantic", "path": "src/wait.rs", "line": 1, "text": "fn sleep()", "score": 0.5 }
            ]
        });
        let index = read_find(&answer, &PathFilter::default());
        assert_eq!(index.definitions.len(), 1);
        assert_eq!(index.definitions[0].references, 12);
        assert_eq!(index.definitions[0].callers, 3);
        assert_eq!(index.text.len(), 1);
        assert_eq!(index.semantic.len(), 1);
        assert!(index.truncated, "a capped answer must never print as complete");
        // References are counted on the definition; printing them too says it twice.
        assert_eq!(index.definitions.len() + index.text.len() + index.semantic.len(), 3);
    }

    #[test]
    fn the_type_and_glob_filters_apply_to_the_index_side_too() {
        let answer = json!({
            "indexed": true,
            "hits": [
                { "layer": "text", "path": "src/net.rs", "line": 1, "text": "a" },
                { "layer": "text", "path": "docs/notes.md", "line": 1, "text": "b" },
                { "layer": "text", "path": "src/deep/inner.rs", "line": 1, "text": "c" }
            ]
        });
        let only_rust = read_find(&answer, &PathFilter::new(&parsed(&["-t", "rust", "x"]), vec![]));
        assert_eq!(only_rust.text.len(), 2);

        let by_glob = read_find(&answer, &PathFilter::new(&parsed(&["-g", "*.md", "x"]), vec![]));
        assert_eq!(by_glob.text.len(), 1);
        assert_eq!(by_glob.text[0].path, "docs/notes.md");

        let negated = read_find(&answer, &PathFilter::new(&parsed(&["-g", "!*.md", "x"]), vec![]));
        assert_eq!(negated.text.len(), 2);

        let under_path = read_find(&answer, &PathFilter::new(&parsed(&["x"]), vec!["src/deep".into()]));
        assert_eq!(under_path.text.len(), 1);
        assert_eq!(under_path.text[0].path, "src/deep/inner.rs");

        // An unknown type filters nothing rather than everything.
        let unknown = read_find(&answer, &PathFilter::new(&parsed(&["-t", "cobol", "x"]), vec![]));
        assert_eq!(unknown.text.len(), 3);
    }

    #[test]
    fn globs_mean_what_ripgrep_means_by_them() {
        let filter = PathFilter::new(&parsed(&["-g", "src/**/*.rs", "x"]), vec![]);
        assert!(filter.keeps("src/deep/inner.rs"));
        assert!(filter.keeps("src/net.rs"));
        assert!(!filter.keeps("docs/notes.md"));
        assert!(!filter.keeps("other/net.rs"));

        // Alternation and character classes are globset's, not escaped literals.
        let braces = PathFilter::new(&parsed(&["-g", "*.{rs,md}", "x"]), vec![]);
        assert!(braces.keeps("src/net.rs"));
        assert!(braces.keeps("docs/notes.md"));
        assert!(!braces.keeps("Cargo.toml"));

        let class = PathFilter::new(&parsed(&["-g", "src/[ab]*.rs", "x"]), vec![]);
        assert!(class.keeps("src/app.rs"));
        assert!(!class.keeps("src/net.rs"));
    }

    #[test]
    fn a_path_argument_outside_the_repository_is_dropped_and_named() {
        let root = Path::new("/repo");
        let cwd = Path::new("/repo/src");
        let (kept, dropped) = relative_paths(
            root,
            cwd,
            &["net.rs".into(), "../docs".into(), "../../etc".into(), "/elsewhere".into()],
        );
        assert_eq!(kept, ["src/net.rs", "docs"]);
        assert_eq!(dropped, ["../../etc", "/elsewhere"]);
    }

    fn report() -> Report {
        Report {
            pattern: "retry".into(),
            repo: Some("demo".into()),
            index: Index {
                indexed: true,
                commit: Some("a1b2c3d4e5f6".into()),
                age_seconds: Some(259_200),
                definitions: vec![Definition {
                    path: "src/net.rs".into(),
                    line: 8,
                    text: "pub fn retry() {".into(),
                    name: "retry".into(),
                    kind: "function".into(),
                    references: 12,
                    callers: 3,
                }],
                ..Default::default()
            },
            text: vec![
                Line { path: "src/net.rs".into(), line: 7, text: "// try again".into(), context: true },
                Line { path: "docs/notes.md".into(), line: 2, text: "retry here".into(), context: false },
            ],
            files_only: false,
            text_source: "rg",
            ..Default::default()
        }
    }

    #[test]
    fn the_output_is_greps_with_the_extras_in_comments() {
        let lines = render(&report()).out;
        assert_eq!(lines[0], "# index: a1b2c3d (3 days ago)");
        assert_eq!(lines[1], "# definitions:");
        assert_eq!(lines[2], "src/net.rs:8:pub fn retry() { # fn, 12 refs, 3 callers");
        assert_eq!(lines[3], "src/net.rs-7-// try again", "context uses grep's dash form");
        assert_eq!(lines[4], "docs/notes.md:2:retry here");
        // A definition line strips back to raw content at the last ` # `.
        let (content, _) = lines[2].rsplit_once(" # ").expect("one annotation");
        assert_eq!(content, "src/net.rs:8:pub fn retry() {");
        // Text and semantic lines stay pure.
        assert!(!lines[4].contains(" # "));
    }

    #[test]
    fn one_reference_and_one_caller_are_not_pluralised() {
        let mut report = report();
        report.index.definitions[0].references = 1;
        report.index.definitions[0].callers = 1;
        let lines = render(&report).out;
        assert!(lines[2].ends_with(" # fn, 1 ref, 1 caller"), "{:?}", lines[2]);
    }

    #[test]
    fn a_path_that_starts_with_a_hash_is_still_a_path() {
        let mut report = report();
        report.text[1].path = "#notes.md".into();
        let lines = render(&report).out;
        assert!(lines.contains(&"./#notes.md:2:retry here".to_string()), "{lines:?}");
    }

    #[test]
    fn the_semantic_block_appears_only_when_the_text_pass_found_nothing() {
        let mut report = report();
        report.index.semantic = vec![Semantic {
            path: "src/wait.rs".into(),
            line: 1,
            text: "fn sleep()".into(),
            score: 0.5,
        }];
        assert!(!render(&report).out.iter().any(|l| l.contains("semantic")));

        report.text.clear();
        let lines = render(&report).out;
        assert!(lines.contains(&"# semantic (no exact match):".to_string()), "{lines:?}");
        assert!(lines.contains(&"src/wait.rs:1:fn sleep()".to_string()), "{lines:?}");
    }

    #[test]
    fn a_dead_viewer_says_so_in_one_comment_line_and_prints_the_rest() {
        let mut report = report();
        report.index = Index {
            unreachable: Some("connection refused".into()),
            ..Default::default()
        };
        let lines = render(&report).out;
        assert_eq!(lines[0], "# index unreachable: connection refused");
        assert!(!lines.iter().any(|l| l.contains("definitions")));
        assert_eq!(lines[1], "src/net.rs-7-// try again");
    }

    #[test]
    fn a_local_pass_that_failed_says_so_rather_than_answering_from_the_index_in_silence() {
        let mut report = report();
        report.text_source = "index";
        report.local_notes = vec!["# local rg failed: unrecognized file type: cobol".into()];
        let lines = render(&report).out;
        assert_eq!(lines[1], "# local rg failed: unrecognized file type: cobol");
        assert_eq!(as_json(&report)["notes"][0], "# local rg failed: unrecognized file type: cobol");
    }

    #[test]
    fn a_capped_index_answer_never_prints_as_complete() {
        let mut report = report();
        report.index.truncated = true;
        assert!(
            render(&report).out.iter().any(|l| l.starts_with("# index answer truncated")),
            "{:?}",
            render(&report).out
        );
        assert_eq!(as_json(&report)["index"]["truncated"], true);
    }

    #[test]
    fn an_empty_answer_carries_the_servers_own_word_on_why() {
        let mut report = report();
        report.text.clear();
        report.index.definitions.clear();
        report.index.hint = Some("indexed at a1b2c3d, but nothing matches".into());
        report.index.semantic_note = Some("the model is not loaded yet".into());
        let lines = render(&report).out;
        assert!(lines.contains(&"# indexed at a1b2c3d, but nothing matches".to_string()), "{lines:?}");
        assert!(
            lines.iter().any(|l| l.starts_with("# semantic search unavailable")),
            "{lines:?}"
        );
    }

    #[test]
    fn files_only_puts_the_paths_on_stdout_and_every_comment_on_stderr() {
        let mut report = report();
        report.files_only = true;
        let rendered = render(&report);
        // stdout is a pure path list, so `nashcode grep -l x | xargs sed` works.
        assert_eq!(rendered.out, ["src/net.rs", "docs/notes.md"]);
        assert_eq!(rendered.notes[0], "# index: a1b2c3d (3 days ago)");
        assert!(rendered.notes.contains(&"# definitions:".to_string()));
    }

    #[test]
    fn files_only_json_is_a_file_list_not_empty_text_rows() {
        let mut report = report();
        report.files_only = true;
        let value = as_json(&report);
        assert_eq!(value["files"], json!(["src/net.rs", "docs/notes.md"]));
        assert!(value.get("text").is_none(), "no rows with line 0 and no content");
    }

    #[test]
    fn the_json_carries_the_same_layers_the_text_does() {
        let value = as_json(&report());
        assert_eq!(value["ok"], true);
        assert_eq!(value["pattern"], "retry");
        assert_eq!(value["repo"], "demo");
        assert_eq!(value["index"]["commit_short"], "a1b2c3d");
        assert_eq!(value["index"]["age"], "3 days ago");
        assert_eq!(value["index"]["source"], "rg");
        assert_eq!(value["definitions"][0]["kind"], "function");
        assert_eq!(value["definitions"][0]["references"], 12);
        assert_eq!(value["text"][0]["context"], true);
        assert_eq!(value["text"][1]["context"], false);
        assert_eq!(value["semantic"].as_array().unwrap().len(), 0);
        assert_eq!(value["hits"], 3);
    }

    #[test]
    fn the_url_carries_the_query_and_every_narrowing_the_local_pass_runs_under() {
        let flags = parsed(&["-i", "-t", "rust", "-g", "*.{rs,md}", "fn retry"]);
        let url = find_url("https://v/", "demo", &flags, &["src".to_string()]);
        assert!(url.starts_with("https://v/demo/code/find?q=fn%20retry&limit=100"), "{url}");
        assert!(url.contains("&case=insensitive"), "{url}");
        assert!(url.contains("&types=rust"), "{url}");
        // A glob with a comma in it survives, which is why the list is newline-joined.
        assert!(url.contains("&globs=%2A.%7Brs%2Cmd%7D"), "{url}");
        assert!(url.contains("&paths=src"), "{url}");

        // Nothing extra when nothing was asked for.
        let plain = parsed(&["retry"]);
        assert_eq!(
            find_url("https://v", "demo", &plain, &[]),
            "https://v/demo/code/find?q=retry&limit=100"
        );
    }
}
