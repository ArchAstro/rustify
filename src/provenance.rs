//! When a module was ported and from which TS: `done` stamps each index
//! entry so `drift` can later list ported modules whose TypeScript changed
//! on the upstream ref (`upstream` in rustify.toml) since, for fixup passes.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};

/// `git merge-base HEAD <upstream>`: the upstream commit whose TS the
/// working tree was ported from. Fetch the upstream first so it is current.
pub fn ts_base(root: &Path, upstream: &str) -> Result<String> {
    git(root, &["merge-base", "HEAD", upstream])
        .with_context(|| format!("find the {upstream} base of HEAD (fetch {upstream} first)"))
}

/// Every path that differs between `base` and `against` (under `under`,
/// when given); with `added_only`, only paths `against` added.
pub fn changed_files(
    root: &Path,
    base: &str,
    against: &str,
    added_only: bool,
    under: Option<&str>,
) -> Result<BTreeSet<String>> {
    let mut args = vec!["diff", "--name-only", "--no-renames"];
    if added_only {
        args.push("--diff-filter=A");
    }
    args.extend([base, against]);
    if let Some(dir) = under {
        args.extend(["--", dir]);
    }
    Ok(git(root, &args)?.lines().map(str::to_owned).collect())
}

/// `git diff base against -- paths`, the patch a fixup pass ports.
pub fn diff(root: &Path, base: &str, against: &str, paths: &[&str]) -> Result<String> {
    let mut args = vec!["diff", base, against, "--"];
    args.extend(paths);
    git(root, &args)
}

/// The newest commit every one of `commits` descends from.
pub fn common_base(root: &Path, commits: &[&str]) -> Result<String> {
    if let [only] = commits {
        return Ok((*only).to_owned());
    }
    let mut args = vec!["merge-base", "--octopus"];
    args.extend(commits);
    git(root, &args)
}

/// Blob ids for `(rev, path)` pairs, in order; `None` where `path` does not
/// exist at `rev`. One `git cat-file --batch-check` process serves every
/// query, so looking up a thousand modules costs one git invocation.
pub fn blobs_at(root: &Path, queries: &[(&str, &str)]) -> Result<Vec<Option<String>>> {
    batch_check(root, queries)?
        .into_iter()
        .zip(queries)
        .map(|(found, (rev, path))| match found {
            Some((oid, kind)) if kind == "blob" => Ok(Some(oid)),
            Some((_, kind)) => bail!("git cat-file: {rev}:{path} is a {kind}, not a blob"),
            None => Ok(None),
        })
        .collect()
}

/// Whether each `(rev, path)` names an object (a file or a directory).
pub fn objects_at(root: &Path, queries: &[(&str, &str)]) -> Result<Vec<bool>> {
    Ok(batch_check(root, queries)?
        .into_iter()
        .map(|found| found.is_some())
        .collect())
}

/// `(oid, type)` for each `(rev, path)`, `None` where it is missing.
fn batch_check(root: &Path, queries: &[(&str, &str)]) -> Result<Vec<Option<(String, String)>>> {
    let lines: Vec<String> = queries
        .iter()
        .map(|(rev, path)| format!("{rev}:{path}"))
        .collect();
    let out = git_with_stdin(root, &["cat-file", "--batch-check"], &lines)?;
    let results: Vec<&str> = out.lines().collect();
    if results.len() != queries.len() {
        bail!(
            "git cat-file answered {} of {} object queries",
            results.len(),
            queries.len()
        );
    }
    results
        .iter()
        .zip(queries)
        .map(|(line, (rev, path))| {
            let mut parts = line.split(' ');
            match (parts.next(), parts.next()) {
                (_, Some("missing")) => Ok(None),
                (Some(oid), Some(kind)) => Ok(Some((oid.to_owned(), kind.to_owned()))),
                _ => bail!("git cat-file: unexpected answer for {rev}:{path}: {line}"),
            }
        })
        .collect()
}

