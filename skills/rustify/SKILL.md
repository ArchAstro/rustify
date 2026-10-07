---
name: rustify
description: Port a TypeScript or JavaScript codebase to Rust with the rustify harness. Use when asked to start or continue a TS → Rust port, to pick and port the next modules, to run a wave of parallel porting agents, to catch a port up after the TypeScript changed (or fix a failing "Rust port ratchet" CI job), to assess whether the end-to-end tests can serve as the port's acceptance suite, to compare the finished Rust program against the TypeScript one, or to review a port PR.
---

# Porting TypeScript to Rust with rustify

`rustify` plans and tracks the port; it writes no Rust. Install it with
`cargo install --locked --git https://github.com/ArchAstro/rustify`.
`docs/workflow.md` and `docs/configuration.md` in that repository are the
reference for every command and key named here.

Order of work:

1. Set up (once).
2. Assess the end-to-end tests (once, before any Rust).
3. Port batches, alone first, then in parallel waves.
4. Keep ported modules current while the TypeScript changes.
5. Compare the two programs when nothing is left to port.

## 1. Set up

1. Create the crate, `rustify.toml`, and `<state_dir>/mappings.toml` from
   `examples/typescript/`.
2. `rustify graph`: fix `roots`, `exclude`, and `test_markers` until the file
   and test counts match the repository.
3. `rustify check`: add a `[[package]]` rule naming the crate for every npm
   package it warns about. Choose crates now; a brief that says "NO MAPPING"
   leaves each agent to choose a different one.
4. Write a conventions document (errors, async runtime, naming, how tests
   are laid out, deliberate differences from the TypeScript) and set
   `conventions` to its path. Every brief then points at it.
5. Add the ratchet workflow from `examples/ci/` and make it a required check.

## 2. Assess the end-to-end tests

Run `rustify e2e` and read all three groups (workflow.md, section 9). It
classifies by imports and by file name (`.test.`/`.spec.`, with an extension
listed in `extensions`), so also search for tests it cannot see: suites with
other names, shell scripts, and tests that start the program through a
wrapper package.

1. **Process tests with no source imports** can run against the Rust binary
   unchanged. For these, check by reading them:
   - Coverage: list every command, flag, exit code, and file the program
     writes; mark which have an assertion. Report the uncovered ones.
   - Target: each test must take the command to run from an environment
     variable. Change tests that hard-code the Node entry point.
   - Other needs: network, accounts, a browser, tools on `PATH`. A test CI
     skips protects nothing; give it a local stand-in or list it as a gap.
2. **Process tests that also import source** need each listed import replaced
   with something observable from outside the process.
3. If group 1 is empty or thin, write the missing protocol tests against the
   TypeScript first, and get them passing there, before porting. Tests
   written after the port encode whatever the Rust happens to do.
4. Put these tests under a `binary_test_markers` path so `next` never assigns
   them to a batch, and add a CI job that runs them against the Rust binary.

Tell the user what the assessment found before starting step 3: which
behavior has an end-to-end test, which has none, and what you added.

## 3. Port a batch

1. `rustify next --brief` (or `rustify brief <ts files>` for files the user
   chose). Port exactly what it lists.
2. `rustify start <ts files>`.
3. Follow the brief for each file:
   - Write the Rust at the listed path and add the `mod` declaration.
   - Use the Rust paths it gives for ported imports; look up anything else
     with `rustify find <name>`.
   - Declare "types from modules not ported yet" in those modules' Rust
     files, types only.
   - Handle every listed hazard on purpose. They are where JS and Rust
     behave differently for the same-looking code.
4. Port each listed TS test case one for one: same scenario, same assertions.
5. Run the focused `cargo test`, then
   `cargo clippy --all-targets -- -D warnings` and `cargo fmt --all`.
6. `rustify done <ts files> --test <ts test>...`, add `--map` for each export
   it could not find, then `rustify check`.
7. For a construct or npm package the brief had no rule for, add a
   `[[construct]]` or `[[package]]` entry to `mappings.toml` before the next
   batch.

Keep user-visible strings, exit codes, and on-disk formats byte-identical
unless the conventions document records a deliberate difference. Files that
are build glue or are replaced by a crate get `rustify skip` or
`rustify replace`, with the reason.

## 4. Parallel waves

Port the first ten or so batches yourself, one at a time, so the conventions
and `mappings.toml` settle. Then one lead session runs waves of 3–4 batches:

1. **Plan:** `rustify next --independent --count 4 --write-briefs <dir>`.
   Take batches only from this output; a hand-assembled batch that imports
   an unported module is refused by `done` at integration.
