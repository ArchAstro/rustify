//! Command implementations. Each returns `Ok(false)` to exit nonzero.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde_json::json;
use walkdir::WalkDir;

use crate::Command;
use crate::analyze::Analyzer;
use crate::brief;
use crate::components::{Components, WidgetStatus};
use crate::config::Workspace;
use crate::graph::{Graph, by_package, module_for_file, rust_target, snake};
use crate::index::{Entry, Index, Status};
use crate::mappings::Mappings;
use crate::plan::{self, Progress};
use crate::provenance;
use crate::rust_items::{declared_modules, parse_items};

struct Ctx<'a> {
    ws: &'a Workspace,
    graph: Graph,
    index: Index,
    mappings: Mappings,
}

pub fn run(ws: &Workspace, command: Command, json: bool) -> Result<bool> {
    if let Command::Compare { cases, keep } = &command {
        // Needs neither the graph nor the index.
        return crate::compare::run(ws, cases, keep.as_deref(), json);
    }
    let mappings = Mappings::load(ws)?;
    let analyzer = Analyzer::new(&mappings)?;
    let graph = Graph::build(ws, &analyzer)?;
    if let Command::E2e = command {
        return crate::e2e::run(ws, &graph, &analyzer, json);
    }
    let mut index = Index::load(ws)?;
    index.load_upstream(ws);
    let mut ctx = Ctx {
        ws,
        graph,
        index,
        mappings,
    };
    match command {
        Command::Status => status(&ctx, json),
        Command::Next {
            count,
            package,
            brief,
            independent,
            catch_up,
            write_briefs,
        } => next(
            &ctx,
            NextOptions {
                count,
                scope: plan::Scope {
                    package: package.as_deref(),
                    catch_up,
                },
                show_brief: brief,
                independent,
                write_briefs: write_briefs.as_deref(),
            },
            json,
        ),
        Command::Brief { files } => {
            let files = ctx.files(&files)?;
            let batch = plan::batch_for_files(&ctx.graph, &ctx.index, &files);
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&batch_json(&ctx, &batch))?
                );
            } else {
                print!(
                    "{}",
                    brief::render(ctx.ws, &ctx.graph, &ctx.index, &ctx.mappings, &batch)
                );
            }
            Ok(true)
        }
        Command::Start { files, force } => start(&mut ctx, &files, force),
        Command::Done {
            files,
            tests,
            test_provenance,
            maps,
            rust,
            verified,
            notes,
        } => done(
            &mut ctx,
            &files,
            &tests,
            &test_provenance,
            &maps,
            rust,
            verified,
            notes,
        ),
        Command::Skip { files, reason } => {
            mark(&mut ctx, &files, Status::Skipped, None, Some(reason))
        }
        Command::Replace {
            files,
            with,
            reason,
        } => mark(&mut ctx, &files, Status::Replaced, Some(with), reason),
        Command::Find { query } => find(&ctx, &query, json),
        Command::Check => check(&ctx),
        Command::Drift { against } => {
            let against = against.unwrap_or_else(|| ctx.ws.upstream().to_owned());
            drift(&ctx, &against, json)
        }
        Command::StampBlobs => stamp_blobs(&mut ctx),
        Command::Ratchet { base } => {
            let base = base.unwrap_or_else(|| ctx.ws.upstream().to_owned());
            crate::ratchet::run(ctx.ws, &ctx.graph, &ctx.index, &base)
        }
        Command::Graph => graph_summary(&ctx, json),
        Command::E2e | Command::Compare { .. } => unreachable!("handled above"),
    }
}

impl Ctx<'_> {
    /// Resolve CLI paths (repository-relative, absolute, or relative to the
    /// current directory) to graph source files.
    fn files(&self, args: &[String]) -> Result<Vec<usize>> {
        let cwd = std::env::current_dir()?;
        let mut out = Vec::new();
        for arg in args {
            let rel = normalize_arg(&self.ws.root, &cwd, arg);
            match self.graph.by_rel.get(&rel) {
                Some(&i) => out.push(i),
                None => bail!(
                    "{arg} is not a source file in the port graph (tests are ported with their modules)"
                ),
            }
        }
        Ok(out)
    }
}

fn normalize_arg(root: &Path, cwd: &Path, arg: &str) -> String {
    let candidate = if Path::new(arg).is_absolute() {
        Path::new(arg).to_path_buf()
    } else if root.join(arg).exists() {
        root.join(arg)
    } else {
        cwd.join(arg)
    };
    candidate
        .canonicalize()
        .ok()
        .and_then(|p| p.strip_prefix(root).ok().map(|r| r.to_path_buf()))
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .unwrap_or_else(|| arg.to_string())
}

