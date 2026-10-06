//! `rustify ratchet`: the CI gate that keeps the Rust port in step with
//! the TypeScript it ports.
//!
//! A ported module is stale when its TS file or a recorded test no longer
//! matches the blobs `done` recorded (see [`crate::index::stale_modules`]).
//! The set of stale modules may only shrink: a branch fails when it makes a
//! module stale that was fresh where it branched. Modules that were already
//! stale at the merge base are grandfathered, and so are modules that
//! `--base` made stale after the branch point, because the comparison runs
//! against the merge base, not the tip of `--base`.
//!
//! A fresh port also may not be dropped from the index (or downgraded to an
//! entry without Rust) while its TS file still exists: that would stop the
//! ratchet tracking it.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::analyze::Analyzer;
use crate::config::Workspace;
use crate::graph::Graph;
use crate::index::{Entry, Index, stale_modules};
use crate::mappings::Mappings;
use crate::plan::Progress;
use crate::provenance;

/// "X is Ported but imports Y, which is not ported" for every done module
/// that imports (at runtime) a module the port has not recorded.
pub fn unported_imports(graph: &Graph, index: &Index) -> Vec<String> {
    let progress = Progress::new(graph, index);
    let mut errors = Vec::new();
    for entry in &index.modules {
        let Some(&f) = graph.by_rel.get(&entry.ts) else {
            continue;
        };
        if !entry.status.is_done() {
            continue;
        }
        let file = &graph.files[f];
        for dep in file.deps.difference(&file.type_deps) {
            if !progress.done_files.contains(dep) {
                errors.push(format!(
                    "{} is {:?} but imports {}, which is not ported",
                    entry.ts, entry.status, graph.files[*dep].rel
                ));
            }
        }
    }
    errors
}

/// Modules stale in `head` but not in `base`, with their changed paths.
pub fn newly_stale(
    base: &BTreeMap<String, Vec<String>>,
    head: &BTreeMap<String, Vec<String>>,
) -> BTreeMap<String, Vec<String>> {
    head.iter()
        .filter(|(ts, _)| !base.contains_key(*ts))
        .map(|(ts, paths)| (ts.clone(), paths.clone()))
        .collect()
}

/// Strings in `head` that `base` does not have.
pub fn newly_present(base: &[String], head: &[String]) -> Vec<String> {
    let base: BTreeSet<&String> = base.iter().collect();
    head.iter().filter(|e| !base.contains(e)).cloned().collect()
}

/// Entries that were fresh ported modules at the merge base (`base_stale`
/// lists the stale ones) and are no longer tracked ports at HEAD although
/// their TS file still exists there: removed from the index, or downgraded to
/// skipped/replaced/in-progress-without-Rust. Returns `(ts, reason)`.
pub fn lost_entries(
    base_index: &Index,
    base_stale: &BTreeMap<String, Vec<String>>,
    head_index: &Index,
    ts_exists_at_head: impl Fn(&str) -> bool,
) -> Vec<(String, &'static str)> {
    base_index
        .modules
        .iter()
        .filter(|e| e.status.has_rust() && !base_stale.contains_key(&e.ts))
        .filter(|e| ts_exists_at_head(&e.ts))
        .filter_map(|e| match head_index.get(&e.ts) {
            None => Some((e.ts.clone(), "removed from the index")),
            Some(h) if !h.is_tracked_port() => {
                Some((e.ts.clone(), "downgraded to an entry without Rust"))
            }
            Some(_) => None,
        })
        .collect()
}

fn stale_set(root: &Path, index: &Index, rev: &str) -> Result<BTreeMap<String, Vec<String>>> {
    stale_modules(
        root,
        index.modules.iter().filter(|e| e.is_tracked_port()),
        rev,
    )
}

