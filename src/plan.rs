//! Choosing what to port next.
//!
//! A unit is ready when every unit it imports behavior from is done
//! (type-only imports are satisfied types-first; see `brief`). Ready units are
//! ranked by how much they unblock, then grouped with ready neighbours from
//! the same directory into batches small enough to review. Each batch
//! carries the tests that become portable once it lands.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use crate::config::Workspace;
use crate::graph::{Graph, rust_target};
use crate::index::{Index, Status};

#[derive(Debug, serde::Serialize)]
pub struct Batch {
    pub units: Vec<usize>,
    pub files: Vec<usize>,
    pub lines: usize,
    /// Units whose last missing dependency is in this batch.
    pub unlocks: usize,
    /// Units that (transitively) depend on this batch.
    pub downstream: usize,
    /// Module-level tests (indices into `graph.tests`) portable after it.
    pub tests: Vec<usize>,
}

pub struct Progress {
    pub done_files: BTreeSet<usize>,
    pub in_progress_files: BTreeSet<usize>,
    /// Done files whose TS changed upstream since they were ported
    /// (`Index::upstream`). They stay in `done_files`, so recording and
    /// checking are unaffected, but their units are not done for planning.
    pub stale_files: BTreeSet<usize>,
}

impl Progress {
    pub fn new(graph: &Graph, index: &Index) -> Self {
        let mut done_files = BTreeSet::new();
        let mut in_progress_files = BTreeSet::new();
        let mut stale_files = BTreeSet::new();
        for (i, f) in graph.files.iter().enumerate() {
            if index.upstream.stale.contains_key(&f.rel) {
                stale_files.insert(i);
            }
            match index.status(&f.rel) {
                Some(Status::InProgress) => {
                    in_progress_files.insert(i);
                }
                Some(s) if s.is_done() => {
                    done_files.insert(i);
                }
                _ => {}
            }
        }
        Self {
            done_files,
            in_progress_files,
            stale_files,
        }
    }

    pub fn unit_done(&self, graph: &Graph, u: usize) -> bool {
        graph.units[u]
            .files
            .iter()
            .all(|f| self.done_files.contains(f) && !self.stale_files.contains(f))
    }

    /// Every file of `u` has Rust recorded, stale or not: its types exist,
    /// so dependents need no types-first stubs for it.
    pub fn unit_recorded(&self, graph: &Graph, u: usize) -> bool {
        graph.units[u]
            .files
            .iter()
            .all(|f| self.done_files.contains(f))
    }

    pub fn unit_claimed(&self, graph: &Graph, u: usize) -> bool {
        graph.units[u]
            .files
            .iter()
            .any(|f| self.in_progress_files.contains(f))
    }

    pub fn ready_units(&self, graph: &Graph) -> Vec<usize> {
        (0..graph.units.len())
            .filter(|&u| {
                !self.unit_done(graph, u)
                    && !self.unit_claimed(graph, u)
                    && graph.units[u]
                        .deps
                        .iter()
                        .all(|&d| self.unit_done(graph, d))
            })
            .collect()
    }
}

/// Which ready units `next` may offer.
#[derive(Clone, Copy)]
pub struct Scope<'a> {
    pub package: Option<&'a str>,
    /// Only the catch-up with the upstream ref: stale modules and the modules
    /// upstream added since the port began (`Index::upstream`).
    pub catch_up: bool,
}