fn status(ctx: &Ctx<'_>, json: bool) -> Result<bool> {
    let g = &ctx.graph;
    let progress = Progress::new(g, &ctx.index);
    let done_lines: usize = progress
        .done_files
        .iter()
        .map(|&f| g.files[f].analysis.lines)
        .sum();
    let module_tests: Vec<&crate::graph::TestFile> = g.tests.iter().filter(|t| !t.binary).collect();
    let ported = ctx.index.ported_tests();
    let tests_done = module_tests
        .iter()
        .filter(|t| ported.contains(t.rel.as_str()))
        .count();
    let units_done = (0..g.units.len())
        .filter(|&u| progress.unit_done(g, u))
        .count();
    let ready = progress.ready_units(g).len();
    let packages = by_package(g);
    if json {
        let per_package: BTreeMap<&String, serde_json::Value> = packages
            .iter()
            .map(|(p, files)| {
                let done = files
                    .iter()
                    .filter(|f| progress.done_files.contains(f))
                    .count();
                (p, json!({"files": files.len(), "done": done}))
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "files": {"total": g.files.len(), "done": progress.done_files.len(), "in_progress": progress.in_progress_files.len()},
                "lines": {"total": g.total_lines(), "done": done_lines},
                "units": {"total": g.units.len(), "done": units_done, "ready": ready},
                "module_tests": {"total": module_tests.len(), "ported": tests_done},
                "packages": per_package,
            }))?
        );
        return Ok(true);
    }
    let pct = |a: usize, b: usize| {
        if b == 0 {
            0.0
        } else {
            100.0 * a as f64 / b as f64
        }
    };
    println!(
        "Files        {:>5} / {:<5} ({:.1}%)  in progress: {}",
        progress.done_files.len(),
        g.files.len(),
        pct(progress.done_files.len(), g.files.len()),
        progress.in_progress_files.len()
    );
    println!(
        "TS lines     {:>6} / {:<6} ({:.1}%)",
        done_lines,
        g.total_lines(),
        pct(done_lines, g.total_lines())
    );
    if !ctx.index.upstream.stale.is_empty() {
        println!(
            "Stale        {:>5} ported module(s) changed on {} (`next --catch-up`)",
            ctx.index.upstream.stale.len(),
            ctx.ws.upstream()
        );
    }
    println!(
        "Units        {:>5} / {:<5} ready now: {ready}",
        units_done,
        g.units.len()
    );
    println!(
        "Module tests {:>5} / {:<5} ({:.1}%)",
        tests_done,
        module_tests.len(),
        pct(tests_done, module_tests.len())
    );
    if let Some(ui) = &ctx.ws.ui {
        let available = ui
            .widgets
            .iter()
            .filter(|w| w.status == WidgetStatus::Available)
            .count();
        println!(
            "UI library   {available} widget(s) available, {} planned ({})",
            ui.widgets.len() - available,
            ui.library.crate_name
        );
    }
    println!("\nBy package:");
    for (package, files) in &packages {
        let done = files
            .iter()
            .filter(|f| progress.done_files.contains(f))
            .count();
        let lines: usize = files.iter().map(|&f| g.files[f].analysis.lines).sum();
        println!(
            "  {package:<40} {done:>4} / {:<4} files  {lines:>7} lines",
            files.len()
        );
    }
    if !progress.in_progress_files.is_empty() {
        println!("\nIn progress:");
        for &f in &progress.in_progress_files {
            println!("  {}", g.files[f].rel);
        }
    }
    Ok(true)
}

fn batch_json(ctx: &Ctx<'_>, batch: &plan::Batch) -> serde_json::Value {
    let g = &ctx.graph;
    json!({
        "files": batch.files.iter().map(|&f| {
            let (file, module) = rust_target(ctx.ws, &g.packages, &g.files[f].rel);
            json!({
                "ts": g.files[f].rel,
                "lines": g.files[f].analysis.lines,
                "level": g.units[g.unit_of[f]].level,
                "update_since": ctx.index.upstream.stale.get(&g.files[f].rel),
                "added_upstream": ctx.index.upstream.added.contains(&g.files[f].rel),
                "rust_file": file,
                "rust_module": module,
                "exports": g.files[f].analysis.exports,
                "externals": g.files[f].externals,
                "constructs": g.files[f].analysis.hits,
            })
        }).collect::<Vec<_>>(),
        "lines": batch.lines,
        "footprint": plan::footprint(ctx.ws, g, &ctx.index, batch),
        "unlocks": batch.unlocks,
        "downstream": batch.downstream,
        "tests": batch.tests.iter().map(|&t| json!({
            "ts": g.tests[t].rel,
            "helpers": g.tests[t].helpers,
        })).collect::<Vec<_>>(),
    })
}

struct NextOptions<'a> {
    count: usize,
    scope: plan::Scope<'a>,
    show_brief: bool,
    independent: bool,
    write_briefs: Option<&'a Path>,
}

fn next(ctx: &Ctx<'_>, opts: NextOptions<'_>, json: bool) -> Result<bool> {
    let batches = if opts.independent {
        plan::independent_wave(ctx.ws, &ctx.graph, &ctx.index, opts.count, opts.scope)
    } else {
        plan::next_batches(ctx.ws, &ctx.graph, &ctx.index, opts.count, opts.scope)
    };
    if let Some(dir) = opts.write_briefs {
        std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
        for (i, batch) in batches.iter().enumerate() {
            let path = dir.join(format!("batch-{}.md", i + 1));
            let text = brief::render(ctx.ws, &ctx.graph, &ctx.index, &ctx.mappings, batch);
            std::fs::write(&path, text).with_context(|| format!("write {}", path.display()))?;
        }
    }
    if json {
        let list: Vec<_> = batches.iter().map(|b| batch_json(ctx, b)).collect();
        println!("{}", serde_json::to_string_pretty(&list)?);
        return Ok(true);
    }
    let g = &ctx.graph;
    if let Some(error) = &ctx.index.upstream.error {
        eprintln!("warning: upstream drift unknown, planning without it: {error}");
    }
    if opts.scope.catch_up {
        print_catch_up(ctx);
    }
    if batches.is_empty() {
        if opts.scope.catch_up {
            println!("Nothing to catch up is ready.");
        } else {
            println!("Nothing is ready: every remaining unit waits on an in-progress module.");
        }
        return Ok(true);
    }
    if opts.independent {
        println!(
            "Wave of {} independent batch(es): no two write the same Rust file, so they can be ported in parallel. Parent `mod` lines they share are merged by the lead.\n",
            batches.len()
        );
    }
    for (i, batch) in batches.iter().enumerate() {
        println!(
            "Batch {} — {} file(s), {} lines, unblocks {} (downstream {}), {} test file(s)",
            i + 1,
            batch.files.len(),
            batch.lines,
            batch.unlocks,
            batch.downstream,
            batch.tests.len()
        );
        for &f in &batch.files {
            let (file, _) = rust_target(ctx.ws, &g.packages, &g.files[f].rel);
            let kind = match ctx.index.upstream.stale.get(&g.files[f].rel) {
                Some(base) => format!("  (update since {})", &base[..base.len().min(12)]),
                None => String::new(),
            };
            println!(
                "  {:<70} {:>5} lines  → {file}{kind}",
                g.files[f].rel, g.files[f].analysis.lines
            );
        }
        for &t in &batch.tests {
            println!("  test: {}", g.tests[t].rel);
        }
        println!();
    }
    if let Some(dir) = opts.write_briefs {
        println!("Briefs written to {}/batch-<n>.md.", dir.display());
    } else if opts.show_brief {
        print!(
            "{}",
            brief::render(ctx.ws, g, &ctx.index, &ctx.mappings, &batches[0])
        );
    } else {
        println!("`rustify next --brief` prints the porting brief for batch 1.");
    }
    Ok(true)
}