pub fn run(ws: &Workspace, graph: &Graph, head_index: &Index, base: &str) -> Result<bool> {
    let root = &ws.root;
    let merge_base = provenance::merge_base_with(root, base)?;
    let index_path = ws.port_rel("index.toml");
    let base_index = match provenance::show(root, &merge_base, &index_path)? {
        Some(text) => Index::parse(&text).context("parse the base port index")?,
        None => Index::default(),
    };

    let base_stale = stale_set(root, &base_index, &merge_base)?;
    let head_stale = stale_set(root, head_index, "HEAD")?;
    let new_stale = newly_stale(&base_stale, &head_stale);

    // Fail closed: without the base graph the import check cannot say what
    // is new, so an error here fails the job instead of skipping the check.
    let head_imports = unported_imports(graph, head_index);
    let base_imports = base_imports(ws, &merge_base, &base_index)
        .context("build the merge-base graph for the unported-import check")?;
    let new_imports = newly_present(&base_imports, &head_imports);

    let head_ts: BTreeSet<String> = {
        let paths: Vec<&str> = base_index
            .modules
            .iter()
            .chain(&head_index.modules)
            .map(|e| e.ts.as_str())
            .collect();
        let queries: Vec<(&str, &str)> = paths.iter().map(|p| ("HEAD", *p)).collect();
        paths
            .iter()
            .zip(provenance::blobs_at(root, &queries)?)
            .filter(|(_, blob)| blob.is_some())
            .map(|(p, _)| (*p).to_owned())
            .collect()
    };
    let lost = lost_entries(&base_index, &base_stale, head_index, |ts| {
        head_ts.contains(ts)
    });

    if new_stale.is_empty() && new_imports.is_empty() && lost.is_empty() {
        println!(
            "OK: no ported module became stale since {} ({} stale at the merge base, {} at HEAD; the count may only shrink).",
            &merge_base[..merge_base.len().min(12)],
            base_stale.len(),
            head_stale.len()
        );
        return Ok(true);
    }
    for (ts, changed) in &new_stale {
        let entry = head_index.get(ts);
        if !head_ts.contains(ts) {
            println!(
                "error: {ts} is a ported module whose TypeScript this branch deleted or renamed"
            );
            println!(
                "  fix:     remove its entry from index.toml, or re-point it at the new TS path (and its Rust), then run `rustify done <new ts file>`"
            );
            continue;
        }
        println!(
            "error: {ts} is a ported module whose TypeScript this branch changed without porting it"
        );
        println!("  changed: {}", changed.join(", "));
        match entry.and_then(|e| rust_path(ws, e)) {
            Some(rust) => {
                println!("  rust:    {}", rust.display());
                println!(
                    "  fix:     port the change into {} and its tests, then run `rustify done {ts}`",
                    rust.display()
                );
            }
            None => println!(
                "  fix:     port the change into its Rust module and tests, then run `rustify done {ts}`"
            ),
        }
    }
    for (ts, why) in &lost {
        println!(
            "error: {ts} was a fresh ported module at the merge base and was {why}, but its TypeScript still exists"
        );
        println!(
            "  fix:     restore the entry in index.toml, or port the change and run `rustify done {ts}`"
        );
    }
    for error in &new_imports {
        println!("error: {error}");
    }
    println!(
        "{} module(s) became stale, {} entry(ies) dropped, and {} new unported import(s). Port each change into Rust and run `rustify done` in this PR.",
        new_stale.len(),
        lost.len(),
        new_imports.len()
    );
    Ok(false)
}

fn rust_path(ws: &Workspace, entry: &Entry) -> Option<PathBuf> {
    let rust = entry.rust.as_ref()?;
    let src = ws.crate_src(entry.module.as_deref().unwrap_or_default());
    let full = src.join(rust);
    Some(
        full.strip_prefix(&ws.root)
            .map(Path::to_path_buf)
            .unwrap_or(full),
    )
}

