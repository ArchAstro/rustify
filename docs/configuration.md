# Configuration reference

rustify reads five files. You write the first four; `index.toml` is written
by the tool.

| File | Location | Required |
|---|---|---|
| `rustify.toml` | anywhere in the repository (found by searching upward, or `--config`) | yes |
| `mappings.toml` | `state_dir` | yes (may be empty) |
| `components.toml` | `state_dir` | no |
| `compare.toml` | `state_dir` | only for `rustify compare` |
| `index.toml` | `state_dir` | created by `start` / `done` |

All paths inside these files are relative to the repository root (the git top
level), except `state_dir` and the `rust` paths noted below. Unknown keys are
rejected, so a typo fails loudly.

## `rustify.toml`

```toml
state_dir = "port"
upstream = "origin/main"
conventions = "docs/porting.md"

roots = ["packages/app/src"]
packages_dir = "packages"
primary_package = "@acme/app"
rust_crate = "rust/app"
rust_crate_name = "app"

exclude = ["packages/app/src/index-bundled.ts"]
extensions = ["ts", "tsx"]
test_markers = ["/__tests__/", ".test.ts", ".test.tsx", ".spec.ts"]
binary_test_markers = ["/__tests__/e2e/"]
external_packages = ["@acme/generated-sdk"]

[batch]
max_files = 6
max_lines = 800
```

| Key | Default | Meaning |
|---|---|---|
| `state_dir` | the directory holding `rustify.toml` | Where `mappings.toml`, `components.toml`, and `index.toml` live. **Relative to `rustify.toml`**, not the repository root. |
| `upstream` | `"origin/main"` | The ref the port tracks. `done` stamps `ts_base` with the merge base of `HEAD` and this ref; `drift`, `next --catch-up`, and `ratchet` compare against it. Fetch it before using those commands. |
| `conventions` | none | A porting conventions document. When set, every brief ends with a pointer to it. |
| `roots` | required | Source directories. Every non-test `.ts`/`.tsx` file under them is in the graph, along with any workspace-package file they import. |
| `packages_dir` | required | Directory whose immediate subdirectories each hold a `package.json`. Used to resolve workspace imports and to group files by package. |
| `primary_package` | required | `name` of the package whose files land at the crate root. Files of every other package land in a top-level module named after the package (`@acme/kit` → `kit`). |
| `rust_crate` | required | Directory of the crate the port writes into (it holds `src/`). |
| `rust_crate_name` | required | That crate's Rust name; module paths start with it. |
| `exclude` | `[]` | Globs of files never added to the graph. |
| `extensions` | `["ts", "tsx"]` | Source file extensions in the graph. Add `js`, `mjs`, `cjs`, or `jsx` to port JavaScript too: it is parsed with the TypeScript grammar (`jsx` with TSX), and an import of `./x.mjs` resolves to `x.ts` first, then `x.mjs`. `.d.ts` files and anything under `dist/` are never sources. |
| `test_markers` | required | A path containing any of these substrings is a test. |
| `binary_test_markers` | `[]` | Tests matching these run against the built binary. They are never assigned to a batch. |
| `external_packages` | `[]` | Workspace packages replaced wholesale by a Rust crate instead of ported. Imports of them are treated as external; give each a `[[package]]` rule. |
| `batch.max_files` | required | Most files `next` puts in one batch. A single import cycle larger than this is still one batch. |
| `batch.max_lines` | required | Most TS lines `next` puts in one batch, with the same exception. |

## `mappings.toml`

Two kinds of rule. Start from
[`examples/typescript/mappings.toml`](../examples/typescript/mappings.toml)
and edit it: the rules are guidance for the people and agents porting, so
make them match the crates and patterns your port uses.

### `[[construct]]`: TypeScript constructs

Each rule is a tree-sitter query over the TypeScript syntax tree. The brief
lists every match by line, with the rule's Rust guidance and hazards.

```toml
[[construct]]
id = "promise-combinator"
title = "Promise.all / allSettled / race / any"
query = '''
(call_expression
  function: (member_expression
    object: (identifier) @obj
    property: (property_identifier) @prop)
  (#eq? @obj "Promise")
  (#match? @prop "^(all|allSettled|race|any)$")) @match
'''
rust = "all → `futures::future::try_join_all`; race → `tokio::select!`; ..."
hazards = ["Promise.all does not cancel the others on failure; dropping a Rust join does."]
```

