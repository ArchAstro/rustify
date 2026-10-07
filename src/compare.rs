//! `rustify compare`: run the same cases through the TypeScript program and
//! the Rust one, and report where exit code, stdout, stderr, or the files
//! written differ. Cases live in `compare.toml` in the state directory.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use regex::Regex;
use serde::Deserialize;
use serde_json::json;
use walkdir::WalkDir;

use crate::config::Workspace;

pub const COMPARE_FILE: &str = "compare.toml";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Spec {
    /// Command line of the TypeScript program. `{root}` expands to the
    /// repository root; each case runs in a temporary directory, so paths
    /// must be absolute or use it.
    old: Vec<String>,
    /// Command line of the Rust program.
    new: Vec<String>,
    #[serde(default = "default_timeout")]
    timeout_secs: u64,
    /// Set for both programs. `{root}` and `{work}` (the case's working
    /// directory) expand in values.
    #[serde(default)]
    env: BTreeMap<String, String>,
    /// Applied to stdout, stderr, and text files before comparing.
    #[serde(default)]
    normalize: Vec<Rule>,
    #[serde(default, rename = "case")]
    cases: Vec<Case>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Rule {
    pattern: String,
    replace: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Case {
    name: String,
    #[serde(default)]
    args: Vec<String>,
    /// Directory copied into the working directory first, relative to the
    /// directory holding `compare.toml`.
    #[serde(default)]
    fixture: Option<String>,
    #[serde(default)]
    stdin: Option<String>,
    /// Why this case is allowed to differ. An accepted case that differs
    /// does not fail the run; one that no longer differs is reported.
    #[serde(default)]
    accept: Option<String>,
}

fn default_timeout() -> u64 {
    60
}

struct Observed {
    /// The exit code, `signal` when killed by one, or `timeout`.
    exit: String,
    stdout: String,
    stderr: String,
    files: BTreeMap<String, Left>,
}

/// What a run left at one path of its working directory.
#[derive(PartialEq)]
enum Left {
    Dir,
    /// Normalized link target.
    Symlink(String),
    /// Normalized contents of a UTF-8 file.
    Text(String),
    /// A file that is not UTF-8. Compared by presence only: two encoders
    /// rarely produce the same bytes for the same picture.
    Binary,
}

impl Left {
    fn kind(&self) -> String {
        match self {
            Left::Dir => "a directory".to_owned(),
            Left::Symlink(target) => format!("a symlink to {target}"),
            Left::Text(_) => "a text file".to_owned(),
            Left::Binary => "a binary file".to_owned(),
        }
    }
}

pub fn run(ws: &Workspace, only: &[String], keep: Option<&Path>, json: bool) -> Result<bool> {
    let path = ws.port_file(COMPARE_FILE);
    let text = std::fs::read_to_string(&path).with_context(|| {
        format!(
            "read {} (list the cases to compare there; see docs/workflow.md, \"Compare the two programs\")",
            path.display()
        )
    })?;
    let spec: Spec = toml::from_str(&text).with_context(|| format!("parse {}", path.display()))?;
    if spec.old.is_empty() || spec.new.is_empty() {
        bail!(
            "{}: `old` and `new` must each name a command",
            path.display()
        );
    }
    let rules = spec
        .normalize
        .iter()
        .map(|rule| {
            Regex::new(&rule.pattern)
                .map(|regex| (regex, rule.replace.as_str()))
                .with_context(|| format!("bad normalize pattern {:?}", rule.pattern))
        })
        .collect::<Result<Vec<_>>>()?;
    let mut names = BTreeSet::new();
    for case in &spec.cases {
        // The name becomes a directory under `--keep`.
        let plain = |c: char| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.');
        if case.name.is_empty() || case.name.starts_with('.') || !case.name.chars().all(plain) {
            bail!(
                "{}: case name {:?} may only use letters, digits, `-`, `_`, and `.`, and may not start with `.`",
                path.display(),
                case.name
            );
        }
        if !names.insert(case.name.as_str()) {
            bail!("{}: two cases are named {:?}", path.display(), case.name);
        }
    }
    for name in only {
        if !names.contains(name.as_str()) {
            bail!("no case named {name:?} in {}", path.display());
        }
    }
    let base = path.parent().unwrap_or(&ws.root).to_path_buf();

    let mut results = Vec::new();
    let (mut same, mut accepted, mut differ, mut stale) = (0, 0, 0, 0);
    for case in &spec.cases {
        if !only.is_empty() && !only.contains(&case.name) {
            continue;
        }
        let old = observe(ws, &spec, &rules, &base, case, &spec.old)
            .with_context(|| format!("case {:?}: run the old program", case.name))?;
        let new = observe(ws, &spec, &rules, &base, case, &spec.new)
            .with_context(|| format!("case {:?}: run the new program", case.name))?;
        if let Some(dir) = keep {
            write_kept(&dir.join(&case.name), "old", &old)?;
            write_kept(&dir.join(&case.name), "new", &new)?;
        }
        let differences = differences(&old, &new);
        let status = match (differences.is_empty(), &case.accept) {
            (true, None) => {
                same += 1;
                "same"
            }
            (true, Some(_)) => {
                stale += 1;
                "stale"
            }
            (false, Some(_)) => {
                accepted += 1;
                "accepted"
            }
            (false, None) => {
                differ += 1;
                "differ"
            }
        };
        if !json {
            print_case(case, status, &old, &new, &differences);
        }
        results.push(json!({
            "name": case.name,
            "status": status,
            "old_exit": old.exit,
            "new_exit": new.exit,
            "differs": differences.iter().map(|d| &d.what).collect::<Vec<_>>(),
            "accept": case.accept,
        }));
    }

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "cases": results,
                "same": same,
                "accepted": accepted,
                "differ": differ,
                "stale": stale,
            }))?
        );
    } else {
        println!(
            "{} case(s): {same} same, {accepted} accepted, {differ} differ{}",
            results.len(),
            if stale > 0 {
                format!(", {stale} accepted but no longer differ (remove `accept`)")
            } else {
                String::new()
            }
        );
    }
    Ok(differ == 0)
}