/// The unported-import errors at the merge base: the graph of its TS under
/// its index, built from a `git archive` of just the port's configuration
/// and state files and the TS sources (no working-tree checkout, nothing to
/// clean up in git). The merge base's own configuration is used, so a
/// branch that edits rustify.toml is compared against what it replaced.
fn base_imports(ws: &Workspace, merge_base: &str, base_index: &Index) -> Result<Vec<String>> {
    let dir = ScratchDir::new()?;
    // Export what the merge base's own configuration reads, so a branch that
    // changes `roots`, `packages_dir`, or `state_dir` still gets a complete
    // base tree. Without a config there, use the current one.
    let base_config = match provenance::show(&ws.root, merge_base, &ws.config_path)? {
        Some(text) => Some(
            toml::from_str::<crate::config::Config>(&text)
                .context("parse the merge base's rustify.toml")?,
        ),
        None => None,
    };
    let config = base_config.as_ref().unwrap_or(&ws.config);
    let state_dir = config.state_dir_for(&ws.config_path);
    let state = |name: &str| {
        if state_dir == "." {
            name.to_owned()
        } else {
            format!("{state_dir}/{name}")
        }
    };
    let mut paths = vec![
        ws.config_path.clone(),
        state("mappings.toml"),
        state("components.toml"),
        config.packages_dir.clone(),
    ];
    paths.extend(config.roots.iter().cloned());
    paths.sort();
    paths.dedup();
    // `git archive` refuses a pathspec that matches nothing at the merge
    // base (no components.toml yet, say), so keep only paths that exist.
    let queries: Vec<(&str, &str)> = paths.iter().map(|p| (merge_base, p.as_str())).collect();
    let present: Vec<&str> = paths
        .iter()
        .zip(provenance::objects_at(&ws.root, &queries)?)
        .filter(|(_, exists)| *exists)
        .map(|(p, _)| p.as_str())
        .collect();
    provenance::export_tree(&ws.root, merge_base, &present, &dir.0)?;
    // A branch that adopts rustify (or moves its config) has no config or
    // mappings at the merge base; read the base's TS with the current ones.
    for (base_rel, head_rel) in [
        (ws.config_path.clone(), ws.config_path.clone()),
        (state("mappings.toml"), ws.port_rel("mappings.toml")),
    ] {
        let target = dir.0.join(&base_rel);
        if !target.exists() {
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::copy(ws.root.join(&head_rel), &target)
                .with_context(|| format!("copy {head_rel} into the merge-base tree"))?;
        }
    }
    let base_ws = Workspace::load(&dir.0, &ws.config_path)?;
    let mappings = Mappings::load(&base_ws)?;
    let analyzer = Analyzer::new(&mappings)?;
    let graph = Graph::build(&base_ws, &analyzer)?;
    Ok(unported_imports(&graph, base_index))
}

/// A temp directory removed on drop.
struct ScratchDir(PathBuf);

