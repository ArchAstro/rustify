//! `index.toml`: what has been ported, where it went, and which TS
//! exports became which Rust items. Later ports and reviews look things up
//! here instead of rediscovering them.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::config::Workspace;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    /// Claimed by a port in progress; blocks the module from `next`.
    InProgress,
    /// Rust exists and its ported tests pass.
    Ported,
    /// Also covered by the binary-level contract suite.
    Verified,
    /// Behavior provided by a crate or the Rust standard library instead of
    /// a ported module (e.g. a Node API wrapper).
    Replaced,
    /// Not needed in Rust (TS-only build glue, bun entrypoints).
    Skipped,
}

impl Status {
    /// Dependents may build on this module.
    pub fn is_done(self) -> bool {
        !matches!(self, Status::InProgress)
    }

    /// A Rust file is expected to exist for this module.
    pub fn has_rust(self) -> bool {
        matches!(self, Status::Ported | Status::Verified)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Entry {
    /// Repository-relative TypeScript path.
    pub ts: String,
    pub status: Status,
    /// Rust file relative to the crate's `src/` (ported/verified).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rust: Option<String>,
    /// Rust module path, e.g. `my_app::storage::store`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub module: Option<String>,
    /// When `done`/`skip`/`replace` last recorded this entry (UTC, RFC 3339).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ported_at: Option<String>,
    /// `git merge-base HEAD <upstream>` at that time: the upstream commit
    /// whose TS was ported. `drift` diffs from here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ts_base: Option<String>,
    /// Git blob ids of `ts` and each tracked test as they were when the port
    /// was recorded: the TS the Rust was written against. A path with no key
    /// did not exist then. Staleness compares these with the blobs at a ref,
    /// which survives rebase-merges (they rewrite commits, not contents);
    /// `ts_base` cannot. Entries without it fall back to `ts_base` diffs.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub ts_blobs: BTreeMap<String, String>,
    /// TS export name → Rust item path.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub symbols: BTreeMap<String, String>,
    /// TS test files whose cases were ported with this module.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tests: Vec<String>,
    /// Ported TS tests outside the current graph. They cannot satisfy `check`'s
    /// graph requirement, but their later edits must still make the port stale.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub test_provenance: Vec<String>,
    /// Why a module is replaced or skipped, or anything a later port needs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Index {
    #[serde(default, rename = "module")]
    pub modules: Vec<Entry>,
    /// Upstream state, filled by [`Index::load_upstream`]; never saved.
    #[serde(skip)]
    pub upstream: Upstream,
}

/// How the upstream ref moved since the port recorded its entries.
#[derive(Debug, Default)]
pub struct Upstream {
    /// Ported modules whose TS or recorded tests changed since their
    /// `ts_base`, keyed by TS path, with that base. Planning treats them as
    /// not done until `done` re-stamps them, so a stale module is re-ported
    /// after the modules it imports and before the modules importing it.
    pub stale: BTreeMap<String, String>,
    /// Unindexed files the upstream added since the oldest `ts_base`: new
    /// modules that landed while the port was underway.
    pub added: std::collections::BTreeSet<String>,
    /// Why upstream state is unavailable (the ref was never fetched, say).
    pub error: Option<String>,
}

impl Index {
    pub fn load(ws: &Workspace) -> Result<Self> {
        let path = ws.port_file("index.toml");
        if !path.exists() {
            return Ok(Self::default());
        }
        let text =
            std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
        Self::parse(&text).with_context(|| format!("parse {}", path.display()))
    }

    /// Parse an `index.toml` body (also used for the copy at another commit).
    pub fn parse(text: &str) -> Result<Self> {
        let index: Index = toml::from_str(text)?;
        let mut seen = std::collections::BTreeSet::new();
        for entry in &index.modules {
            if !seen.insert(&entry.ts) {
                bail!("{} appears twice in index.toml", entry.ts);
            }
        }
        Ok(index)
    }

    pub fn save(&mut self, ws: &Workspace) -> Result<()> {
        self.modules.sort_by(|a, b| a.ts.cmp(&b.ts));
        let body = toml::to_string_pretty(self)?;
        let text = format!(
            "# Port progress. Maintained by `rustify start|done|skip`; edit notes by hand.\n\n{body}"
        );
        let path = ws.port_file("index.toml");
        std::fs::write(&path, text).with_context(|| format!("write {}", path.display()))
    }

    /// Compare every stamped entry with the upstream ref (see [`Upstream`]).
    /// Failure is recorded, not raised: planning still works offline.
    pub fn load_upstream(&mut self, ws: &Workspace) {
        match self.compute_upstream(ws) {
            Ok(upstream) => self.upstream = upstream,
            Err(error) => {
                self.upstream = Upstream {
                    error: Some(format!("{error:#}")),
                    ..Upstream::default()
                }
            }
        }
    }

    fn compute_upstream(&self, ws: &Workspace) -> Result<Upstream> {
        use crate::provenance::{changed_files, common_base};
        let upstream_ref = ws.upstream();
        let mut by_base: BTreeMap<&str, Vec<&Entry>> = BTreeMap::new();
        let stamped: Vec<&Entry> = self
            .modules
            .iter()
            .filter(|e| e.is_tracked_port() && e.ts_base.is_some())
            .collect();
        for entry in &stamped {
            if let Some(base) = &entry.ts_base {
                by_base.entry(base.as_str()).or_default().push(entry);
            }
        }
        let mut upstream = Upstream::default();
        let stale = stale_modules(&ws.root, stamped.iter().copied(), upstream_ref)?;
        for entry in &stamped {
            if let (Some(base), true) = (&entry.ts_base, stale.contains_key(&entry.ts)) {
                upstream.stale.insert(entry.ts.clone(), base.clone());
            }
        }
        let bases: Vec<&str> = by_base.keys().copied().collect();
        if !bases.is_empty() {
            let oldest = common_base(&ws.root, &bases)?;
            upstream.added = changed_files(
                &ws.root,
                &oldest,
                upstream_ref,
                true,
                Some(&ws.config.packages_dir),
            )?
            .into_iter()
            .filter(|path| self.get(path).is_none())
            .collect();
        }
        Ok(upstream)
    }

    pub fn get(&self, ts: &str) -> Option<&Entry> {
        self.modules.iter().find(|e| e.ts == ts)
    }

    pub fn upsert(&mut self, entry: Entry) {
        match self.modules.iter_mut().find(|e| e.ts == entry.ts) {
            Some(existing) => *existing = entry,
            None => self.modules.push(entry),
        }
    }

    pub fn status(&self, ts: &str) -> Option<Status> {
        self.get(ts).map(|e| e.status)
    }

    /// Every test file already claimed by a ported module.
    pub fn ported_tests(&self) -> std::collections::BTreeSet<&str> {
        self.modules
            .iter()
            .filter(|e| e.status.is_done())
            .flat_map(Entry::tracked_tests)
            .collect()
    }
}

/// Ported modules (or ones re-claimed with `start`, which keep their Rust
/// and record) among `entries` that changed at `against` since they were
/// ported, keyed by TS path with the tracked paths that changed.
///
/// An entry with `ts_blobs` is stale when any tracked path's blob at
/// `against` differs from the record (a path appearing or disappearing
/// counts). One without falls back to diffing `ts_base..against`.
pub fn stale_modules<'a>(
    root: &std::path::Path,
    entries: impl IntoIterator<Item = &'a Entry>,
    against: &str,
) -> Result<BTreeMap<String, Vec<String>>> {
    use crate::provenance::{blobs_at, changed_files};
    let mut stale = BTreeMap::new();
    let mut with_blobs: Vec<&Entry> = Vec::new();
    let mut by_base: BTreeMap<&str, Vec<&Entry>> = BTreeMap::new();
    for entry in entries {
        if !entry.ts_blobs.is_empty() {
            with_blobs.push(entry);
        } else if let Some(base) = &entry.ts_base {
            by_base.entry(base.as_str()).or_default().push(entry);
        }
    }
    let mut paths: BTreeSet<&str> = BTreeSet::new();
    for entry in &with_blobs {
        paths.extend(entry.tracked_paths());
    }
    let paths: Vec<&str> = paths.into_iter().collect();
    let queries: Vec<(&str, &str)> = paths.iter().map(|p| (against, *p)).collect();
    let now: BTreeMap<&str, String> = paths
        .iter()
        .copied()
        .zip(blobs_at(root, &queries)?)
        .filter_map(|(path, blob)| blob.map(|b| (path, b)))
        .collect();
    for entry in with_blobs {
        let mut changed: Vec<String> = entry
            .tracked_paths()
            .filter(|p| entry.ts_blobs.get(*p) != now.get(p))
            .map(str::to_owned)
            .collect();
        changed.sort();
        if !changed.is_empty() {
            stale.insert(entry.ts.clone(), changed);
        }
    }
    for (base, entries) in by_base {
        let changed = changed_files(root, base, against, false, None)?;
        for entry in entries {
            let mut touched: Vec<String> = entry
                .tracked_paths()
                .filter(|p| changed.contains(*p))
                .map(str::to_owned)
                .collect();
            touched.sort();
            if !touched.is_empty() {
                stale.insert(entry.ts.clone(), touched);
            }
        }
    }
    Ok(stale)
}

impl Entry {
    /// A Rust port exists (or is being updated): the entries whose staleness
    /// is tracked. A stale module re-claimed with `start` keeps its Rust and
    /// record, so it stays stale until `done` re-stamps it.
    pub fn is_tracked_port(&self) -> bool {
        self.status.has_rust() || (self.status == Status::InProgress && self.rust.is_some())
    }

    /// `ts` followed by every tracked test.
    pub fn tracked_paths(&self) -> impl Iterator<Item = &str> {
        std::iter::once(self.ts.as_str()).chain(self.tracked_tests())
    }

    /// Every test whose cases have been ported, whether or not the current
    /// dependency graph can assign it to a module batch.
    pub fn tracked_tests(&self) -> impl Iterator<Item = &str> {
        self.tests
            .iter()
            .chain(&self.test_provenance)
            .map(String::as_str)
    }
}