/// What catching up with the upstream ref still involves, and what blocks the
/// parts that are not ready.
fn print_catch_up(ctx: &Ctx<'_>) {
    let g = &ctx.graph;
    let up = &ctx.index.upstream;
    let progress = Progress::new(g, &ctx.index);
    let catch_up = plan::catch_up_units(g, &ctx.index);
    let units: Vec<usize> = catch_up
        .iter()
        .copied()
        .filter(|&u| !progress.unit_done(g, u))
        .collect();
    let ready: BTreeSet<usize> = progress.ready_units(g).into_iter().collect();
    let claimed = units
        .iter()
        .filter(|&&u| progress.unit_claimed(g, u))
        .count();
    let ready_now = units.iter().filter(|u| ready.contains(u)).count();
    let new_in_graph: Vec<&String> = up
        .added
        .iter()
        .filter(|rel| {
            g.by_rel
                .get(rel.as_str())
                .is_some_and(|&f| !progress.done_files.contains(&f))
        })
        .collect();
    let deferred = new_in_graph
        .iter()
        .filter(|rel| !catch_up.contains(&g.unit_of[g.by_rel[rel.as_str()]]))
        .count();
    println!(
        "Catch-up with {}: {} stale module(s), {} new upstream module(s) ({deferred} more import never-ported code and are left to plain `next`); {} unit(s) left: {ready_now} ready, {claimed} in progress, {} waiting on other catch-up units.\n",
        ctx.ws.upstream(),
        up.stale.len(),
        new_in_graph.len() - deferred,
        units.len(),
        units.len() - ready_now - claimed,
    );
}

fn start(ctx: &mut Ctx<'_>, args: &[String], force: bool) -> Result<bool> {
    let files = ctx.files(args)?;
    let set: BTreeSet<usize> = files.iter().copied().collect();
    let progress = Progress::new(&ctx.graph, &ctx.index);
    let mut problems = Vec::new();
    for &f in &files {
        let unit = ctx.graph.unit_of[f];
        for &member in &ctx.graph.units[unit].files {
            if !set.contains(&member) && !progress.done_files.contains(&member) {
                problems.push(format!(
                    "{} is in an import cycle with {}; start them together",
                    ctx.graph.files[f].rel, ctx.graph.files[member].rel
                ));
            }
        }
        let file = &ctx.graph.files[f];
        for dep in file.deps.difference(&file.type_deps) {
            if !set.contains(dep) && !progress.done_files.contains(dep) {
                problems.push(format!(
                    "{} imports {}, which is not ported",
                    file.rel, ctx.graph.files[*dep].rel
                ));
            }
        }
    }
    if !problems.is_empty() && !force {
        for p in &problems {
            eprintln!("error: {p}");
        }
        eprintln!("Port dependencies first (`rustify next`), or pass --force.");
        return Ok(false);
    }
    for &f in &files {
        let rel = ctx.graph.files[f].rel.clone();
        let mut entry = ctx.index.get(&rel).cloned().unwrap_or(Entry {
            ts: rel.clone(),
            status: Status::InProgress,
            rust: None,
            module: None,
            ported_at: None,
            ts_base: None,
            ts_blobs: BTreeMap::new(),
            symbols: BTreeMap::new(),
            tests: Vec::new(),
            test_provenance: Vec::new(),
            notes: None,
        });
        entry.status = Status::InProgress;
        ctx.index.upsert(entry);
        println!("started {rel}");
    }
    ctx.index.save(ctx.ws)?;
    Ok(true)
}