/// Blob ids (as `git hash-object` computes them) of the working-tree files
/// among `paths`; paths with no file on disk are left out.
pub fn working_blobs(root: &Path, paths: &[&str]) -> Result<BTreeMap<String, String>> {
    let present: Vec<String> = paths
        .iter()
        .filter(|p| root.join(p).is_file())
        .map(|p| (*p).to_owned())
        .collect();
    if present.is_empty() {
        return Ok(BTreeMap::new());
    }
    let out = git_with_stdin(root, &["hash-object", "--stdin-paths"], &present)?;
    let oids: Vec<&str> = out.lines().collect();
    if oids.len() != present.len() {
        bail!(
            "git hash-object hashed {} of {} files",
            oids.len(),
            present.len()
        );
    }
    Ok(present
        .into_iter()
        .zip(oids)
        .map(|(path, oid)| (path, oid.to_owned()))
        .collect())
}

/// `git merge-base HEAD <base>`: where the pull request branched from `base`.
pub fn merge_base_with(root: &Path, base: &str) -> Result<String> {
    git(root, &["merge-base", "HEAD", base])
        .with_context(|| format!("find the merge base of HEAD and {base} (fetch full history)"))
}

/// The contents of `path` at `rev`, or `None` when it does not exist there.
pub fn show(root: &Path, rev: &str, path: &str) -> Result<Option<String>> {
    if blobs_at(root, &[(rev, path)])?[0].is_none() {
        return Ok(None);
    }
    git(root, &["show", &format!("{rev}:{path}")]).map(Some)
}

/// Extracts `paths` of `rev` into `dest` (a `git archive | tar -x`).
pub fn export_tree(root: &Path, rev: &str, paths: &[&str], dest: &Path) -> Result<()> {
    let mut archive = Command::new("git")
        .args(["archive", rev, "--"])
        .args(paths)
        .current_dir(root)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("run git archive")?;
    let tar = Command::new("tar")
        .arg("-x")
        .arg("-C")
        .arg(dest)
        .stdin(Stdio::from(
            archive.stdout.take().context("git archive stdout")?,
        ))
        .output()
        .context("run tar")?;
    let archive = archive.wait_with_output().context("wait for git archive")?;
    if !archive.status.success() {
        bail!(
            "git archive {rev}: {}",
            String::from_utf8_lossy(&archive.stderr).trim()
        );
    }
    if !tar.status.success() {
        bail!("tar: {}", String::from_utf8_lossy(&tar.stderr).trim());
    }
    Ok(())
}

/// Runs git feeding `lines` (newline-terminated) on stdin. A writer thread
/// keeps a large query from deadlocking against git's output pipe.
fn git_with_stdin(root: &Path, args: &[&str], lines: &[String]) -> Result<String> {
    let mut child = Command::new("git")
        .args(args)
        .current_dir(root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("run git")?;
    let mut stdin = child.stdin.take().context("git stdin")?;
    let input: String = lines.iter().map(|l| format!("{l}\n")).collect();
    let writer = std::thread::spawn(move || stdin.write_all(input.as_bytes()));
    let output = child.wait_with_output().context("wait for git")?;
    let _ = writer.join();
    if !output.status.success() {
        bail!(
            "git {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn git(root: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .context("run git")?;
    if !output.status.success() {
        bail!(
            "git {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

/// Current UTC time as RFC 3339 with seconds, e.g. `2026-09-23T21:04:05Z`.
pub fn now_utc() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    format_utc(secs)
}

fn format_utc(secs: u64) -> String {
    let days = secs / 86_400;
    let rem = secs % 86_400;
    let (year, month, day) = civil_from_days(i64::try_from(days).unwrap_or(i64::MAX));
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rem / 3_600,
        rem % 3_600 / 60,
        rem % 60
    )
}

/// Days since 1970-01-01 → (year, month, day), proleptic Gregorian
/// (Howard Hinnant's `civil_from_days`).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = u32::try_from(doy - (153 * mp + 2) / 5 + 1).expect("day is 1..=31");
    let month = u32::try_from(if mp < 10 { mp + 3 } else { mp - 9 }).expect("month is 1..=12");
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_utc_timestamps() {
        assert_eq!(format_utc(0), "1970-01-01T00:00:00Z");
        assert_eq!(format_utc(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(format_utc(1_790_197_445), "2026-09-23T21:04:05Z");
    }
}
