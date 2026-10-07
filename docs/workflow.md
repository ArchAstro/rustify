# Porting workflow

This guide covers a port from setup to keeping it current. For every
configuration key, see [configuration.md](configuration.md).

1. [Set up](#1-set-up)
2. [How rustify sees your code](#2-how-rustify-sees-your-code)
3. [Port a batch](#3-port-a-batch)
4. [Files that should not be ported](#4-files-that-should-not-be-ported)
5. [Port in parallel with agents](#5-port-in-parallel-with-agents)
6. [Keep the port current](#6-keep-the-port-current)
7. [The CI ratchet](#7-the-ci-ratchet)
8. [UI components](#8-ui-components)
9. [End-to-end tests](#9-end-to-end-tests)
10. [Compare the two programs](#10-compare-the-two-programs)
11. [Errors and what to do](#11-errors-and-what-to-do)

## 1. Set up

1. Make sure the repository has at least one commit and the upstream ref
   exists (`git fetch origin main`, or `git update-ref
   refs/remotes/origin/main HEAD` for a local experiment). `done`, `drift`,
   and `ratchet` need it.
2. Create the Rust crate the port goes into (`cargo new --lib rust/app`).
3. Copy [`examples/typescript/rustify.toml`](../examples/typescript/rustify.toml)
   into your repository and fill in:
   - `roots`: the TS source directories that must be ported.
   - `packages_dir`: the directory holding your workspace packages (each with
     a `package.json`). For a single-package repo, this is the directory that
     contains that package.
   - `primary_package`: the `name` from your main `package.json`.
   - `rust_crate` / `rust_crate_name`: the crate directory and its name.
4. Copy [`examples/typescript/mappings.toml`](../examples/typescript/mappings.toml)
   into `state_dir` (by default the directory holding `rustify.toml`).
5. Run `rustify graph`. It prints how many files and units it found and how
   often each mapping rule matches. If the counts look wrong, fix `roots`,
   `exclude`, or `test_markers` before porting anything.
6. Run `rustify check`. Its warnings list every npm package your code imports
   that `mappings.toml` has no rule for. Add a `[[package]]` rule for each,
   naming the crate you will use.

Where to put `rustify.toml`: `rustify` searches the current directory and its
parents. Put it at the repository root, or next to your Rust crate if you run
commands from there. Paths inside it are always relative to the repository
root (the git top level), wherever the file lives.

## 2. How rustify sees your code

**Files.** Every `.ts`/`.tsx` file (plus any JavaScript extensions you add
to `extensions`) reachable from `roots` is in the graph,
including files in other workspace packages that `roots` import. Files
matching `test_markers` are tests; tests matching `binary_test_markers` are
end-to-end tests that run against your binary and are never assigned to a
batch.

**Imports.** Relative imports resolve to files. A bare import (`@acme/kit/x`)
is a workspace package only if the importing package declares it as
`workspace:*`; it then resolves through that package's `exports` (or `main`),
mapping `dist/*.js` back to `src/*.ts`. Anything else is external: an npm
package or a Node builtin, looked up in `mappings.toml`.

**Units.** Files that import each other in a cycle form one unit and are
ported together, because neither compiles in Rust without the other.

**Type-only imports** (`import type`, or imports used only as types) do not
order the port. If `a.ts` uses only the `Store` type from `store.ts`, `a.ts`
can be ported first: you declare `Store` in `store.rs` now and port the rest
of `store.ts` later. The brief lists these as "types from modules not ported
yet".

**Where Rust goes.** Each TS file maps to one Rust file by mirroring its path:

| TypeScript | Rust file | Module |
|---|---|---|
| `packages/app/src/config.ts` (primary package) | `config.rs` | `app::config` |
| `packages/app/src/local-daemon/store.ts` | `local_daemon/store.rs` | `app::local_daemon::store` |
| `packages/app/src/hooks/index.ts` | `hooks/mod.rs` | `app::hooks` |
| `packages/kit/src/helpers.ts` (other package `@acme/kit`) | `kit/helpers.rs` | `app::kit::helpers` |

Names become `snake_case`; a Rust keyword gets a trailing `_`. Use
`done --rust <file>` when one file must land elsewhere; `check` reports two TS
files that would land on the same module.

**Ranking.** A unit is ready when every unit it imports behavior from is
ported. `next` ranks ready units by how many units they unblock, groups
neighbours from the same directory into batches within `[batch]` limits, and
lists the TS tests that become portable once the batch lands.

## 3. Port a batch

```bash
rustify next --brief                 # or: rustify brief <file>...
rustify start <files>
# write the Rust and port the tests
cargo test -p <crate>
rustify done <files> --test <ts test>...
rustify check
```

**The brief** gives, per file:

1. The Rust file and module path.
2. Each export and the Rust name it should get.
3. Each import: ported ones with the Rust paths their names map to, batch
   members, and type-only imports to declare first.
4. External imports with the crate to use, from `mappings.toml`. "NO MAPPING"
   means you need to choose a crate and add a `[[package]]` rule.
5. TypeScript constructs found in the file, by line, with the Rust pattern and
   the semantic hazards for each.
6. The TS tests to port with the batch, and their helper files.

**`start`** marks files `in_progress` so `next` stops offering them. It
refuses when a dependency is not ported, or when a type the file needs from an
unported module is not declared in Rust yet.

**Porting.** Add `mod` declarations so the new file compiles. Port each TS
test case one for one as a Rust `#[test]`: same name, same assertions. A
module with no TS tests gets Rust unit tests for its exports; the brief says
so. rustify does not run or inspect your Rust tests; `--test` records which TS
tests were ported so later edits to them make the module stale.

**`done`** records the port. It:

1. Refuses if a dependency is still unported, a needed type is not declared,
   or a `--test` file is not a test of the module.
2. Finds a Rust item for each TS export by name (`slugify` → `slugify`,
   `Store` → `Store`). Exports it cannot find are warned about; record them
   with `--map tsName=app::path::item`, or `--map tsName=-` when there is
   deliberately no Rust counterpart (say why with `--notes`).
3. Errors if the Rust file is not declared with `mod` in its parent.
4. Stamps the entry with `ts_base` (the merge base of `HEAD` and the
   upstream ref) and `ts_blobs` (git blob ids of the TS file and its tests).

`done` can be re-run at any time; it keeps earlier `--map` entries.

**`check`** verifies that every indexed Rust file exists and is compiled,
every recorded symbol exists in it, no ported module imports an unported one,
and every TS file still exists. Exports without a mapping and externals
without a rule are warnings. Run it before committing.

**`find <name>`** looks a name up everywhere at once, e.g. to see where a TS
helper ended up: `rustify find parseConfig` finds `parse_config` too.

## 4. Files that should not be ported

- `rustify skip <file> --reason "bun entrypoint shim"`: not needed in Rust.
- `rustify replace <file> --with tokio::process --reason "..."`: behavior comes
  from a crate or std instead of a port.
- `exclude` in `rustify.toml`: never put the file in the graph at all.
- `external_packages`: a whole workspace package (a generated SDK, say)
  replaced by a Rust crate. Give it a `[[package]]` rule.

Skipped and replaced files count as done, so their dependents become ready.

## 5. Port in parallel with agents

```bash
rustify next --independent --count 6 --write-briefs briefs/
```

`--independent` returns a wave of batches that write disjoint Rust files, so
agents can port them at the same time without conflicting. Two batches that
only add `mod` lines to the same parent (`lib.rs`, a `mod.rs`) can share a
wave; merge those lines when combining the work. `--write-briefs` writes
`briefs/batch-1.md`, `batch-2.md`, ..., each a self-contained prompt.

A workable loop for a lead agent:

1. `next --independent --write-briefs`.
2. Run `start` for every batch, then hand each brief to one agent.
3. Each agent ports, tests, and runs `done` for its batch.
4. The lead merges, runs `check` and the full test suite, and commits.

`--json` on `next` includes each batch's `footprint` (`writes`, `registers`)
if you are scheduling agents yourself.

## 6. Keep the port current

While you port, the TypeScript keeps changing. A ported module is **stale**
when its TS file or one of its recorded tests no longer matches the blob ids
`done` recorded. Blobs compare content, so rebasing or squashing commits does
not make anything stale; only an actual edit does.

```bash
git fetch origin main
rustify drift            # stale modules, what changed, and the git diff to read
rustify next --catch-up  # re-port plan, in dependency order
```

`drift` output:

```
1 ported module(s) changed on origin/main since they were ported:
  packages/app/src/slug.ts  (ported 2026-10-06T21:06:42Z from 27f904fccdd7)
    changed: packages/app/src/slug.ts
    git diff 27f904fccdd700cb734b50d0b65ab9e190ece50b origin/main -- packages/app/src/slug.ts packages/app/src/__tests__/slug.test.ts
Port each diff into the Rust module and its tests, then re-run `rustify done` on the file to re-stamp it.
```

For a stale module the brief shows the TS diff since it was ported instead of
a full port. Port the diff into the existing Rust and tests, then run
`rustify done <file>` to re-stamp it. `--catch-up` also offers modules that
were added upstream after the port began.

`status` shows the stale count, and `next` treats stale modules as not done,
so their dependents wait until they are current.

## 7. The CI ratchet

`rustify ratchet --base <ref>` compares `HEAD` with its merge base with
`<ref>` and fails when the branch:

1. changes the TS or a recorded test of a module that was current at the
   merge base, without re-running `done`,
2. deletes or renames the TS of a ported module without updating its entry,
3. removes a current port's entry, or turns it into an entry without Rust,
   while the TS still exists,
4. makes a ported module import a TS module that is not ported.

Modules already stale at the merge base are not reported, so a project can
adopt the ratchet with a backlog: the stale count can only go down.

To set it up, copy
[`examples/ci/rustify-ratchet.yml`](../examples/ci/rustify-ratchet.yml) to
`.github/workflows/` and:

1. Replace `<commit>` with the rustify commit to pin.
2. Set `RUSTIFY_CONFIG` if `rustify.toml` is not at the repository root.
3. Keep `fetch-depth: 0`: the ratchet needs history to find the merge base.
4. Make the job a required check in branch protection.

Run the same check locally before pushing:

```bash
git fetch origin main
rustify ratchet
```

The PR that first adds `rustify.toml` passes: the merge base has no index, so
nothing can become newly stale.

## 8. UI components

Skip this unless you are porting UI components (Ink, React) onto a Rust UI
library such as ratatui.

Add a `components.toml` to `state_dir` (start from
[`examples/typescript/components.toml`](../examples/typescript/components.toml)).
It lets you:

- **Route shared components into a library crate.** A `[[module]]` entry sends
  that TS file to the library crate instead of the app crate.
- **Point ports at existing widgets.** A `[[widget]]` names a Rust widget and
  the TS components it replaces. Briefs for those components, and for files
  importing them, say to reuse it.
- **Map UI APIs.** An `[[ink]]` rule maps imported names (`Box`, `useInput`)
  from any `ui_packages` specifier to their Rust equivalent, with hazards.

Briefs for `.tsx` files and files importing a `ui_packages` specifier get a
UI section with these, plus how to prove rendering parity: a TS test renders
each component into golden text frames (regenerated with `golden_command`),
and the Rust test renders the same fixtures and compares.

With a `components.toml`, `check` also requires:

1. Every widget marked `available` exists in its file and names a golden file.
2. Every file in `goldens` is read by a Rust test through a call written as
   `cases("name.txt")`, `case("name.txt")`, or `assert_file("name.txt")`.
   Your library provides those helpers in a `golden` module.
3. Every ported `.tsx` module's Rust file references `golden::`.

## 9. End-to-end tests

Run this before porting anything:

```bash
rustify e2e
```

A test that starts the program as a process and asserts on its output can run
against the Rust binary without changes. A test that imports the program's
source cannot: it is ported with its module, and it stops checking the
TypeScript once that module is gone. `e2e` sorts every test file into three
groups:

| Group | Meaning | What to do |
|---|---|---|
| Process tests with no source imports | Imports a process spawner (`node:child_process`, `execa`, `node-pty`, ...) and no source file or workspace package, directly or through helpers | Keep them. They are the port's acceptance suite. `needs:` lists the npm packages each one imports |
| Process tests that also import source | Starts the program and also imports the listed files | Replace each import with something observable from outside (output, files), or the test cannot run against Rust |
| Never start a process | Module tests | Ported with their modules |

```
38 test file(s): 5 run the program as a process without importing its source, 2 run it but also import source, 31 never start a process

Process tests with no source imports (can run against the Rust binary as they are):
  packages/app/src/cli.e2e.test.ts  (174 lines)  needs: sharp
  ...
Process tests that also import source (cannot run against the Rust binary until these imports are removed):
  packages/app/test/doctor.test.mjs
    imports packages/app/bin/doctor.mjs
```

What `e2e` can and cannot see:

- It reads files whose name contains `.test.` or `.spec.`, with an extension
  listed in `extensions`, under `roots` and every workspace package. Add
  `mjs` or `js` to `extensions` if the tests are JavaScript.
- A test starts a process when it or a helper imports a spawner, imports
  `$`/`spawn` from `bun`, or mentions `Bun.spawn`, `Bun.$`, or
  `Deno.Command`. A wrapper from an npm package it does not know is missed.
- `import type` does not count as importing source. A specifier starting
  with `@/`, `~/`, or `#` (a path alias) does.

Check three things by hand for the first group:

1. **Coverage.** List every command, flag, exit code, and file the program
   writes, and mark which a test asserts on. Write tests for the rest while
   the TypeScript is still the reference.
2. **Which program they run.** A test that hard-codes `node dist/cli.js`
   tests only the TypeScript. Make each one read the command from an
   environment variable so CI can run the same file against both builds.
3. **What else they need.** A test that needs the network, an account, or a
   tool CI does not install will be skipped there. Replace the dependency
   with a local stand-in or note it as uncovered.

Add the directory or suffix of these tests to `binary_test_markers`, so
`next` never hands them to a batch; `e2e` lists the ones that match no marker.

If the first group is empty, write these tests before porting. Without them
the only check on the finished port is `rustify compare`.

## 10. Compare the two programs

When the port is complete (`rustify status` shows nothing left), run the same
commands through both programs:

```bash
rustify compare                    # every case in compare.toml
rustify compare --case help        # one case (repeatable)
rustify compare --keep out/        # also write out/<case>/{old,new}.{stdout,stderr,exit,files}
```

Cases live in `compare.toml` in `state_dir`; start from
[`examples/typescript/compare.toml`](../examples/typescript/compare.toml) and
see [configuration.md](configuration.md#comparetoml) for every key. For each
case, rustify runs the old program and then the new one, each in its own
empty temporary directory (after copying the case's `fixture` into it), and
compares:

1. the exit code (`signal N` when a signal ended the program),
2. stdout and stderr,
3. the paths left in the working directory and what each is (directory,
   symlink and its target, text file, binary file),
4. the contents of the files that are UTF-8 text.

Files that are not UTF-8 are compared by presence only. For images and video,
write a check of your own (dimensions, decoded pixels, duration).

Cases run one at a time, so programs that start a browser or a server do not
pile up. When a case ends, rustify kills whatever is still running in the
program's process group. `timeout_secs` covers the program and its output: a
case whose program is still running, or whose stdout or stderr is still held
open by something it started, is killed and reported as `timeout`. A timeout
always counts as a difference, including when both programs time out. Two
limits: a process that moves itself to another session (`setsid`) is not
killed, and on Windows only the program itself is.

```
DIFF hello: stdout (old exit 0, new exit 0)
  stdout:
    line 1
    old: hello
    new: Hello
ACCEPTED version: stdout (old exit 0, new exit 0)
  accepted: the Rust build reports the crate version
STALE fail: accepted in compare.toml but no longer differs
3 case(s): 0 same, 1 accepted, 1 differ, 1 accepted but no longer differ (remove `accept`)
```

`compare` exits 1 when any case differs and has no `accept`. For each
difference, either fix the Rust or add `accept = "<why>"` to the case and
record the divergence where users will find it.

**Writing the cases.** Build the list from the TypeScript, not from the Rust:

1. One case per command with no arguments, with `--help`, and with an unknown
   flag. Usage text and argument errors are where ports drift first.
2. One case per flag, including a bad value for each flag that validates.
3. One case per error path the TypeScript reports: missing file, malformed
   input, missing tool on `PATH`.
4. One case per kind of file written, with a `fixture` that exercises it.
5. Sequences that depend on earlier commands need one script per sequence;
   make the script the case (`old = ["sh", "{root}/cases/seq.sh", "old"]`).

Add `[[normalize]]` rules for timestamps, process ids, durations, and random
ids. Set `HOME` and cache directories to `{work}` under `[env]` so neither
program reads your own configuration.

Once the cases pass, run `rustify compare` in CI for as long as both programs
ship.

## 11. Errors and what to do

| Message | Meaning | Fix |
|---|---|---|
| `no rustify.toml in ... or its parents` | Not run inside a configured repo | `cd` into it, or pass `--config <path>` |
| `X imports Y, which is not ported` (`start`, `done`, `check`) | Ports must follow the import order | Port `Y` first (`rustify next`), or port both together. `start --force` claims anyway; `done` still refuses |
| `X needs types from unported Y; declare ... in y.rs first (types-first)` | `X` uses only types from `Y` | Declare those types in `Y`'s Rust file, matching the TS shape |
| `no Rust item found for a, b; pass --map name=path` (warning) | Exports renamed or folded in Rust | `done X --map a=app::m::new_name` or `--map a=-` |
| `x.rs is not declared with mod in its parent` | The file is not compiled | Add `pub mod x;` to the parent |
| `X: ... maps to app::m::f, which is not in m.rs` | A recorded symbol was renamed or removed | Re-run `done X` (with `--map` if needed) |
| `A, B all map to app::m` | Two TS files mirror to one module (`m.ts` and `m/index.ts`) | Give one a `--rust` path when porting |
| `external 'pkg' has no [[package]] mapping` (warning) | No rule for an npm import | Add a `[[package]]` rule to `mappings.toml` |
| `X is indexed but the TS file is gone` | TS deleted or renamed | Remove the entry, or `done <new path> --rust <file>` |
| `warning: upstream drift unknown` | The upstream ref is missing | `git fetch origin main` (or set `upstream`) |
| ratchet: `... changed without porting it` | See [section 7](#7-the-ci-ratchet) | Port the diff, `rustify done <file>`, commit `index.toml` |
| ratchet: `find the merge base ... (fetch full history)` | Shallow clone in CI | `fetch-depth: 0` |
| `read .../compare.toml` | `compare` has no cases | Copy `examples/typescript/compare.toml` into `state_dir` |
| compare: `start <program>: No such file or directory` | A command in `old`/`new` is not found from the temporary working directory | Use `{root}/...` or an absolute path |
| `index.toml` conflict after a rebase | Two branches recorded ports | Keep both sides' entries, re-run `done` on your modules |