2. **Claim:** the lead runs `rustify start` for every file in the wave.
3. **Fan out:** one subagent per batch, each in its own git worktree created
   at the lead's tip. Its prompt is the `batch-<n>.md` brief, the path to the
   conventions document, and these rules:
   - Write only the files in the brief's table, its types-first files, and
     the tests.
   - Never run `rustify start|done|skip|replace` and never edit the state
     directory.
   - Run only the focused tests for the modules you wrote, plus clippy and
     fmt. Do not commit; stage everything and report the worktree path, the
     TS → Rust renames, the tests ported, and anything the brief did not
     cover.
   - Test fakes never return a process id; a test that needs a live pid
     spawns a real child. Never `pkill`/`killall` or kill by pattern: other
     agents are running cargo. Kill only pids you started.
   - Tests that start a browser, a server, or another heavy process run one
     at a time, never in a loop or in parallel, and must not leave the
     process running when the test fails.
4. **Review:** a fresh subagent that did not write the batch checks it
   against the brief: hazards, one-for-one tests, unchanged user-visible
   strings. Treat the porting agent's summary as a claim to verify.
5. **Integrate one batch at a time** on the lead's branch: apply the staged
   diff, merge `mod` lines and Cargo dependencies, run
   `rustify done <files> --test ... [--map ...]` and `rustify check`, commit.
   A `done` error means the batch is not integrated; fix it before the next.
6. **End of wave:** run the whole workspace's clippy and tests once. Breaks
   between batches only show up here.

More than about five agents compiling at once slows every one of them. Give
each worktree the same compiler cache (`RUSTC_WRAPPER=sccache`) and delete a
worktree's `target/` when its batch is integrated.

## 5. Keep ported modules current

A ported module is stale when its TS file or a recorded test changed after
`done`. The ratchet job fails a PR that makes one stale.

1. `git fetch`, then `rustify ratchet` (what CI runs) or `rustify drift`
   (everything stale on the upstream ref).
2. `rustify brief <ts file>` shows the TS diff since the module was ported.
   Port that diff into the existing Rust and its tests.
3. `rustify done <ts file>` re-records it. Commit `index.toml` with the Rust.
4. For a backlog, `rustify next --catch-up` orders the stale modules by their
   imports.

There is no opt-out flag: a PR that changes ported TypeScript carries the
Rust change.

## 6. Compare the two programs when the port is done

When `rustify status` shows nothing left, the module tests pass, and the
end-to-end suite from step 2 passes against the Rust binary, run both
programs side by side (workflow.md, section 10):

1. **Inventory.** Have a subagent that did not write the port read the
   TypeScript entry points and list every command, flag, default, error
   message, exit code, and file written. Build the list from the TypeScript
   only.
2. **Cases.** Turn the inventory into `[[case]]` entries in
   `<state_dir>/compare.toml` (start from `examples/typescript/compare.toml`):
   every command bare, with `--help`, with an unknown flag, with each flag,
   with a bad value for each validated flag, each error path, and each kind
   of output file with a `fixture`. Add `[[normalize]]` rules for timestamps,
   ids, and durations, and set `HOME` and cache directories to `{work}`.
3. **Run** `rustify compare` against release builds of both programs. It
   runs one case at a time and, when a case ends or passes its timeout,
   kills what is still running in the program's process group. Do not wrap
   it in a parallel loop. A `timeout` is a failure even when both programs
   time out.
4. **Triage every `DIFF`.** The TypeScript is right unless the user says
   otherwise:
   - Fix the Rust and add a Rust test for the case, or
   - add `accept = "<why>"` and record the difference in the conventions
     document, and name it in the PR description. Accept nothing silently.
5. **Check what `compare` cannot.** Files that are not UTF-8 are compared by
   presence only, and stdout or stderr that is not UTF-8 is compared after
   lossy decoding. For images, video, and archives, decode both outputs and
   compare what users see (dimensions, pixels, duration, colour). For
   interactive screens, drive both through a pty with the same keystrokes
   and compare the rendered text.
6. **Measure** startup time and the main commands on both, and report the
   numbers, including any the Rust loses.
7. Add `rustify compare` to CI while both programs ship. Remove an `accept`
   when `compare` reports it `STALE`.

Report the result as counts (same, accepted, differ) with the list of
accepted differences. The port is not finished while a case differs without
an `accept`.

## Terminal UI

For `.tsx` files and anything importing a UI package, follow the brief's UI
section (workflow.md, section 8): reuse the widgets it lists, build a
`planned` widget as its own batch before the screen that needs it, map each
UI API through an `[[ink]]` rule, and prove rendering parity against golden
frames rendered by the real TS component. Never edit a golden by hand.

## Rules

- Do not edit `index.toml` by hand except for `notes`.
- CI must run the same checks you ran: pin the Rust toolchain version in the
  workflow, and run the tests on every OS the program supports before
  calling the port done.
- State what was verified and what was not. An untested platform or an
  uncovered command is a finding to report, not a detail to leave out.