/// The units catching up with the upstream ref means porting: every stale
/// module, plus each module upstream added since the port began whose
/// imports are all ported, stale, or themselves such new modules. A new
/// module that sits on never-ported code is ordinary `next` work, and is
/// left to it.
pub fn catch_up_units(graph: &Graph, index: &Index) -> BTreeSet<usize> {
    let progress = Progress::new(graph, index);
    let holds = |u: usize, set: &BTreeMap<String, String>| {
        graph.units[u]
            .files
            .iter()
            .any(|&f| set.contains_key(&graph.files[f].rel))
    };
    let stale: BTreeSet<usize> = (0..graph.units.len())
        .filter(|&u| holds(u, &index.upstream.stale))
        .collect();
    let mut added: BTreeSet<usize> = (0..graph.units.len())
        .filter(|&u| {
            !progress.unit_recorded(graph, u)
                && graph.units[u]
                    .files
                    .iter()
                    .any(|&f| index.upstream.added.contains(&graph.files[f].rel))
        })
        .collect();
    loop {
        let blocked: Vec<usize> = added
            .iter()
            .copied()
            .filter(|&u| {
                graph.units[u].deps.iter().any(|&d| {
                    !progress.unit_recorded(graph, d) && !stale.contains(&d) && !added.contains(&d)
                })
            })
            .collect();
        if blocked.is_empty() {
            break;
        }
        for u in blocked {
            added.remove(&u);
        }
    }
    stale.union(&added).copied().collect()
}

pub fn next_batches(
    ws: &Workspace,
    graph: &Graph,
    index: &Index,
    count: usize,
    scope: Scope<'_>,
) -> Vec<Batch> {
    ranked_batches(ws, graph, index, count, scope, |_| true)
}

/// A wave of up to `n` batches, in rank order, that are pairwise
/// `compatible`. Ready batches never depend on each other, so a wave can be
/// ported in parallel; the footprint check keeps their diffs from colliding.
/// Units of a batch that does not fit stay for a later wave.
pub fn independent_wave(
    ws: &Workspace,
    graph: &Graph,
    index: &Index,
    n: usize,
    scope: Scope<'_>,
) -> Vec<Batch> {
    let mut chosen: Vec<Footprint> = Vec::new();
    ranked_batches(ws, graph, index, n, scope, |batch| {
        let fp = footprint(ws, graph, index, batch);
        if chosen.iter().all(|c| compatible(c, &fp)) {
            chosen.push(fp);
            true
        } else {
            false
        }
    })
}

fn ranked_batches(
    ws: &Workspace,
    graph: &Graph,
    index: &Index,
    count: usize,
    scope: Scope<'_>,
    mut accept: impl FnMut(&Batch) -> bool,
) -> Vec<Batch> {
    let progress = Progress::new(graph, index);
    let mut ready = progress.ready_units(graph);
    if scope.catch_up {
        let catch_up = catch_up_units(graph, index);
        ready.retain(|u| catch_up.contains(u));
    }
    if let Some(package) = scope.package {
        ready.retain(|&u| {
            graph.units[u]
                .files
                .iter()
                .any(|&f| graph.files[f].package == package)
        });
    }
    let score = |u: usize| {
        let unlocks = graph.units[u]
            .dependents
            .iter()
            .filter(|&&d| {
                !progress.unit_done(graph, d)
                    && graph.units[d]
                        .deps
                        .iter()
                        .all(|&x| x == u || progress.unit_done(graph, x))
            })
            .count();
        (unlocks, downstream(graph, u))
    };
    // Ranking: no types-first stubs needed, then units that bring a test
    // with them, then the most unblocking, then the smallest.
    let pending_types = |u: usize| {
        graph.units[u]
            .type_deps
            .iter()
            .filter(|&&d| !progress.unit_recorded(graph, d))
            .count()
    };
    // Tests that porting this unit alone would make runnable: the goal is
    // to keep as much ported code under test as possible.
    let ported_tests = index.ported_tests();
    let tests_enabled = |u: usize| {
        let files: BTreeSet<usize> = graph.units[u].files.iter().copied().collect();
        graph
            .tests
            .iter()
            .filter(|t| {
                !t.binary
                    && !ported_tests.contains(t.rel.as_str())
                    && t.deps.iter().any(|d| files.contains(d))
                    && t.deps
                        .iter()
                        .all(|d| files.contains(d) || progress.done_files.contains(d))
            })
            .count()
    };
    let mut ranked: Vec<(usize, (usize, usize))> = ready.iter().map(|&u| (u, score(u))).collect();
    let tests: std::collections::HashMap<usize, usize> =
        ready.iter().map(|&u| (u, tests_enabled(u))).collect();
    ranked.sort_by(|a, b| {
        pending_types(a.0)
            .cmp(&pending_types(b.0))
            .then(tests[&b.0].min(1).cmp(&tests[&a.0].min(1)))
            .then(b.1.cmp(&a.1))
            .then(tests[&b.0].cmp(&tests[&a.0]))
            .then(graph.units[a.0].lines.cmp(&graph.units[b.0].lines))
            .then(a.0.cmp(&b.0))
    });

    let is_update = |u: usize| {
        graph.units[u]
            .files
            .iter()
            .any(|f| progress.stale_files.contains(f))
    };
    let caps = &ws.config.batch;
    let mut used: BTreeSet<usize> = BTreeSet::new();
    let mut batches = Vec::new();
    for &(seed, _) in &ranked {
        if batches.len() >= count {
            break;
        }
        if used.contains(&seed) {
            continue;
        }
        let dir = unit_dir(graph, seed);
        let mut units = vec![seed];
        let mut files = graph.units[seed].files.len();
        let mut lines = graph.units[seed].lines;
        used.insert(seed);
        let seed_needs_stubs = pending_types(seed) > 0;
        let seed_is_update = is_update(seed);
        for &(other, _) in &ranked {
            // Same directory, the same answer to "does this need types-first
            // stubs?", and the same kind of work (fresh port or update of a
            // stale one), so a batch reviews as one kind of change.
            if used.contains(&other)
                || unit_dir(graph, other) != dir
                || (pending_types(other) > 0) != seed_needs_stubs
                || is_update(other) != seed_is_update
            {
                continue;
            }
            let u = &graph.units[other];
            if files + u.files.len() > caps.max_files || lines + u.lines > caps.max_lines {
                continue;
            }
            files += u.files.len();
            lines += u.lines;
            units.push(other);
            used.insert(other);
        }
        let batch = assemble(graph, &progress, index, units);
        if accept(&batch) {
            batches.push(batch);
        }
    }
    batches
}