#[allow(clippy::too_many_arguments)]
fn done(
    ctx: &mut Ctx<'_>,
    args: &[String],
    tests: &[String],
    test_provenance: &[String],
    maps: &[String],
    rust_override: Option<String>,
    verified: bool,
    notes: Option<String>,
) -> Result<bool> {
    let files = ctx.files(args)?;
    if rust_override.is_some() && files.len() != 1 {
        bail!("--rust applies to a single TS file");
    }
    if !test_provenance.is_empty() && files.len() != 1 {
        bail!("--test-provenance applies to a single TS file");
    }
    let set: BTreeSet<usize> = files.iter().copied().collect();
    let progress = Progress::new(&ctx.graph, &ctx.index);
    for &f in &files {
        let file = &ctx.graph.files[f];
        for dep in file.deps.difference(&file.type_deps) {
            if !set.contains(dep) && !progress.done_files.contains(dep) {
                bail!(
                    "{} imports {}, which is not ported; port dependencies first",
                    file.rel,
                    ctx.graph.files[*dep].rel
                );
            }
        }
        let missing = missing_types(ctx, f, &set, &progress)?;
        if !missing.is_empty() {
            bail!("{}", missing.join("\n"));
        }
    }
    let known_tests: BTreeSet<&str> = ctx.graph.tests.iter().map(|t| t.rel.as_str()).collect();
    let cwd = std::env::current_dir()?;
    let tests: Vec<String> = tests
        .iter()
        .map(|t| normalize_arg(&ctx.ws.root, &cwd, t))
        .collect();
    for t in &tests {
        if !known_tests.contains(t.as_str()) {
            bail!("{t} is not a module test in the port graph");
        }
    }
    let test_provenance: Vec<String> = test_provenance
        .iter()
        .map(|t| normalize_arg(&ctx.ws.root, &cwd, t))
        .collect();
    for t in &test_provenance {
        if !t.starts_with(&format!("{}/", ctx.ws.config.packages_dir))
            || !ctx
                .ws
                .config
                .test_markers
                .iter()
                .any(|marker| t.contains(marker))
            || !ctx.ws.root.join(t).is_file()
        {
            bail!("{t} is not an existing TypeScript test in the port workspace");
        }
    }
    let mut explicit: BTreeMap<String, String> = BTreeMap::new();
    for m in maps {
        let (ts, rs) = m
            .split_once('=')
            .with_context(|| format!("--map expects tsName=rust::path, got {m}"))?;
        explicit.insert(ts.to_string(), rs.to_string());
    }

    let ported_at = provenance::now_utc();
    let ts_base = provenance::ts_base(&ctx.ws.root, ctx.ws.upstream())?;
    let mut ok = true;
    for &f in &files {
        let rel = ctx.graph.files[f].rel.clone();
        // Re-running `done` (a fixup pass) keeps earlier `--map` entries.
        let previous_symbols = ctx
            .index
            .get(&rel)
            .map(|e| e.symbols.clone())
            .unwrap_or_default();
        let (default_file, default_module) = rust_target(ctx.ws, &ctx.graph.packages, &rel);
        let rust_file = rust_override.clone().unwrap_or(default_file);
        let crate_name = default_module
            .split("::")
            .next()
            .unwrap_or_default()
            .to_string();
        let module = if rust_override.is_some() {
            module_for_file(&crate_name, &rust_file)
        } else {
            default_module
        };
        let path = ctx.ws.crate_src(&module).join(&rust_file);
        let source = std::fs::read_to_string(&path)
            .with_context(|| format!("{rel}: expected Rust at {}", path.display()))?;
        let items = parse_items(&source)?;
        let names: BTreeSet<&str> = items.iter().map(|i| i.name.as_str()).collect();

        let mut symbols = BTreeMap::new();
        let mut unmapped = Vec::new();
        for export in &ctx.graph.files[f].analysis.exports {
            if let Some(rs) = explicit.get(&export.name) {
                symbols.insert(export.name.clone(), rs.clone());
                continue;
            }
            let candidates = [
                export.name.clone(),
                snake(&export.name),
                export.name.to_uppercase(),
            ];
            match candidates.iter().find(|c| names.contains(c.as_str())) {
                Some(found) => {
                    symbols.insert(export.name.clone(), format!("{module}::{found}"));
                }
                None if export.kind == "reexport" || export.kind == "default" => {}
                None => match previous_symbols.get(&export.name) {
                    Some(rs) => {
                        symbols.insert(export.name.clone(), rs.clone());
                    }
                    None => unmapped.push(export.name.clone()),
                },
            }
        }
        if !unmapped.is_empty() {
            eprintln!(
                "warning: {rel}: no Rust item found for {}; pass --map name=path if they were renamed or folded",
                unmapped.join(", ")
            );
        }
        let mut tracked_tests: BTreeSet<String> = ctx
            .index
            .get(&rel)
            .map(|e| e.tracked_tests().map(str::to_owned).collect())
            .unwrap_or_default();
        tracked_tests.extend(test_provenance.iter().cloned());
        // Keep provenance when a test-only import moves a ported test outside
        // the graph. `check` validates only graph-eligible `tests`, while drift
        // watches both lists after `done` re-stamps the module.
        let mut entry_tests = Vec::new();
        let mut off_graph_tests = Vec::new();
        for t in tracked_tests {
            if ctx
                .graph
                .tests
                .iter()
                .any(|test| test.rel == t && test.deps.contains(&f))
            {
                entry_tests.push(t);
            } else {
                off_graph_tests.push(t);
            }
        }
        for t in &tests {
            let covers = ctx
                .graph
                .tests
                .iter()
                .find(|x| &x.rel == t)
                .is_some_and(|x| x.deps.contains(&f));
            if covers && !entry_tests.contains(t) {
                entry_tests.push(t.clone());
            }
        }
        if !module_declared(&ctx.ws.crate_src(&module), &rust_file)? {
            eprintln!(
                "error: {rust_file} is not declared with `mod` in its parent module, so it is not compiled"
            );
            ok = false;
        }
        let tracked: Vec<&str> = std::iter::once(rel.as_str())
            .chain(entry_tests.iter().map(String::as_str))
            .chain(off_graph_tests.iter().map(String::as_str))
            .collect();
        let ts_blobs = provenance::working_blobs(&ctx.ws.root, &tracked)?;
        ctx.index.upsert(Entry {
            ts: rel.clone(),
            status: if verified {
                Status::Verified
            } else {
                Status::Ported
            },
            rust: Some(rust_file),
            module: Some(module),
            ported_at: Some(ported_at.clone()),
            ts_base: Some(ts_base.clone()),
            ts_blobs,
            symbols,
            tests: entry_tests,
            test_provenance: off_graph_tests,
            notes: notes
                .clone()
                .or_else(|| ctx.index.get(&rel).and_then(|e| e.notes.clone())),
        });
        println!("done {rel}");
    }
    ctx.index.save(ctx.ws)?;
    Ok(ok)
}