struct Difference {
    what: String,
    detail: Vec<String>,
}

fn differences(old: &Observed, new: &Observed) -> Vec<Difference> {
    let mut out = Vec::new();
    // Two runs that both hung agree on nothing.
    if old.exit == "timeout" || new.exit == "timeout" {
        out.push(Difference {
            what: "timeout".into(),
            detail: vec![format!("old: {}", old.exit), format!("new: {}", new.exit)],
        });
    } else if old.exit != new.exit {
        out.push(Difference {
            what: "exit".into(),
            detail: vec![format!("old: {}", old.exit), format!("new: {}", new.exit)],
        });
    }
    for (what, a, b) in [
        ("stdout", &old.stdout, &new.stdout),
        ("stderr", &old.stderr, &new.stderr),
    ] {
        if a != b {
            out.push(Difference {
                what: what.into(),
                detail: first_difference(a, b),
            });
        }
    }
    let only_old: Vec<&String> = old
        .files
        .keys()
        .filter(|k| !new.files.contains_key(*k))
        .collect();
    let only_new: Vec<&String> = new
        .files
        .keys()
        .filter(|k| !old.files.contains_key(*k))
        .collect();
    if !only_old.is_empty() || !only_new.is_empty() {
        let mut detail = Vec::new();
        detail.extend(only_old.iter().map(|path| format!("only old wrote {path}")));
        detail.extend(only_new.iter().map(|path| format!("only new wrote {path}")));
        out.push(Difference {
            what: "files".into(),
            detail,
        });
    }
    for (path, a) in &old.files {
        match (a, new.files.get(path)) {
            (Left::Text(a), Some(Left::Text(b))) if a != b => out.push(Difference {
                what: format!("file {path}"),
                detail: first_difference(a, b),
            }),
            (a, Some(b)) if a.kind() != b.kind() => out.push(Difference {
                what: format!("file {path}"),
                detail: vec![format!("old: {}", a.kind()), format!("new: {}", b.kind())],
            }),
            _ => {}
        }
    }
    out
}

/// The first line where two texts differ, as lines to print.
fn first_difference(old: &str, new: &str) -> Vec<String> {
    let mut a = old.lines();
    let mut b = new.lines();
    let mut line = 1;
    loop {
        match (a.next(), b.next()) {
            (Some(x), Some(y)) if x == y => line += 1,
            (None, None) => {
                // Same lines: only the final newline differs.
                return vec![format!(
                    "old ends {} a newline, new ends {} one",
                    if old.ends_with('\n') {
                        "with"
                    } else {
                        "without"
                    },
                    if new.ends_with('\n') {
                        "with"
                    } else {
                        "without"
                    },
                )];
            }
            (x, y) => {
                let show = |side: Option<&str>| match side {
                    Some(text) => clip(text),
                    None => "(no more lines)".to_owned(),
                };
                return vec![
                    format!("line {line}"),
                    format!("old: {}", show(x)),
                    format!("new: {}", show(y)),
                ];
            }
        }
    }
}