/// Describe an explicit set of files as a batch (for `brief` on paths).
pub fn batch_for_files(graph: &Graph, index: &Index, files: &[usize]) -> Batch {
    let progress = Progress::new(graph, index);
    let mut units: Vec<usize> = files.iter().map(|&f| graph.unit_of[f]).collect();
    units.sort();
    units.dedup();
    assemble(graph, &progress, index, units)
}

fn assemble(graph: &Graph, progress: &Progress, index: &Index, units: Vec<usize>) -> Batch {
    let files: Vec<usize> = units
        .iter()
        .flat_map(|&u| graph.units[u].files.iter().copied())
        .collect();
    let file_set: BTreeSet<usize> = files.iter().copied().collect();
    let ported = index.ported_tests();
    // A stale module's recorded tests come back with it: they are the
    // scenarios its update must keep (and extend with upstream's changes).
    let recorded: BTreeSet<&str> = files
        .iter()
        .filter(|f| progress.stale_files.contains(f))
        .filter_map(|&f| index.get(&graph.files[f].rel))
        .flat_map(|e| e.tests.iter().map(String::as_str))
        .collect();
    let tests = graph
        .tests
        .iter()
        .enumerate()
        .filter(|(_, t)| {
            recorded.contains(t.rel.as_str()) || {
                !t.binary
                    && !ported.contains(t.rel.as_str())
                    && t.deps.iter().any(|d| file_set.contains(d))
                    && t.deps
                        .iter()
                        .all(|d| file_set.contains(d) || progress.done_files.contains(d))
            }
        })
        .map(|(i, _)| i)
        .collect();
    // Counted for the batch as a whole, so a dependent that needs two
    // co-batched units is still counted, and `next` and `brief` agree.
    let in_batch: BTreeSet<usize> = units.iter().copied().collect();
    let unlocks = (0..graph.units.len())
        .filter(|d| !in_batch.contains(d) && !progress.unit_done(graph, *d))
        .filter(|&d| {
            let deps = &graph.units[d].deps;
            deps.iter().any(|x| in_batch.contains(x))
                && deps
                    .iter()
                    .all(|&x| in_batch.contains(&x) || progress.unit_done(graph, x))
        })
        .count();
    let mut reached = BTreeSet::new();
    let mut queue: VecDeque<usize> = units
        .iter()
        .flat_map(|&u| graph.units[u].dependents.iter().copied())
        .collect();
    while let Some(d) = queue.pop_front() {
        if !in_batch.contains(&d) && reached.insert(d) {
            queue.extend(graph.units[d].dependents.iter().copied());
        }
    }
    let downstream = reached.len();
    Batch {
        lines: units.iter().map(|&u| graph.units[u].lines).sum(),
        units,
        files,
        unlocks,
        downstream,
        tests,
    }
}