/// Types a module imports from not-yet-ported modules must already be
/// declared (types-first) in those modules' Rust files.
fn missing_types(
    ctx: &Ctx<'_>,
    f: usize,
    same_batch: &BTreeSet<usize>,
    progress: &Progress,
) -> Result<Vec<String>> {
    let file = &ctx.graph.files[f];
    let mut problems = Vec::new();
    for &dep in &file.type_deps {
        if same_batch.contains(&dep) || progress.done_files.contains(&dep) {
            continue;
        }
        let dep_rel = &ctx.graph.files[dep].rel;
        if ctx
            .index
            .status(dep_rel)
            .is_some_and(|s| matches!(s, Status::Replaced | Status::Skipped))
        {
            continue; // the replacement provides the types
        }
        let (rust_file, dep_module) = rust_target(ctx.ws, &ctx.graph.packages, dep_rel);
        let names = file.imported_names.get(&dep).cloned().unwrap_or_default();
        let path = ctx.ws.crate_src(&dep_module).join(&rust_file);
        let declared: BTreeSet<String> = match std::fs::read_to_string(&path) {
            Ok(source) => parse_items(&source)?.into_iter().map(|i| i.name).collect(),
            Err(_) => BTreeSet::new(),
        };
        let missing: Vec<&String> = names.iter().filter(|n| !declared.contains(*n)).collect();
        if !missing.is_empty() {
            problems.push(format!(
                "{} needs types from unported {dep_rel}; declare {} in {rust_file} first (types-first)",
                file.rel,
                missing.iter().map(|n| format!("`{n}`")).collect::<Vec<_>>().join(", ")
            ));
        }
    }
    Ok(problems)
}

/// Whether the parent module file declares `mod <name>` for `rust_file`.
fn module_declared(src: &Path, rust_file: &str) -> Result<bool> {
    if rust_file == "lib.rs" || rust_file == "main.rs" {
        return Ok(true);
    }
    let stem = rust_file.strip_suffix(".rs").unwrap_or(rust_file);
    let stem = stem.strip_suffix("/mod").unwrap_or(stem);
    let (parent_dir, name) = match stem.rsplit_once('/') {
        Some((dir, name)) => (Some(dir), name),
        None => (None, stem),
    };
    let parents = match parent_dir {
        None => vec![src.join("lib.rs"), src.join("main.rs")],
        Some(dir) => vec![src.join(format!("{dir}.rs")), src.join(dir).join("mod.rs")],
    };
    for parent in parents {
        if let Ok(text) = std::fs::read_to_string(&parent)
            && declared_modules(&text)?.iter().any(|m| m == name)
        {
            return Ok(true);
        }
    }
    Ok(false)
}

fn mark(
    ctx: &mut Ctx<'_>,
    args: &[String],
    status: Status,
    with: Option<String>,
    reason: Option<String>,
) -> Result<bool> {
    let files = ctx.files(args)?;
    let ported_at = provenance::now_utc();
    let ts_base = provenance::ts_base(&ctx.ws.root, ctx.ws.upstream())?;
    for f in files {
        let rel = ctx.graph.files[f].rel.clone();
        let ts_blobs = provenance::working_blobs(&ctx.ws.root, &[rel.as_str()])?;
        ctx.index.upsert(Entry {
            ts: rel.clone(),
            status,
            rust: None,
            module: with.clone(),
            ported_at: Some(ported_at.clone()),
            ts_base: Some(ts_base.clone()),
            ts_blobs,
            symbols: BTreeMap::new(),
            tests: Vec::new(),
            test_provenance: Vec::new(),
            notes: reason.clone(),
        });
        println!("{status:?} {rel}");
    }
    ctx.index.save(ctx.ws)?;
    Ok(true)
}

fn find(ctx: &Ctx<'_>, query: &str, json: bool) -> Result<bool> {
    // Match the query as typed and in snake_case, so a TS name
    // (`parseRunnerSpec`) also finds its Rust port (`parse_runner_spec`).
    let q = query.to_lowercase();
    let q_snake = snake(query);
    let matches = |s: &str| {
        let s = s.to_lowercase();
        s.contains(&q) || s.contains(&q_snake)
    };
    let mut results: Vec<serde_json::Value> = Vec::new();

    for entry in &ctx.index.modules {
        for (ts, rs) in &entry.symbols {
            if matches(ts) || matches(rs) {
                results.push(json!({"source": "index", "ts": ts, "rust": rs, "file": entry.ts, "status": entry.status}));
            }
        }
    }
    for file in &ctx.graph.files {
        for export in &file.analysis.exports {
            if matches(&export.name) {
                let status = ctx.index.status(&file.rel);
                let (_, module) = rust_target(ctx.ws, &ctx.graph.packages, &file.rel);
                results.push(json!({
                    "source": "typescript",
                    "ts": export.name,
                    "kind": export.kind,
                    "file": format!("{}:{}", file.rel, export.line),
                    "status": status,
                    "rust_target": module,
                }));
            }
        }
    }
    let srcs: Vec<std::path::PathBuf> = ctx.ws.crates().into_iter().map(|(_, src)| src).collect();
    for entry in srcs
        .iter()
        .flat_map(|src| WalkDir::new(src).into_iter().filter_map(Result::ok))
    {
        if entry.path().extension().is_none_or(|e| e != "rs") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(entry.path()) else {
            continue;
        };
        for item in parse_items(&text)? {
            if matches(&item.name) {
                let rel = entry
                    .path()
                    .strip_prefix(&ctx.ws.root)
                    .unwrap_or(entry.path());
                results.push(json!({
                    "source": "rust",
                    "rust": item.name,
                    "kind": item.kind,
                    "file": format!("{}:{}", rel.display(), item.line),
                }));
            }
        }
    }
    for rule in &ctx.mappings.packages {
        if matches(&rule.npm) || matches(&rule.rust) {
            results.push(json!({"source": "mapping", "ts": rule.npm, "rust": rule.rust, "crates": rule.crates}));
        }
    }
    for rule in &ctx.mappings.constructs {
        if matches(&rule.id) || matches(&rule.title) {
            results.push(json!({"source": "construct", "ts": rule.title, "rust": rule.rust}));
        }
    }
    if let Some(ui) = &ctx.ws.ui {
        for w in &ui.widgets {
            if matches(&w.name) || matches(&w.rust) || w.from.iter().any(|f| matches(f)) {
                results.push(json!({"source": "widget", "ts": w.from.join(", "), "rust": w.rust, "status": w.status}));
            }
        }
        for rule in &ui.ink {
            if rule.names.iter().any(|n| matches(n)) {
                results
                    .push(json!({"source": "ink", "ts": rule.names.join(", "), "rust": rule.rust}));
            }
        }
    }
    if json {
        println!("{}", serde_json::to_string_pretty(&results)?);
        return Ok(true);
    }
    if results.is_empty() {
        println!("No matches for {query}.");
    }
    for r in &results {
        let get = |k: &str| r.get(k).and_then(|v| v.as_str()).unwrap_or("");
        match get("source") {
            "index" => println!(
                "index       {} → {}  ({})",
                get("ts"),
                get("rust"),
                get("file")
            ),
            "typescript" => println!(
                "typescript  {} {} at {}  [{}] → {}",
                get("kind"),
                get("ts"),
                get("file"),
                r.get("status")
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| "null".into())
                    .trim_matches('"'),
                get("rust_target")
            ),
            "rust" => println!(
                "rust        {} {} at {}",
                get("kind"),
                get("rust"),
                get("file")
            ),
            "mapping" => println!("mapping     {} → {}", get("ts"), get("rust")),
            "widget" => println!(
                "widget      {} [{}] ← {}",
                get("rust"),
                get("status"),
                get("ts")
            ),
            "ink" => println!("ink         {} → {}", get("ts"), get("rust")),
            _ => println!("construct   {} → {}", get("ts"), get("rust")),
        }
    }
    Ok(true)
}