impl ScratchDir {
    fn new() -> Result<Self> {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.subsec_nanos());
        let dir =
            std::env::temp_dir().join(format!("rustify-ratchet-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
        Ok(Self(dir))
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::Status;

    fn map(items: &[(&str, &[&str])]) -> BTreeMap<String, Vec<String>> {
        items
            .iter()
            .map(|(k, v)| ((*k).to_owned(), v.iter().map(|s| (*s).to_owned()).collect()))
            .collect()
    }

    #[test]
    fn only_modules_stale_at_head_and_fresh_at_base_fail() {
        let base = map(&[("a.ts", &["a.ts"]), ("b.ts", &["b.ts"])]);
        let head = map(&[("a.ts", &["a.ts", "a.test.ts"]), ("c.ts", &["c.ts"])]);
        let new = newly_stale(&base, &head);
        assert_eq!(new.keys().collect::<Vec<_>>(), ["c.ts"]);
    }

    #[test]
    fn the_stale_set_shrinking_passes() {
        let base = map(&[("a.ts", &["a.ts"])]);
        assert!(newly_stale(&base, &BTreeMap::new()).is_empty());
    }

    #[test]
    fn new_import_errors_are_those_absent_at_base() {
        let base = vec!["x".to_owned(), "y".to_owned()];
        let head = vec!["y".to_owned(), "z".to_owned()];
        assert_eq!(newly_present(&base, &head), ["z"]);
    }

    fn idx(entries: Vec<Entry>) -> Index {
        Index {
            modules: entries,
            ..Index::default()
        }
    }

    fn with_status(ts: &str, status: Status, rust: bool) -> Entry {
        let mut e = entry(ts, &[], None);
        e.status = status;
        e.rust = rust.then(|| "x.rs".to_owned());
        e
    }

    #[test]
    fn dropping_or_downgrading_a_fresh_port_is_lost_but_stale_or_deleted_ones_are_not() {
        let base = idx(vec![
            with_status("a.ts", Status::Ported, true),
            with_status("b.ts", Status::Verified, true),
            with_status("c.ts", Status::Ported, true),
            with_status("d.ts", Status::Ported, true),
            with_status("e.ts", Status::Ported, true),
        ]);
        let head = idx(vec![
            with_status("b.ts", Status::Skipped, false),
            with_status("c.ts", Status::InProgress, true),
            with_status("d.ts", Status::Ported, true),
            with_status("e.ts", Status::Ported, true),
        ]);
        // d.ts was already stale at the base; e.ts's TS file was deleted.
        let stale = map(&[("d.ts", &["d.ts"])]);
        let lost = lost_entries(&base, &stale, &head, |ts| ts != "e.ts");
        let names: Vec<&str> = lost.iter().map(|(t, _)| t.as_str()).collect();
        assert_eq!(names, ["a.ts", "b.ts"]);
        // A deleted TS file whose entry is also removed is fine.
        assert!(lost_entries(&base, &stale, &head, |_| false).is_empty());
    }

    fn entry(ts: &str, blobs: &[(&str, &str)], base: Option<&str>) -> Entry {
        Entry {
            ts: ts.to_owned(),
            status: Status::Ported,
            rust: Some("x.rs".to_owned()),
            module: None,
            ported_at: None,
            ts_base: base.map(str::to_owned),
            ts_blobs: blobs
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect(),
            symbols: BTreeMap::new(),
            tests: vec!["t.test.ts".to_owned()],
            test_provenance: Vec::new(),
            notes: None,
        }
    }

    struct Repo {
        dir: tempfile::TempDir,
    }

    impl Repo {
        fn new() -> Self {
            let repo = Self {
                dir: tempfile::tempdir().unwrap(),
            };
            repo.git(&["init", "-q", "-b", "main"]);
            repo
        }

        fn git(&self, args: &[&str]) -> String {
            let out = std::process::Command::new("git")
                .args(["-c", "user.name=t", "-c", "user.email=t@example.test"])
                .args(["-c", "commit.gpgsign=false"])
                .args(args)
                .current_dir(self.dir.path())
                .output()
                .unwrap();
            assert!(out.status.success(), "{args:?}");
            String::from_utf8_lossy(&out.stdout).trim().to_owned()
        }

        fn commit(&self, files: &[(&str, Option<&str>)]) -> String {
            for (path, body) in files {
                let full = self.dir.path().join(path);
                match body {
                    Some(body) => std::fs::write(full, body).unwrap(),
                    None => std::fs::remove_file(full).unwrap(),
                }
            }
            self.git(&["add", "-A"]);
            self.git(&["commit", "-qm", "c"]);
            self.git(&["rev-parse", "HEAD"])
        }

        fn blob(&self, rev: &str, path: &str) -> Option<String> {
            provenance::blobs_at(self.dir.path(), &[(rev, path)])
                .unwrap()
                .remove(0)
        }
    }

    fn recorded(repo: &Repo, rev: &str) -> Vec<(String, String)> {
        ["m.ts", "t.test.ts"]
            .iter()
            .filter_map(|p| repo.blob(rev, p).map(|b| ((*p).to_owned(), b)))
            .collect()
    }

    fn stale_at(repo: &Repo, e: &Entry, rev: &str) -> Option<Vec<String>> {
        stale_modules(repo.dir.path(), [e], rev)
            .unwrap()
            .remove(&e.ts)
    }

    fn blob_entry(repo: &Repo, rev: &str) -> Entry {
        let rec = recorded(repo, rev);
        let rec: Vec<(&str, &str)> = rec.iter().map(|(a, b)| (a.as_str(), b.as_str())).collect();
        let mut e = entry("m.ts", &rec, Some(rev));
        e.tests = vec!["t.test.ts".to_owned()];
        e
    }

    #[test]
    fn blob_staleness_tracks_content_not_commits() {
        let repo = Repo::new();
        let first = repo.commit(&[("m.ts", Some("1")), ("t.test.ts", Some("t"))]);
        let e = blob_entry(&repo, &first);

        // Unchanged content, new commit: fresh.
        let same = repo.commit(&[("other", Some("x"))]);
        assert_eq!(stale_at(&repo, &e, &same), None);

        // TS edited: stale, naming the path.
        let edited = repo.commit(&[("m.ts", Some("2"))]);
        assert_eq!(stale_at(&repo, &e, &edited), Some(vec!["m.ts".to_owned()]));

        // Edited back to the recorded content (a rewritten commit carrying
        // the ported change): fresh again.
        let restored = repo.commit(&[("m.ts", Some("1"))]);
        assert_eq!(stale_at(&repo, &e, &restored), None);

        // A tracked test changes.
        let test_edit = repo.commit(&[("t.test.ts", Some("t2"))]);
        assert_eq!(
            stale_at(&repo, &e, &test_edit),
            Some(vec!["t.test.ts".to_owned()])
        );
    }

    #[test]
    fn a_tracked_path_appearing_or_disappearing_is_stale() {
        let repo = Repo::new();
        let first = repo.commit(&[("m.ts", Some("1"))]);
        // The test did not exist when recorded: no key for it.
        let e = blob_entry(&repo, &first);
        assert!(!e.ts_blobs.contains_key("t.test.ts"));
        let appeared = repo.commit(&[("t.test.ts", Some("t"))]);
        assert_eq!(
            stale_at(&repo, &e, &appeared),
            Some(vec!["t.test.ts".to_owned()])
        );
        let e2 = blob_entry(&repo, &appeared);
        let gone = repo.commit(&[("t.test.ts", None)]);
        assert_eq!(
            stale_at(&repo, &e2, &gone),
            Some(vec!["t.test.ts".to_owned()])
        );
    }

    #[test]
    fn entries_without_blobs_fall_back_to_the_ts_base_diff() {
        let repo = Repo::new();
        let first = repo.commit(&[("m.ts", Some("1")), ("t.test.ts", Some("t"))]);
        let e = entry("m.ts", &[], Some(&first));
        let unrelated = repo.commit(&[("other", Some("x"))]);
        assert_eq!(stale_at(&repo, &e, &unrelated), None);
        let edited = repo.commit(&[("m.ts", Some("2"))]);
        assert_eq!(stale_at(&repo, &e, &edited), Some(vec!["m.ts".to_owned()]));
    }

    #[test]
    fn working_tree_blobs_equal_committed_blobs() {
        let repo = Repo::new();
        let rev = repo.commit(&[("m.ts", Some("body\n"))]);
        let working = provenance::working_blobs(repo.dir.path(), &["m.ts", "absent.ts"]).unwrap();
        assert_eq!(working.len(), 1);
        assert_eq!(Some(&working["m.ts"]), repo.blob(&rev, "m.ts").as_ref());
    }
}