/// What a batch changes in Rust, as `<crate>/<path under src>`.
#[derive(Debug, Default, serde::Serialize)]
pub struct Footprint {
    /// Files whose content the batch writes: its own targets and the
    /// dependency files it declares types-first in.
    pub writes: BTreeSet<String>,
    /// Parent module files that only gain a `mod` line (created if missing,
    /// up to the first ancestor that exists). Two batches adding lines to the
    /// same parent merge mechanically, so these never block a wave.
    pub registers: BTreeSet<String>,
}

pub fn footprint(ws: &Workspace, graph: &Graph, index: &Index, batch: &Batch) -> Footprint {
    let progress = Progress::new(graph, index);
    let in_batch: BTreeSet<usize> = batch.units.iter().copied().collect();
    let mut out = Footprint::default();
    let mut add_target = |rel: &str, own: bool| {
        let (file, module) = rust_target(ws, &graph.packages, rel);
        let krate = module.split("::").next().unwrap_or_default().to_string();
        out.writes.insert(format!("{krate}/{file}"));
        if !own {
            return;
        }
        let src = ws.crate_src(&module);
        let stem = file.strip_suffix(".rs").unwrap_or(&file);
        let mut dir = stem.strip_suffix("/mod").unwrap_or(stem);
        loop {
            match dir.rsplit_once('/').map(|(p, _)| p) {
                None => {
                    out.registers.insert(format!("{krate}/lib.rs"));
                    break;
                }
                Some(p) => {
                    out.registers.insert(format!("{krate}/{p}/mod.rs"));
                    if src.join(p).join("mod.rs").exists() || src.join(format!("{p}.rs")).exists() {
                        break;
                    }
                    dir = p;
                }
            }
        }
    };
    for &f in &batch.files {
        add_target(&graph.files[f].rel, true);
    }
    for &u in &batch.units {
        for &d in &graph.units[u].type_deps {
            if in_batch.contains(&d) || progress.unit_recorded(graph, d) {
                continue;
            }
            for &f in &graph.units[d].files {
                add_target(&graph.files[f].rel, false);
            }
        }
    }
    out
}

/// Whether two batches can be ported at the same time: neither writes a
/// file the other writes, and neither writes the content of a module file
/// the other only registers into.
pub fn compatible(a: &Footprint, b: &Footprint) -> bool {
    a.writes.is_disjoint(&b.writes)
        && a.writes.is_disjoint(&b.registers)
        && b.writes.is_disjoint(&a.registers)
}

fn unit_dir(graph: &Graph, u: usize) -> String {
    let rel = &graph.files[graph.units[u].files[0]].rel;
    rel.rsplit_once('/')
        .map(|(d, _)| d.to_string())
        .unwrap_or_default()
}

/// Number of units that transitively depend on `u`.
pub fn downstream(graph: &Graph, u: usize) -> usize {
    let mut seen = BTreeSet::new();
    let mut queue: VecDeque<usize> = graph.units[u].dependents.iter().copied().collect();
    while let Some(d) = queue.pop_front() {
        if seen.insert(d) {
            queue.extend(graph.units[d].dependents.iter().copied());
        }
    }
    seen.len()
}