/// Ported modules whose TS file or ported tests changed at `against` since
/// the port recorded them (blob comparison, or `ts_base` diff for entries
/// without `ts_blobs`; see [`crate::index::stale_modules`]).
fn drift(ctx: &Ctx<'_>, against: &str, json: bool) -> Result<bool> {
    let mut drifted = Vec::new();
    let mut unstamped = Vec::new();
    let mut stamped = Vec::new();
    for entry in ctx.index.modules.iter().filter(|e| e.status.is_done()) {
        if entry.ts_base.is_some() {
            stamped.push(entry);
        } else {
            unstamped.push(entry.ts.clone());
        }
    }
    let stale = crate::index::stale_modules(&ctx.ws.root, stamped.iter().copied(), against)?;
    for entry in stamped {
        let (Some(base), Some(changed)) = (&entry.ts_base, stale.get(&entry.ts)) else {
            continue;
        };
        let paths: Vec<&str> = entry.tracked_paths().collect();
        drifted.push(json!({
            "ts": entry.ts,
            "status": entry.status,
            "ported_at": entry.ported_at,
            "ts_base": base,
            "changed": changed,
            "diff": format!("git diff {base} {against} -- {}", paths.join(" ")),
        }));
    }
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "against": against,
                "drifted": drifted,
                "unstamped": unstamped,
            }))?
        );
        return Ok(true);
    }
    if drifted.is_empty() {
        println!("No ported module changed on {against} since it was ported.");
    } else {
        println!(
            "{} ported module(s) changed on {against} since they were ported:",
            drifted.len()
        );
        for d in &drifted {
            let base = d["ts_base"].as_str().unwrap_or_default();
            println!(
                "  {}  (ported {} from {})",
                d["ts"].as_str().unwrap_or_default(),
                d["ported_at"].as_str().unwrap_or("?"),
                &base[..base.len().min(12)]
            );
            let changed: Vec<&str> = d["changed"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|c| c.as_str())
                .collect();
            println!("    changed: {}", changed.join(", "));
            println!("    {}", d["diff"].as_str().unwrap_or_default());
        }
        println!(
            "Port each diff into the Rust module and its tests, then re-run `rustify done` on the file to re-stamp it."
        );
    }
    if !unstamped.is_empty() {
        println!(
            "{} ported module(s) have no ts_base; re-run `rustify done` on them to stamp.",
            unstamped.len()
        );
    }
    Ok(true)
}

/// Backfill `ts_blobs` for done entries that predate it, from the blobs at
/// their `ts_base` (the TS that was ported). Entries that already have a
/// record, or have no `ts_base`, are left alone.
fn stamp_blobs(ctx: &mut Ctx<'_>) -> Result<bool> {
    let todo: Vec<usize> = ctx
        .index
        .modules
        .iter()
        .enumerate()
        .filter(|(_, e)| e.status.is_done() && e.ts_blobs.is_empty() && e.ts_base.is_some())
        .map(|(i, _)| i)
        .collect();
    let mut queries: Vec<(&str, &str)> = Vec::new();
    for &i in &todo {
        let entry = &ctx.index.modules[i];
        let base = entry.ts_base.as_deref().unwrap_or_default();
        queries.extend(entry.tracked_paths().map(|p| (base, p)));
    }
    let mut blobs = provenance::blobs_at(&ctx.ws.root, &queries)?.into_iter();
    let mut filled: Vec<(usize, BTreeMap<String, String>)> = Vec::new();
    for &i in &todo {
        let entry = &ctx.index.modules[i];
        let map = entry
            .tracked_paths()
            .filter_map(|p| blobs.next().flatten().map(|blob| (p.to_owned(), blob)))
            .collect();
        filled.push((i, map));
    }
    let count = filled.len();
    for (i, map) in filled {
        ctx.index.modules[i].ts_blobs = map;
    }
    ctx.index.save(ctx.ws)?;
    println!("stamped ts_blobs on {count} module(s)");
    Ok(true)
}