fn clip(text: &str) -> String {
    const LIMIT: usize = 200;
    if text.chars().count() <= LIMIT {
        text.to_owned()
    } else {
        let head: String = text.chars().take(LIMIT).collect();
        format!("{head}…")
    }
}

fn print_case(case: &Case, status: &str, old: &Observed, new: &Observed, found: &[Difference]) {
    match status {
        "same" => {}
        "stale" => println!(
            "STALE {}: accepted in {COMPARE_FILE} but no longer differs",
            case.name
        ),
        _ => {
            let what: Vec<&str> = found.iter().map(|d| d.what.as_str()).collect();
            println!(
                "{} {}: {} (old exit {}, new exit {})",
                if status == "accepted" {
                    "ACCEPTED"
                } else {
                    "DIFF"
                },
                case.name,
                what.join(", "),
                old.exit,
                new.exit
            );
            if let Some(reason) = &case.accept {
                println!("  accepted: {reason}");
            } else {
                for difference in found
                    .iter()
                    .filter(|d| d.what != "exit" && d.what != "timeout")
                {
                    println!("  {}:", difference.what);
                    for line in &difference.detail {
                        println!("    {line}");
                    }
                }
            }
        }
    }
}

fn observe(
    ws: &Workspace,
    spec: &Spec,
    rules: &[(Regex, &str)],
    base: &Path,
    case: &Case,
    command: &[String],
) -> Result<Observed> {
    let work = tempfile::tempdir()?;
    // macOS reports a temporary directory under /private from inside it.
    let work_dir = work.path().canonicalize()?;
    if let Some(fixture) = &case.fixture {
        let from = base.join(fixture);
        if !from.is_dir() {
            bail!("fixture {} is not a directory", from.display());
        }
        copy_tree(&from, &work_dir)?;
    }
    let root = ws.root.to_string_lossy().into_owned();
    let work_text = work_dir.to_string_lossy().into_owned();
    let expand = |text: &str| text.replace("{root}", &root).replace("{work}", &work_text);

    let mut process = Command::new(expand(&command[0]));
    process
        .args(command[1..].iter().map(|arg| expand(arg)))
        .args(case.args.iter().map(|arg| expand(arg)))
        .current_dir(&work_dir)
        .stdin(if case.stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (key, value) in &spec.env {
        process.env(key, expand(value));
    }
    #[cfg(unix)]
    {
        // Its own group, so a timeout can also stop what it started.
        use std::os::unix::process::CommandExt;
        process.process_group(0);
    }
    let mut child = process
        .spawn()
        .with_context(|| format!("start {}", expand(&command[0])))?;
    if let (Some(text), Some(mut stdin)) = (&case.stdin, child.stdin.take()) {
        let text = text.clone();
        std::thread::spawn(move || {
            let _ = stdin.write_all(text.as_bytes());
        });
    }
    let stdout = drain(child.stdout.take());
    let stderr = drain(child.stderr.take());

    // One deadline covers the program and its output: a process it left
    // running can hold the pipes open after it exits.
    let deadline = Instant::now() + Duration::from_secs(spec.timeout_secs);
    let mut exit = loop {
        if let Some(status) = child.try_wait()? {
            break describe(status);
        }
        if Instant::now() >= deadline {
            kill_group(child.id());
            let _ = child.kill();
            let _ = child.wait();
            break "timeout".to_owned();
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let left = |deadline: Instant| deadline.saturating_duration_since(Instant::now());
    let mut stdout_bytes = stdout.recv_timeout(left(deadline)).ok();
    let mut stderr_bytes = stderr.recv_timeout(left(deadline)).ok();
    // Stop whatever the program left running in its process group.
    kill_group(child.id());
    if stdout_bytes.is_none() || stderr_bytes.is_none() {
        exit = "timeout".to_owned();
        // The pipes close once the group is dead. A process that left the
        // group keeps them open; its output is given up on.
        let grace = Instant::now() + Duration::from_secs(2);
        stdout_bytes = stdout_bytes.or_else(|| stdout.recv_timeout(left(grace)).ok());
        stderr_bytes = stderr_bytes.or_else(|| stderr.recv_timeout(left(grace)).ok());
    }
    let clean = |bytes: Vec<u8>| {
        let mut text = String::from_utf8_lossy(&bytes).into_owned();
        // The canonical path first: the other spelling is a suffix of it.
        text = text.replace(&work_text, "<WORK>");
        text = text.replace(&*work.path().to_string_lossy(), "<WORK>");
        for (regex, replace) in rules {
            text = regex.replace_all(&text, *replace).into_owned();
        }
        text
    };
    let stdout = clean(stdout_bytes.unwrap_or_default());
    let stderr = clean(stderr_bytes.unwrap_or_default());

    let mut files = BTreeMap::new();
    for entry in WalkDir::new(&work_dir).min_depth(1).sort_by_file_name() {
        let entry = entry?;
        let rel = entry
            .path()
            .strip_prefix(&work_dir)
            .unwrap_or(entry.path())
            .to_string_lossy()
            .replace('\\', "/");
        let rel = clean(rel.into_bytes());
        let kind = entry.file_type();
        let left = if kind.is_dir() {
            Left::Dir
        } else if kind.is_symlink() {
            let target = std::fs::read_link(entry.path())?;
            Left::Symlink(clean(target.to_string_lossy().into_owned().into_bytes()))
        } else {
            match std::fs::read(entry.path()).map(String::from_utf8) {
                Ok(Ok(text)) => Left::Text(clean(text.into_bytes())),
                _ => Left::Binary,
            }
        };
        files.insert(rel, left);
    }
    Ok(Observed {
        exit,
        stdout,
        stderr,
        files,
    })
}

fn drain(pipe: Option<impl Read + Send + 'static>) -> std::sync::mpsc::Receiver<Vec<u8>> {
    let (send, receive) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        if let Some(mut pipe) = pipe {
            let _ = pipe.read_to_end(&mut bytes);
        }
        let _ = send.send(bytes);
    });
    receive
}

/// The exit code, or `signal N` for a program a signal ended.
fn describe(status: std::process::ExitStatus) -> String {
    if let Some(code) = status.code() {
        return code.to_string();
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(signal) = status.signal() {
            return format!("signal {signal}");
        }
    }
    "signal".to_owned()
}

/// Kill the process group the program led: a browser or a server it started
/// would otherwise outlive the case. A process that moved itself to another
/// group or session is not reached. Does nothing where there are no process
/// groups.
fn kill_group(pid: u32) {
    if cfg!(unix) {
        let _ = Command::new("kill")
            .args(["-KILL", "--", &format!("-{pid}")])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

fn copy_tree(from: &Path, to: &Path) -> Result<()> {
    for entry in WalkDir::new(from).min_depth(1) {
        let entry = entry?;
        let target: PathBuf = to.join(entry.path().strip_prefix(from)?);
        if entry.file_type().is_dir() {
            std::fs::create_dir_all(&target)?;
            continue;
        }
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        #[cfg(unix)]
        if entry.file_type().is_symlink() {
            // Kept as a link, working or not, so both programs see the
            // fixture as it is checked in.
            std::os::unix::fs::symlink(std::fs::read_link(entry.path())?, &target)
                .with_context(|| format!("link {}", target.display()))?;
            continue;
        }
        std::fs::copy(entry.path(), &target)
            .with_context(|| format!("copy {}", entry.path().display()))?;
    }
    Ok(())
}

fn write_kept(dir: &Path, side: &str, observed: &Observed) -> Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    let listing: String = observed
        .files
        .keys()
        .map(|path| format!("{path}\n"))
        .collect();
    for (name, text) in [
        ("exit", &format!("{}\n", observed.exit)),
        ("stdout", &observed.stdout),
        ("stderr", &observed.stderr),
        ("files", &listing),
    ] {
        std::fs::write(dir.join(format!("{side}.{name}")), text)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_shipped_example_parses_and_its_patterns_compile() {
        let spec: Spec = toml::from_str(include_str!("../examples/typescript/compare.toml"))
            .expect("examples/typescript/compare.toml matches the schema");
        assert_eq!(spec.cases.len(), 5);
        for rule in &spec.normalize {
            Regex::new(&rule.pattern).expect("pattern compiles");
        }
    }

    #[test]
    fn names_the_first_line_that_differs() {
        assert_eq!(
            first_difference("a\nb\nc\n", "a\nB\nc\n"),
            ["line 2", "old: b", "new: B"]
        );
        assert_eq!(
            first_difference("a\n", "a\nb\n"),
            ["line 2", "old: (no more lines)", "new: b"]
        );
        assert_eq!(
            first_difference("a\n", "a"),
            ["old ends with a newline, new ends without one"]
        );
    }
}