| Key | Meaning |
|---|---|
| `id` | Unique identifier, shown by `graph`. |
| `title` | Heading in the brief. |
| `query` | A [tree-sitter-typescript](https://github.com/tree-sitter/tree-sitter-typescript) query. Capture the node to report as `@match`. Every query is compiled at startup; a bad one stops the tool with an error. |
| `rust` | The Rust abstraction to use. |
| `hazards` | Behavior differences to watch for. Each is printed under the match. |
| `tsx_only` | `true` if the query uses JSX nodes (only the TSX grammar has them). |
| `contains` | Report the match only if a node of this kind appears inside it (not counting nested functions). Use it for "a loop that awaits": queries cannot express "at any depth". |

Query gotcha: tree-sitter applies every predicate in a pattern to every
branch of a `[...]` alternation, so alternatives that need different
predicates must be separate top-level patterns. `rustify graph` prints the hit
count per rule; check any rule with 0 hits against your source.

### `[[package]]`: npm packages and Node builtins

```toml
[[package]]
npm = "commander"
rust = "clap derive (`#[derive(Parser, Subcommand)]`)"
crates = ["clap"]
notes = "Option spellings and aliases must match; help layout need not."
```

| Key | Meaning |
|---|---|
| `npm` | Package name (`commander`, `@scope/name`) or builtin (`node:fs`). A bare builtin import (`fs`) also matches `node:fs`. |
| `rust` | What replaces it. |
| `crates` | Crates to add, printed next to the rule. |
| `notes` | Anything else the porter must know. |

## `components.toml` (optional)

For porting UI components onto a Rust UI library crate. See
[workflow.md, section 8](workflow.md#8-ui-components).

```toml
[library]
crate = "rust/app-ui"                  # library crate directory
crate_name = "app_ui"
goldens = "rust/app-ui/goldens"        # golden frame files
golden_test = "packages/tui/src/__tests__/goldens.test.tsx"
golden_command = "npx vitest run {test} -u"
ui_packages = ["ink", "react"]

[[module]]
ts = "packages/tui/src/layout.ts"
rust = "layout.rs"

[[widget]]
name = "Row"
rust = "app_ui::layout::Row"
file = "layout.rs"
from = ["packages/tui/src/layout.ts"]
golden = "row.txt"
status = "available"
summary = "one-line row of fixed and fill cells"

[[ink]]
names = ["Box"]
rust = "`app_ui::layout::Row`"
hazards = ["Yoga shrinks fixed-width children on overflow; ratatui does not."]
```

| Key | Meaning |
|---|---|
| `library.crate`, `library.crate_name` | The UI library crate. |
| `library.goldens` | Directory of golden frame files. |
| `library.golden_test` | The TS test that renders components into `goldens`. |
| `library.golden_command` | Command that regenerates goldens, run in the golden test's package directory. `{test}` becomes the test path relative to it. Default `npx vitest run {test} -u`. |
| `library.ui_packages` | Import specifiers whose names `[[ink]]` rules map. Default `["ink", "react"]`. |
| `[[module]]` `ts`, `rust` | Port this TS file into the library crate, at `rust` (relative to the library's `src/`). |
| `[[widget]]` `name`, `rust`, `file` | A widget: its type name, full Rust path, and file relative to the library's `src/`. |
| `[[widget]]` `from` | TS files it replaces in whole or in part. |
| `[[widget]]` `golden` | Golden file its parity test reads. |
| `[[widget]]` `status` | `available` (exists and tested) or `planned` (to build first). |
| `[[widget]]` `summary` | One line for the brief. |
| `[[ink]]` `names`, `rust`, `hazards` | Imported UI API names and their Rust equivalent. |

## `compare.toml` (optional)

Read by `rustify compare` from `state_dir`. Starter:
[`examples/typescript/compare.toml`](../examples/typescript/compare.toml).

| Key | Default | Meaning |
|---|---|---|
| `old` | required | Command line of the TypeScript program, as an array. `{root}` expands to the repository root. Each case runs in a temporary directory, so every path must be absolute or start with `{root}`. |
| `new` | required | Command line of the Rust program. |
| `timeout_secs` | `60` | Seconds allowed for the program to exit and its output to close. Past that the program's process group is killed and the case is reported as `timeout`, which always counts as a difference. |
| `[env]` | none | Variables set for both programs. `{root}` and `{work}` (the case's working directory) expand in values. The rest of the environment is inherited. |
| `[[normalize]]` | none | `pattern` (a [regex](https://docs.rs/regex)) and `replace`, applied in order to stdout, stderr, text file contents, and file names before comparing. The working directory is replaced with `<WORK>` first. |

Each `[[case]]`:

| Key | Default | Meaning |
|---|---|---|
| `name` | required | Unique. Letters, digits, `-`, `_`, and `.` only. Used by `--case` and as the directory name under `--keep`. |
| `args` | `[]` | Arguments appended to both command lines. `{root}` and `{work}` expand. |
| `fixture` | none | Directory copied into the working directory before each run, relative to `compare.toml`. Symlinks are copied as symlinks. |
| `stdin` | closed | Text written to the program's stdin. |
| `accept` | none | Why the case may differ. An accepted case does not fail the run; `compare` reports it as `STALE` once the programs agree. |

## `index.toml` (written by rustify)

One `[[module]]` per TS file the port has touched, sorted by path. Edit
`notes` by hand freely; let the commands manage the rest.

| Key | Meaning |
|---|---|
| `ts` | TS file, repository-relative. |
| `status` | `in_progress` (claimed by `start`), `ported`, `verified` (`done --verified`: also covered by end-to-end tests), `replaced`, or `skipped`. Everything except `in_progress` counts as done for planning. |
| `rust` | Rust file, relative to the owning crate's `src/`. |
| `module` | Rust module path. |
| `ported_at` | When the entry was last recorded (UTC). |
| `ts_base` | Merge base of `HEAD` and `upstream` at that time. `drift` diffs from here. |
| `ts_blobs` | Git blob id of the TS file and each tracked test when recorded. Staleness compares these. |
| `symbols` | TS export name → Rust item path (`-` for none). |
| `tests` | TS tests whose cases were ported with the module. |
| `test_provenance` | Ported tests that are no longer in the graph but are still watched for changes. |
| `notes` | Free text: why a file was skipped, a deliberate divergence, anything the next porter needs. |