fn check(ctx: &Ctx<'_>) -> Result<bool> {
    let mut errors = crate::ratchet::unported_imports(&ctx.graph, &ctx.index);
    let mut warnings = Vec::new();
    let progress = Progress::new(&ctx.graph, &ctx.index);
    for entry in &ctx.index.modules {
        let Some(&f) = ctx.graph.by_rel.get(&entry.ts) else {
            if ctx.ws.root.join(&entry.ts).exists() {
                warnings.push(format!(
                    "{} is indexed but no longer reached from the roots",
                    entry.ts
                ));
            } else {
                errors.push(format!("{} is indexed but the TS file is gone", entry.ts));
            }
            continue;
        };
        if entry.status.is_done() && entry.ts_base.is_none() {
            warnings.push(format!(
                "{} has no ts_base; re-run `rustify done` on it so `drift` can track it",
                entry.ts
            ));
        }
        if entry.status.is_done() && entry.status.has_rust() {
            errors.extend(missing_types(ctx, f, &BTreeSet::new(), &progress)?);
        }
        if entry.status.has_rust() {
            let Some(rust_file) = &entry.rust else {
                errors.push(format!(
                    "{} is {:?} without a rust file",
                    entry.ts, entry.status
                ));
                continue;
            };
            let module = entry.module.clone().unwrap_or_default();
            let path = ctx.ws.crate_src(&module).join(rust_file);
            let Ok(source) = std::fs::read_to_string(&path) else {
                errors.push(format!("{}: {} does not exist", entry.ts, path.display()));
                continue;
            };
            let names: BTreeSet<String> =
                parse_items(&source)?.into_iter().map(|i| i.name).collect();
            for (ts, rs) in &entry.symbols {
                let local = rs.strip_prefix(&format!("{module}::")).unwrap_or(rs);
                if rs.starts_with(&format!("{module}::")) && !names.contains(local) {
                    errors.push(format!(
                        "{}: `{ts}` maps to {rs}, which is not in {rust_file}",
                        entry.ts
                    ));
                }
            }
            if !module_declared(&ctx.ws.crate_src(&module), rust_file)? {
                errors.push(format!(
                    "{rust_file} is not declared with `mod` in its parent, so it is not compiled"
                ));
            }
            for export in &ctx.graph.files[f].analysis.exports {
                if !matches!(export.kind, "reexport" | "default")
                    && !entry.symbols.contains_key(&export.name)
                {
                    warnings.push(format!(
                        "{}: export `{}` has no Rust mapping",
                        entry.ts, export.name
                    ));
                }
            }
        }
        for t in &entry.tests {
            if !ctx.graph.tests.iter().any(|x| &x.rel == t) {
                errors.push(format!("{}: test {t} is not in the graph", entry.ts));
            }
        }
    }
    if let Some(ui) = &ctx.ws.ui {
        check_ui(ctx, ui, &mut errors)?;
    }
    // Two TS files must not land on the same Rust module (e.g. `hooks.ts`
    // next to `hooks/index.ts`); rustc would reject both `hooks.rs` and
    // `hooks/mod.rs`.
    let mut targets: BTreeMap<String, Vec<&str>> = BTreeMap::new();
    for file in &ctx.graph.files {
        let (_, module) = rust_target(ctx.ws, &ctx.graph.packages, &file.rel);
        targets.entry(module).or_default().push(&file.rel);
    }
    for (module, files) in &targets {
        if files.len() > 1 {
            errors.push(format!(
                "{} all map to {module}; give one an explicit --rust path when porting and note it",
                files.join(", ")
            ));
        }
    }
    let mut unmapped: BTreeMap<&str, usize> = BTreeMap::new();
    for file in &ctx.graph.files {
        for name in &file.externals {
            if ctx.mappings.package_for(name).is_none() {
                *unmapped.entry(name.as_str()).or_default() += 1;
            }
        }
    }
    for (name, count) in unmapped {
        warnings.push(format!(
            "external `{name}` ({count} file(s)) has no [[package]] mapping"
        ));
    }
    for w in &warnings {
        println!("warning: {w}");
    }
    for e in &errors {
        println!("error: {e}");
    }
    println!(
        "{} indexed module(s), {} error(s), {} warning(s)",
        ctx.index.modules.len(),
        errors.len(),
        warnings.len()
    );
    Ok(errors.is_empty())
}

/// The component library's own invariants: routed modules exist, available
/// widgets exist and have goldens, every golden is read by a Rust test, and
/// every ported `.tsx` has a golden parity test.
fn check_ui(ctx: &Ctx<'_>, ui: &Components, errors: &mut Vec<String>) -> Result<()> {
    let lib_src = ctx.ws.root.join(&ui.library.krate).join("src");
    let goldens = ctx.ws.root.join(&ui.library.goldens);
    for m in &ui.modules {
        if !ctx.graph.by_rel.contains_key(&m.ts) {
            errors.push(format!(
                "components.toml routes {}, which is not a source file in the port graph",
                m.ts
            ));
        }
    }
    for w in &ui.widgets {
        for from in &w.from {
            if !ctx.graph.by_rel.contains_key(from) {
                errors.push(format!(
                    "widget {} replaces {from}, which is not in the port graph",
                    w.name
                ));
            }
        }
        if w.status != WidgetStatus::Available {
            continue;
        }
        let path = lib_src.join(&w.file);
        match std::fs::read_to_string(&path) {
            Ok(source) => {
                if !parse_items(&source)?.iter().any(|i| i.name == w.name) {
                    errors.push(format!(
                        "widget {} is available but {} does not define it",
                        w.name,
                        path.display()
                    ));
                }
            }
            Err(_) => errors.push(format!(
                "widget {} is available but {} does not exist",
                w.name,
                path.display()
            )),
        }
        match &w.golden {
            Some(golden) if !goldens.join(golden).is_file() => errors.push(format!(
                "widget {} names golden {golden}, which is not in {}",
                w.name, ui.library.goldens
            )),
            Some(_) => {}
            None => errors.push(format!(
                "widget {} is available without a golden parity test",
                w.name
            )),
        }
    }
    // Every golden must be read by some Rust test through `golden::cases`,
    // `golden::case`, or `golden::assert_file`, or Ink changes go unchecked.
    // A mention in a comment does not count. The reader is not always in the
    // library crate: a small UI piece that is not shared stays local to the
    // crate it is used from while still comparing against a golden fixture
    // under `goldens/`. So this
    // scans every crate the port writes into (`Workspace::crates`), not
    // only the component library's own `src/`.
    let mut rust_sources = String::new();
    for (_, src) in ctx.ws.crates() {
        for entry in WalkDir::new(&src).into_iter().filter_map(Result::ok) {
            if entry.path().extension().is_some_and(|e| e == "rs") {
                rust_sources.push_str(&std::fs::read_to_string(entry.path()).unwrap_or_default());
            }
        }
    }
    if let Ok(dir) = std::fs::read_dir(&goldens) {
        for entry in dir.filter_map(Result::ok) {
            let name = entry.file_name().to_string_lossy().to_string();
            let read = ["cases(", "case(", "assert_file("]
                .iter()
                .any(|call| rust_sources.contains(&format!("{call}\"{name}\"")));
            if !read {
                errors.push(format!(
                    "golden {}/{name} is not read by any Rust test in a crate the port writes into",
                    ui.library.goldens
                ));
            }
        }
    }
    for entry in &ctx.index.modules {
        if !(entry.ts.ends_with(".tsx") || entry.ts.ends_with(".jsx")) || !entry.status.has_rust() {
            continue;
        }
        let (Some(rust), Some(module)) = (&entry.rust, &entry.module) else {
            continue;
        };
        let source =
            std::fs::read_to_string(ctx.ws.crate_src(module).join(rust)).unwrap_or_default();
        if !source.contains("golden::") {
            errors.push(format!(
                "{} is ported but {rust} has no golden parity test ({}::golden)",
                entry.ts, ui.library.crate_name
            ));
        }
    }
    Ok(())
}

fn graph_summary(ctx: &Ctx<'_>, json: bool) -> Result<bool> {
    let g = &ctx.graph;
    if json {
        let files: Vec<_> = g
            .files
            .iter()
            .enumerate()
            .map(|(i, f)| {
                json!({
                    "ts": f.rel,
                    "package": f.package,
                    "lines": f.analysis.lines,
                    "unit": g.unit_of[i],
                    "level": g.units[g.unit_of[i]].level,
                    "deps": f.deps.iter().map(|&d| &g.files[d].rel).collect::<Vec<_>>(),
                    "type_deps": f.type_deps.iter().map(|&d| &g.files[d].rel).collect::<Vec<_>>(),
                    "externals": f.externals,
                    "unresolved": f.unresolved,
                })
            })
            .collect();
        let tests: Vec<_> = g
            .tests
            .iter()
            .map(|t| json!({"ts": t.rel, "binary": t.binary, "deps": t.deps.iter().map(|&d| &g.files[d].rel).collect::<Vec<_>>(), "helpers": t.helpers}))
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({"files": files, "tests": tests}))?
        );
        return Ok(true);
    }
    let max_level = g.units.iter().map(|u| u.level).max().unwrap_or(0);
    let mut levels = vec![0usize; max_level + 1];
    for u in &g.units {
        levels[u.level] += 1;
    }
    println!(
        "{} source files ({} lines) in {} units; {} module tests, {} binary tests; depth {}",
        g.files.len(),
        g.total_lines(),
        g.units.len(),
        g.tests.iter().filter(|t| !t.binary).count(),
        g.tests.iter().filter(|t| t.binary).count(),
        max_level
    );
    println!("\nUnits per level (0 = no in-graph dependencies):");
    for (level, count) in levels.iter().enumerate() {
        println!("  {level:>3}: {count}");
    }
    let mut cycles: Vec<&crate::graph::Unit> =
        g.units.iter().filter(|u| u.files.len() > 1).collect();
    cycles.sort_by_key(|u| std::cmp::Reverse(u.files.len()));
    if !cycles.is_empty() {
        println!("\nImport cycles (ported as one unit):");
        for u in cycles.iter().take(10) {
            let first = &g.files[u.files[0]].rel;
            println!("  {} files, {} lines, e.g. {first}", u.files.len(), u.lines);
        }
        if cycles.len() > 10 {
            println!("  … {} more", cycles.len() - 10);
        }
    }
    let mut constructs: BTreeMap<&str, (usize, usize)> = ctx
        .mappings
        .constructs
        .iter()
        .map(|r| (r.id.as_str(), (0, 0)))
        .collect();
    for f in &g.files {
        let mut seen = BTreeSet::new();
        for hit in &f.analysis.hits {
            if let Some(entry) = constructs.get_mut(hit.rule.as_str()) {
                entry.0 += 1;
                if seen.insert(hit.rule.as_str()) {
                    entry.1 += 1;
                }
            }
        }
    }
    println!("\nTypeScript constructs (occurrences, files):");
    for (id, (hits, files)) in &constructs {
        println!("  {id:<22} {hits:>6} {files:>5}");
    }
    let mut externals: BTreeMap<&str, usize> = BTreeMap::new();
    for f in &g.files {
        for e in &f.externals {
            *externals.entry(e.as_str()).or_default() += 1;
        }
    }
    println!("\nExternal imports:");
    for (name, count) in &externals {
        let mapping = ctx
            .mappings
            .package_for(name)
            .map(|r| r.rust.as_str())
            .unwrap_or("NO MAPPING");
        println!("  {name:<40} {count:>4} file(s)  → {mapping}");
    }
    let unresolved: BTreeSet<(&str, &str)> = g
        .files
        .iter()
        .flat_map(|f| {
            f.unresolved
                .iter()
                .map(move |u| (f.rel.as_str(), u.as_str()))
        })
        .collect();
    if !unresolved.is_empty() {
        println!("\nUnresolved imports:");
        for (file, spec) in unresolved.iter().take(20) {
            println!("  {file}: {spec}");
        }
        if unresolved.len() > 20 {
            println!("  … {} more", unresolved.len() - 20);
        }
    }
    Ok(true)
}
