//! Canonical end-to-end proof for `rustify`: the real binary plans,
//! briefs, records, and checks a port of a small fixture workspace shaped
//! like a TypeScript monorepo CLI's (a primary package, a workspace library imported through
//! package.json `exports`, an npm alias that shadows a workspace package
//! name, an import cycle, a type-only back-edge, module tests with helpers,
//! and an e2e test that must never be assigned to a batch).

use std::path::{Path, PathBuf};
use std::process::Output;

use serde_json::Value;

struct Fixture {
    _dir: tempfile::TempDir,
    root: PathBuf,
}

impl Fixture {
    fn write(&self, rel: &str, contents: &str) {
        let path = self.root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }

    fn port(&self, args: &[&str]) -> Output {
        std::process::Command::new(env!("CARGO_BIN_EXE_rustify"))
            .arg("--repo")
            .arg(&self.root)
            .args(args)
            .current_dir(&self.root)
            .output()
            .expect("rustify runs")
    }

    fn port_json(&self, args: &[&str]) -> Value {
        let mut full = vec!["--json"];
        full.extend_from_slice(args);
        let out = self.port(&full);
        assert!(out.status.success(), "{args:?} failed: {}", text(&out));
        serde_json::from_slice(&out.stdout).expect("stdout is JSON")
    }

    /// Runs git in the fixture, returning trimmed stdout.
    fn git(&self, args: &[&str]) -> String {
        let out = std::process::Command::new("git")
            .args([
                "-c",
                "user.name=fixture",
                "-c",
                "user.email=fixture@example.test",
            ])
            .args(["-c", "commit.gpgsign=false"])
            .args(args)
            .current_dir(&self.root)
            .output()
            .expect("git runs");
        assert!(out.status.success(), "git {args:?}: {}", text(&out));
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// Commits the whole tree and points `origin/main` at it, as if it had
    /// landed upstream and been fetched.
    fn land_upstream(&self, message: &str) -> String {
        self.git(&["add", "-A"]);
        self.git(&["commit", "-qm", message]);
        self.git(&["update-ref", "refs/remotes/origin/main", "HEAD"]);
        self.git(&["rev-parse", "HEAD"])
    }

    fn rust(&self, rel: &str, contents: &str) {
        self.write(&format!("crates/app/src/{rel}"), contents);
    }
}

fn text(out: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

fn files_of(batch: &Value) -> Vec<String> {
    batch["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["ts"].as_str().unwrap().to_string())
        .collect()
}

fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let f = Fixture { _dir: dir, root };

    // Port state: config plus a slice of the real mapping rules.
    f.write(
        "rustify.toml",
        r#"
state_dir = "port"
roots = ["src/ts/app/src"]
packages_dir = "src/ts"
primary_package = "@x/app"
rust_crate = "crates/app"
rust_crate_name = "app"
test_markers = ["/__tests__/", ".test.ts"]
binary_test_markers = ["/__tests__/e2e/"]
external_packages = ["@x/generated-sdk"]
[batch]
max_files = 2
max_lines = 1000
"#,
    );
    f.write(
        "port/mappings.toml",
        r#"
[[construct]]
id = "async-fn"
title = "async function"
query = '(function_declaration "async") @match'
rust = "async fn on tokio"
hazards = ["futures are lazy"]

[[package]]
npm = "@real/sdk"
rust = "the sdk crate"

[[package]]
npm = "@x/generated-sdk"
rust = "the sdk crate"
"#,
    );
    f.rust("lib.rs", "//! fixture crate\n");

    // Primary package: declares the workspace library, an npm alias whose
    // target name collides with a workspace package, and a generated SDK
    // that is replaced wholesale.
    f.write(
        "src/ts/app/package.json",
        r#"{"name":"@x/app","dependencies":{
            "@x/kit":"workspace:*",
            "@x/generated-sdk":"workspace:*",
            "@x/sdk":"npm:@real/sdk@1.2.3"}}"#,
    );
    f.write(
        "src/ts/app/src/util.ts",
        "export function slugify(s: string): string { return s.toLowerCase(); }\n",
    );
    // types.ts only needs `Store` as a type; store.ts needs types' values.
    f.write(
        "src/ts/app/src/types.ts",
        "import type { Store } from \"./store\";\nexport interface Options { store?: Store }\nexport const DEFAULT_NAME = \"main\";\n",
    );
    f.write(
        "src/ts/app/src/store.ts",
        r#"import { DEFAULT_NAME, Options } from "./types";
import { slugify } from "./util";
import { helper } from "@x/kit/helpers";
import { Client } from "@x/sdk";
import { Generated } from "@x/generated-sdk";
export class Store { name = slugify(DEFAULT_NAME); }
export async function openStore(options: Options): Promise<Store> { helper(); return new Store(); }
"#,
    );
    // A value cycle: must be one unit.
    f.write(
        "src/ts/app/src/cycle-a.ts",
        "import { b } from \"./cycle-b\";\nexport const a = () => b();\n",
    );
    f.write(
        "src/ts/app/src/cycle-b.ts",
        "import { a } from \"./cycle-a\";\nexport const b = () => a;\n",
    );
    f.write(
        "src/ts/app/src/__tests__/util.test.ts",
        "import { slugify } from \"../util\";\n",
    );
    f.write(
        "src/ts/app/src/__tests__/store.test.ts",
        "import { openStore } from \"../store\";\nimport { fixtureOptions } from \"./helpers/fixture\";\n",
    );
    f.write(
        "src/ts/app/src/__tests__/helpers/fixture.ts",
        "import { DEFAULT_NAME } from \"../../types\";\nexport const fixtureOptions = {};\n",
    );
    f.write(
        "src/ts/app/src/__tests__/e2e/cli.e2e.test.ts",
        "import { slugify } from \"../../util\";\n",
    );

    // Workspace library reached through package.json `exports` → dist.
    f.write(
        "src/ts/kit/package.json",
        r#"{"name":"@x/kit","exports":{"./helpers":{"types":"./dist/helpers.d.ts","default":"./dist/helpers.js"}}}"#,
    );
    f.write("src/ts/kit/src/helpers.ts", "export function helper() {}\n");
    f.write("src/ts/kit/src/unused.ts", "export const unused = 1;\n");
    // Same name as the npm alias target: must not be pulled in.
    f.write("src/ts/real-sdk/package.json", r#"{"name":"@real/sdk"}"#);
    f.write("src/ts/real-sdk/src/index.ts", "export class Client {}\n");
    f.write(
        "src/ts/generated-sdk/package.json",
        r#"{"name":"@x/generated-sdk"}"#,
    );
    f.write(
        "src/ts/generated-sdk/src/index.ts",
        "export class Generated {}\n",
    );
    // `done` stamps entries with `git merge-base HEAD origin/main`.
    f.git(&["init", "-q", "-b", "main"]);
    f.land_upstream("fixture");
    f
}

fn assert_ok(out: &Output) {
    assert!(out.status.success(), "{}", text(out));
}

#[test]
fn dependency_ports_allow_existing_consumers_to_refresh_provenance() {
    let f = fixture();
    let consumer = "src/ts/app/src/consumer.ts";
    let extraction = "src/ts/app/src/publication/extraction.ts";
    let desk = "src/ts/app/src/local-daemon/pr-remediation-desk.ts";

    // The consumer already has an honest port and a recorded source blob.
    f.write(consumer, "export function consume() {}\n");
    f.rust(
        "lib.rs",
        "pub mod consumer;\npub mod publication;\npub mod local_daemon;\n",
    );
    f.rust("consumer.rs", "pub fn consume() {}\n");
    f.rust("publication/mod.rs", "pub mod extraction;\n");
    f.rust("local_daemon/mod.rs", "pub mod pr_remediation_desk;\n");
    f.land_upstream("existing consumer");
    assert_ok(&f.port(&["start", consumer]));
    assert_ok(&f.port(&["done", consumer]));

    // Its retained feature imports two newly introduced runtime dependencies.
    f.write(extraction, "export function extractPublicationProse() {}\n");
    f.write(desk, "export class PullRequestRemediationNeedsYou {}\n");
    f.write(consumer, "import { extractPublicationProse } from './publication/extraction';\nimport { PullRequestRemediationNeedsYou } from './local-daemon/pr-remediation-desk';\nexport function consume() { extractPublicationProse(); new PullRequestRemediationNeedsYou(); }\n");
    f.land_upstream("consumer acquires publication and desk imports");
    let refused = f.port(&["done", consumer]);
    assert!(!refused.status.success());
    assert!(text(&refused).contains("not ported"), "{}", text(&refused));
    let consumer_bytes = std::fs::read(f.root.join(consumer)).unwrap();

    // Supported CLI recording crosses a real process and Git-blob boundary.
    f.rust(
        "publication/extraction.rs",
        "pub fn extract_publication_prose() {}\n",
    );
    f.rust(
        "local_daemon/pr_remediation_desk.rs",
        "pub struct PullRequestRemediationNeedsYou;\n",
    );
    assert_ok(&f.port(&["start", extraction, desk]));
    assert_ok(&f.port(&["done", extraction, desk]));
    assert_ok(&f.port(&["done", consumer]));

    // Refresh changes provenance, not the preserved consumer feature source.
    assert_eq!(
        std::fs::read(f.root.join(consumer)).unwrap(),
        consumer_bytes
    );
    let index = std::fs::read_to_string(f.root.join("port/index.toml")).unwrap();
    assert!(index.contains(&f.git(&["hash-object", consumer])));
    assert!(index.contains(&f.git(&["hash-object", extraction])));
    assert!(index.contains(&f.git(&["hash-object", desk])));
    assert_ok(&f.port(&["ratchet", "--base", "origin/main"]));
}

#[test]
fn done_reconciles_a_recorded_test_that_leaves_the_dependency_graph() {
    let f = fixture();
    let test = "src/ts/kit/src/__tests__/helpers.test.ts";
    let retained_test = "src/ts/kit/src/__tests__/helpers-retained.test.ts";
    let source = "src/ts/kit/src/helpers.ts";

    // A workspace-library test initially reaches a source imported by the
    // primary package, so `done --test` records it as ported.
    f.write(test, "import { helper } from \"../helpers\";\nhelper();\n");
    f.write(
        retained_test,
        "import { helper } from \"../helpers\";\nhelper();\n",
    );
    f.land_upstream("add kit helper test");
    f.rust("lib.rs", "pub mod kit;\n");
    f.rust("kit/mod.rs", "pub mod helpers;\n");
    f.rust("kit/helpers.rs", "pub fn helper() {}\n");
    assert_ok(&f.port(&["start", source]));
    assert_ok(&f.port(&["done", source, "--test", test, "--test", retained_test]));
    assert_ok(&f.port(&["check"]));

    // The test now imports a test-only module the primary package cannot
    // reach. The graph drops this library test; a fixup `done` must reconcile
    // its old record while keeping the Rust port and source stamp.
    f.write(
        "src/ts/kit/src/test-log.ts",
        "export const log = () => {};\n",
    );
    f.write(test, "import { helper } from \"../helpers\";\nimport { log } from \"../test-log\";\nhelper(); log();\n");
    f.land_upstream("kit helper test uses test-only log");
    let before = f.port(&["check"]);
    assert!(!before.status.success());
    assert!(
        text(&before).contains("test src/ts/kit/src/__tests__/helpers.test.ts is not in the graph"),
        "{}",
        text(&before)
    );

    assert_ok(&f.port(&["done", source]));
    let index = std::fs::read_to_string(f.root.join("port/index.toml")).unwrap();
    assert!(
        index.contains(&format!("test_provenance = [\"{test}\"]")),
        "{index}"
    );
    assert!(index.contains(retained_test), "{index}");
    assert!(
        index.contains("helper = \"app::kit::helpers::helper\""),
        "{index}"
    );
    assert_ok(&f.port(&["check"]));

    // A later edit to the off-graph test still makes its Rust module stale.
    f.write(
        test,
        "import { helper } from \"../helpers\";\nimport { log } from \"../test-log\";\nhelper(); log(); log();\n",
    );
    f.land_upstream("change off-graph helper scenarios");
    let drift = f.port_json(&["drift"]);
    let helper = drift["drifted"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["ts"] == source)
        .expect("ported helper must be stale");
    assert!(
        helper["changed"]
            .as_array()
            .unwrap()
            .contains(&Value::from(test)),
        "{drift}"
    );
    let next = f.port_json(&["next", "--catch-up"]);
    assert!(
        next.as_array()
            .unwrap()
            .iter()
            .any(|batch| files_of(batch).contains(&source.to_string())),
        "{next}"
    );
    let brief = text(&f.port(&["next", "--catch-up", "--brief"]));
    assert!(brief.contains(test), "{brief}");
    assert!(brief.contains("helper(); log(); log();"), "{brief}");
}

#[test]
fn done_records_a_ported_test_that_is_already_outside_the_graph() {
    let f = fixture();
    let source = "src/ts/kit/src/helpers.ts";
    let test = "src/ts/kit/src/__tests__/helpers.test.ts";
    f.write(
        "src/ts/kit/src/test-log.ts",
        "export const log = () => {};\n",
    );
    f.write(
        test,
        "import { helper } from \"../helpers\";\nimport { log } from \"../test-log\";\nhelper(); log();\n",
    );
    f.land_upstream("add off-graph helper test");
    f.rust("lib.rs", "pub mod kit;\n");
    f.rust("kit/mod.rs", "pub mod helpers;\n");
    f.rust("kit/helpers.rs", "pub fn helper() {}\n");
    assert_ok(&f.port(&["start", source]));
    assert_ok(&f.port(&["done", source, "--test-provenance", test]));
    assert_ok(&f.port(&["check"]));
    let index = std::fs::read_to_string(f.root.join("port/index.toml")).unwrap();
    assert!(
        index.contains(&format!("test_provenance = [\"{test}\"]")),
        "{index}"
    );
}

#[test]
fn plans_briefs_records_and_checks_a_dependency_ordered_port() {
    let f = fixture();

    // ── The graph: what is in scope and how it is cut into units. ────────
    let graph = f.port_json(&["graph"]);
    let files: Vec<&str> = graph["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| x["ts"].as_str().unwrap())
        .collect();
    // The workspace library is followed through exports; unreached files,
    // the npm-alias namesake, and the replaced SDK are not.
    assert!(files.contains(&"src/ts/kit/src/helpers.ts"));
    assert!(!files.contains(&"src/ts/kit/src/unused.ts"));
    assert!(!files.iter().any(|x| x.starts_with("src/ts/real-sdk/")));
    assert!(!files.iter().any(|x| x.starts_with("src/ts/generated-sdk/")));
    let store = graph["files"]
        .as_array()
        .unwrap()
        .iter()
        .find(|x| x["ts"] == "src/ts/app/src/store.ts")
        .unwrap();
    assert_eq!(
        store["externals"],
        serde_json::json!(["@real/sdk", "@x/generated-sdk"])
    );
    let types = graph["files"]
        .as_array()
        .unwrap()
        .iter()
        .find(|x| x["ts"] == "src/ts/app/src/types.ts")
        .unwrap();
    assert_eq!(
        types["type_deps"],
        serde_json::json!(["src/ts/app/src/store.ts"])
    );
    // The value cycle shares a unit; the type-only back-edge does not fuse
    // types.ts and store.ts.
    let unit_of = |ts: &str| {
        graph["files"]
            .as_array()
            .unwrap()
            .iter()
            .find(|x| x["ts"] == ts)
            .unwrap()["unit"]
            .clone()
    };
    assert_eq!(
        unit_of("src/ts/app/src/cycle-a.ts"),
        unit_of("src/ts/app/src/cycle-b.ts")
    );
    assert_ne!(
        unit_of("src/ts/app/src/types.ts"),
        unit_of("src/ts/app/src/store.ts")
    );
    // The e2e test is a binary test; the module tests fold in their helper.
    let tests = graph["tests"].as_array().unwrap();
    let e2e = tests
        .iter()
        .find(|t| t["ts"] == "src/ts/app/src/__tests__/e2e/cli.e2e.test.ts")
        .unwrap();
    assert_eq!(e2e["binary"], true);
    let store_test = tests
        .iter()
        .find(|t| t["ts"] == "src/ts/app/src/__tests__/store.test.ts")
        .unwrap();
    assert_eq!(
        store_test["helpers"],
        serde_json::json!(["src/ts/app/src/__tests__/helpers/fixture.ts"])
    );

    // ── What to port first: leaves that bring a test come first. ────────
    let batches = f.port_json(&["next", "--count", "5"]);
    let first = &batches[0];
    assert_eq!(files_of(first), ["src/ts/app/src/util.ts"]);
    assert_eq!(
        first["tests"][0]["ts"],
        "src/ts/app/src/__tests__/util.test.ts"
    );
    let all_tests: Vec<&Value> = batches
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|b| b["tests"].as_array().unwrap())
        .collect();
    assert!(
        all_tests
            .iter()
            .all(|t| !t["ts"].as_str().unwrap().contains("/e2e/"))
    );

    // ── Order is enforced: store.ts cannot be recorded before its imports. ─
    let early = f.port(&["done", "src/ts/app/src/store.ts"]);
    assert!(!early.status.success());
    assert!(
        text(&early).contains("which is not ported"),
        "{}",
        text(&early)
    );

    // ── Port util.ts and record it with its test. ────────────────────────
    assert_ok(&f.port(&["start", "src/ts/app/src/util.ts"]));
    let claimed = f.port_json(&["next", "--count", "5"]);
    assert!(
        claimed
            .as_array()
            .unwrap()
            .iter()
            .all(|b| !files_of(b).contains(&"src/ts/app/src/util.ts".to_string())),
        "an in-progress module is not offered again"
    );
    f.rust("lib.rs", "pub mod util;\n");
    f.rust(
        "util.rs",
        "pub fn slugify(s: &str) -> String { s.to_lowercase() }\n",
    );
    assert_ok(&f.port(&[
        "done",
        "src/ts/app/src/util.ts",
        "--test",
        "src/ts/app/src/__tests__/util.test.ts",
    ]));
    let index = std::fs::read_to_string(f.root.join("port/index.toml")).unwrap();
    assert!(
        index.contains("slugify = \"app::util::slugify\""),
        "{index}"
    );
    assert!(
        index.contains("src/ts/app/src/__tests__/util.test.ts"),
        "{index}"
    );
    // Each entry records when it was ported and from which upstream commit.
    let first_base = f.git(&["rev-parse", "origin/main"]);
    assert!(
        index.contains(&format!("ts_base = \"{first_base}\"")),
        "{index}"
    );
    assert!(index.contains("ported_at = \"20"), "{index}");
    assert_ok(&f.port(&["check"]));

    // ── Fixup pass: `drift` lists modules whose TS changed upstream. ─────
    let clean = f.port_json(&["drift"]);
    assert_eq!(clean["drifted"], serde_json::json!([]));
    // Upstream edits the ported TS file after the port was recorded.
    f.write(
        "src/ts/app/src/util.ts",
        "export function slugify(s: string): string { return s.trim().toLowerCase(); }\n",
    );
    f.land_upstream("util: trim before slugifying");
    let drift = f.port_json(&["drift"]);
    let drifted = drift["drifted"].as_array().unwrap();
    assert_eq!(drifted.len(), 1, "{drift}");
    assert_eq!(drifted[0]["ts"], "src/ts/app/src/util.ts");
    assert_eq!(drifted[0]["ts_base"], first_base.as_str());
    assert_eq!(
        drifted[0]["changed"],
        serde_json::json!(["src/ts/app/src/util.ts"])
    );
    // After porting the change, re-running `done` re-stamps the entry and
    // keeps its symbol map; the module no longer drifts.
    assert_ok(&f.port(&["done", "src/ts/app/src/util.ts"]));
    let restamped = f.port_json(&["drift"]);
    assert_eq!(restamped["drifted"], serde_json::json!([]), "{restamped}");
    let index = std::fs::read_to_string(f.root.join("port/index.toml")).unwrap();
    assert!(
        index.contains("src/ts/app/src/__tests__/util.test.ts"),
        "{index}"
    );

    // ── Types-first: types.ts needs `Store` from the unported store.ts. ──
    let brief = f.port(&["brief", "src/ts/app/src/types.ts"]);
    assert_ok(&brief);
    let brief = text(&brief);
    assert!(
        brief.contains("Types from modules not ported yet"),
        "{brief}"
    );
    assert!(
        brief.contains("`Store` from `src/ts/app/src/store.ts` → declare in `store.rs`"),
        "{brief}"
    );

    f.rust("lib.rs", "pub mod types;\npub mod util;\n");
    f.rust(
        "types.rs",
        "pub struct Options;\npub const DEFAULT_NAME: &str = \"main\";\n",
    );
    // Claiming store.ts does not stand in for declaring the type: a module
    // still in progress has no Rust yet.
    assert_ok(&f.port(&["start", "src/ts/app/src/store.ts", "--force"]));
    let missing = f.port(&["done", "src/ts/app/src/types.ts"]);
    assert!(!missing.status.success());
    assert!(
        text(&missing).contains("declare `Store` in store.rs first"),
        "{}",
        text(&missing)
    );

    f.rust("lib.rs", "pub mod store;\npub mod types;\npub mod util;\n");
    f.rust("store.rs", "pub struct Store;\n");
    assert_ok(&f.port(&["done", "src/ts/app/src/types.ts"]));
    assert_ok(&f.port(&["check"]));

    // ── The brief for store.ts reuses what is already ported. ────────────
    let store_brief = text(&f.port(&["brief", "src/ts/app/src/store.ts"]));
    assert!(
        store_brief.contains("`slugify` = `app::util::slugify`"),
        "{store_brief}"
    );
    assert!(
        store_brief.contains("`@real/sdk` → the sdk crate"),
        "{store_brief}"
    );
    assert!(store_brief.contains("**async function**"), "{store_brief}");
    assert!(
        store_brief.contains("Hazard: futures are lazy"),
        "{store_brief}"
    );
    assert!(
        store_brief.contains("`src/ts/kit/src/helpers.ts` → NOT PORTED"),
        "{store_brief}"
    );

    // ── Lookup across the index, TS exports, and Rust items. ─────────────
    let found = f.port_json(&["find", "slugify"]);
    let sources: Vec<&str> = found
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["source"].as_str().unwrap())
        .collect();
    assert!(
        sources.contains(&"index") && sources.contains(&"typescript") && sources.contains(&"rust")
    );

    // ── Progress and drift. ──────────────────────────────────────────────
    let status = f.port_json(&["status"]);
    assert_eq!(status["files"]["done"], 2);
    assert_eq!(status["module_tests"]["ported"], 1);

    f.rust(
        "util.rs",
        "pub fn renamed(s: &str) -> String { s.to_lowercase() }\n",
    );
    let drift = f.port(&["check"]);
    assert!(!drift.status.success());
    assert!(
        text(&drift).contains("`slugify` maps to app::util::slugify, which is not in util.rs"),
        "{}",
        text(&drift)
    );

    // An uncompiled file is caught too.
    f.rust(
        "util.rs",
        "pub fn slugify(s: &str) -> String { s.to_lowercase() }\n",
    );
    f.rust("lib.rs", "pub mod store;\npub mod types;\n");
    let orphan = f.port(&["check"]);
    assert!(!orphan.status.success());
    assert!(
        text(&orphan).contains("util.rs is not declared with `mod`"),
        "{}",
        text(&orphan)
    );
}

#[test]
fn every_shipped_example_loads_and_its_mapping_queries_compile() {
    // The starter files under examples/ are what new projects copy: a broken
    // query, a config key the schema rejects, or a bad component table would
    // fail a project's first run.
    let examples = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/typescript");

    // Their own config, against the layout it names.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let f = Fixture { _dir: dir, root };
    std::fs::create_dir_all(f.root.join("port")).unwrap();
    for (from, to) in [
        ("rustify.toml", "rustify.toml"),
        ("mappings.toml", "port/mappings.toml"),
        ("components.toml", "port/components.toml"),
    ] {
        std::fs::copy(examples.join(from), f.root.join(to)).unwrap();
    }
    f.write(
        "packages/app/package.json",
        r#"{"name":"@acme/app","dependencies":{"@acme/tui":"workspace:*"}}"#,
    );
    f.write(
        "packages/app/src/main.ts",
        "import { Row } from \"@acme/tui/layout\";\nexport async function main() { for (const x of [1]) { await Promise.all([x]); } }\n",
    );
    f.write("packages/tui/package.json", r#"{"name":"@acme/tui"}"#);
    f.write("packages/tui/src/layout.ts", "export class Row {}\n");
    f.write("rust/app/src/lib.rs", "");
    f.git(&["init", "-q", "-b", "main"]);
    f.land_upstream("example");

    // Graph builds (every query compiles) and the UI module routes into the
    // library crate the example names.
    assert_ok(&f.port(&["graph"]));
    let brief = text(&f.port(&["brief", "packages/tui/src/layout.ts"]));
    assert!(brief.contains("`app_ui::layout`"), "{brief}");
    let brief = text(&f.port(&["brief", "packages/app/src/main.ts"]));
    assert!(brief.contains("Promise.all"), "{brief}");
}

#[test]
fn finds_rustify_toml_above_the_working_directory_and_keeps_state_beside_it() {
    // Setup: a config nested in the repository (not at its root) with no
    // `state_dir`, run from a directory below it with no flags.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let f = Fixture { _dir: dir, root };
    f.write(
        "tools/port/rustify.toml",
        r#"
upstream = "origin/trunk"
conventions = "docs/porting.md"
roots = ["web/app/src"]
packages_dir = "web"
primary_package = "app"
rust_crate = "rust/app"
rust_crate_name = "app"
test_markers = [".test.ts"]
[batch]
max_files = 4
max_lines = 400
"#,
    );
    f.write("tools/port/mappings.toml", "");
    f.write("web/app/package.json", r#"{"name":"app"}"#);
    f.write("web/app/src/a.ts", "export const a = 1;\n");
    f.write("rust/app/src/lib.rs", "pub mod a;\n");
    f.write("rust/app/src/a.rs", "pub const A: i32 = 1;\n");
    f.write("tools/port/deep/.keep", "");
    f.git(&["init", "-q", "-b", "trunk"]);
    f.git(&["add", "-A"]);
    f.git(&["commit", "-qm", "init"]);
    f.git(&["update-ref", "refs/remotes/origin/trunk", "HEAD"]);

    let run = |args: &[&str]| {
        std::process::Command::new(env!("CARGO_BIN_EXE_rustify"))
            .args(args)
            .current_dir(f.root.join("tools/port/deep"))
            .output()
            .unwrap()
    };

    // The brief names the configured conventions doc.
    let brief = text(&run(&["brief", "web/app/src/a.ts"]));
    assert!(brief.contains("Conventions: docs/porting.md."), "{brief}");

    // `done` stamps against the configured upstream and writes the index
    // beside rustify.toml.
    assert_ok(&run(&["start", "web/app/src/a.ts"]));
    assert_ok(&run(&["done", "web/app/src/a.ts"]));
    let index = std::fs::read_to_string(f.root.join("tools/port/index.toml")).unwrap();
    let head = f.git(&["rev-parse", "HEAD"]);
    assert!(index.contains(&format!("ts_base = \"{head}\"")), "{index}");

    // The ratchet defaults its base to the configured upstream too.
    f.git(&["add", "-A"]);
    f.git(&["commit", "-qm", "port a"]);
    let ratchet = run(&["ratchet"]);
    assert_ok(&ratchet);

    // Outcome: with no rustify.toml above it, the tool says how to point at one.
    let outside = tempfile::tempdir().unwrap();
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_rustify"))
        .arg("status")
        .current_dir(outside.path())
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(
        text(&out).contains("pass --config <path>"),
        "{}",
        text(&out)
    );
}

#[test]
fn reports_loops_that_await_and_ts_files_that_collide_on_one_rust_module() {
    let f = fixture();
    // A `contains` rule: report loops only when they await in their own body.
    f.write(
        "port/mappings.toml",
        r#"
[[construct]]
id = "await-in-loop"
title = "await inside a loop"
contains = "await_expression"
query = '[(for_statement) @match (for_in_statement) @match]'
rust = "JoinSet when iterations are independent"
"#,
    );
    f.write(
        "src/ts/app/src/util.ts",
        r#"export async function slugify(items: string[]) {
  for (const item of items) {
    if (item) {
      const done = await save(item);
    }
  }
  for (const item of items) {
    items.map(async () => await save(item));
  }
}
async function save(_: string) {}
"#,
    );
    // `hooks.ts` and `hooks/index.ts` would both become `app::hooks`.
    f.write("src/ts/app/src/hooks.ts", "export const a = 1;\n");
    f.write("src/ts/app/src/hooks/index.ts", "export const b = 2;\n");

    let brief = text(&f.port(&["brief", "src/ts/app/src/util.ts"]));
    assert!(
        brief.contains("**await inside a loop** (lines 4)"),
        "{brief}"
    );

    let check = f.port(&["check"]);
    assert!(!check.status.success());
    assert!(
        text(&check).contains(
            "src/ts/app/src/hooks.ts, src/ts/app/src/hooks/index.ts all map to app::hooks"
        ),
        "{}",
        text(&check)
    );
}

#[test]
fn routes_ui_components_into_the_library_and_holds_their_ports_to_ink_goldens() {
    // Setup: the fixture app gains two Ink components. `Picker.tsx` is a
    // shared widget routed into a separate UI library crate by
    // port/components.toml; `Screen.tsx` stays in the app and uses it.
    let f = fixture();
    f.write(
        "port/components.toml",
        r#"
[library]
crate = "crates/ui"
crate_name = "ui"
goldens = "crates/ui/goldens"
golden_test = "src/ts/app/src/__tests__/ui-goldens.test.tsx"

[[module]]
ts = "src/ts/app/src/Picker.tsx"
rust = "widgets/picker.rs"

[[widget]]
name = "Picker"
rust = "ui::widgets::picker::Picker"
file = "widgets/picker.rs"
from = ["src/ts/app/src/Picker.tsx"]
golden = "picker.txt"
status = "available"
summary = "option list"

[[ink]]
names = ["Box"]
rust = "layout::Row"
hazards = ["Yoga shrinks fixed cells"]
"#,
    );
    f.write(
        "src/ts/app/src/Picker.tsx",
        "import { Box, Text } from \"ink\";\nimport { slugify } from \"./util\";\nexport function Picker() { return <Box><Text>{slugify(\"x\")}</Text></Box>; }\n",
    );
    f.write(
        "src/ts/app/src/Screen.tsx",
        "import { Box } from \"ink\";\nimport { Picker } from \"./Picker\";\nexport function Screen() { return <Box><Picker /></Box>; }\n",
    );
    f.write("crates/ui/goldens/picker.txt", "### one\n› a\n");

    // The brief for the library component names the library crate, its
    // module, the library rules, and the crate to test.
    let brief = text(&f.port(&["brief", "src/ts/app/src/Picker.tsx"]));
    assert!(
        brief.contains(
            "| `src/ts/app/src/Picker.tsx` | `widgets/picker.rs` | `ui::widgets::picker` |"
        ),
        "{brief}"
    );
    assert!(
        brief.contains("belongs to the component library"),
        "{brief}"
    );
    assert!(brief.contains("`cargo test -p ui`"), "{brief}");
    assert!(brief.contains("`Box` → layout::Row"), "{brief}");
    assert!(
        brief.contains("Hazard: Yoga shrinks fixed cells"),
        "{brief}"
    );
    assert!(brief.contains("No [[ink]] rule for `Text`"), "{brief}");

    // The screen that imports it is told to reuse the widget, not rebuild it.
    let brief = text(&f.port(&["brief", "src/ts/app/src/Screen.tsx"]));
    assert!(
        brief.contains(
            "`ui::widgets::picker::Picker` (available, use it) replaces `src/ts/app/src/Picker.tsx`"
        ),
        "{brief}"
    );
    assert!(
        brief.contains("| `src/ts/app/src/Screen.tsx` | `screen.rs` | `app::screen` |"),
        "{brief}"
    );

    // A widget marked available must exist.
    let check = f.port(&["check"]);
    assert!(!check.status.success());
    assert!(
        text(&check).contains("widget Picker is available but"),
        "{}",
        text(&check)
    );

    // The widget exists, but nothing reads its golden: Ink changes would go
    // unchecked, so check still fails.
    f.write("crates/ui/src/lib.rs", "pub mod widgets;\n");
    f.write("crates/ui/src/widgets/mod.rs", "pub mod picker;\n");
    f.write("crates/ui/src/widgets/picker.rs", "pub struct Picker;\n");
    let check = text(&f.port(&["check"]));
    assert!(
        check.contains("golden crates/ui/goldens/picker.txt is not read by any Rust test"),
        "{check}"
    );

    // Port the dependency, then the component. `done` records the symbol in
    // the library crate's module path.
    f.rust("lib.rs", "pub mod util;\n");
    f.rust(
        "util.rs",
        "pub fn slugify(s: &str) -> String { s.to_lowercase() }\n",
    );
    assert_ok(&f.port(&["start", "src/ts/app/src/util.ts"]));
    assert_ok(&f.port(&["done", "src/ts/app/src/util.ts"]));
    assert_ok(&f.port(&["start", "src/ts/app/src/Picker.tsx"]));
    assert_ok(&f.port(&["done", "src/ts/app/src/Picker.tsx"]));
    let index = std::fs::read_to_string(f.root.join("port/index.toml")).unwrap();
    assert!(
        index.contains("Picker = \"ui::widgets::picker::Picker\""),
        "{index}"
    );

    // A ported .tsx without a golden parity test fails the check.
    let check = text(&f.port(&["check"]));
    assert!(
        check.contains(
            "src/ts/app/src/Picker.tsx is ported but widgets/picker.rs has no golden parity test"
        ),
        "{check}"
    );

    // Outcome: with a parity test reading the golden, the port checks clean.
    f.write(
        "crates/ui/src/widgets/picker.rs",
        "pub struct Picker;\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn matches_ink() { crate::golden::assert_file(\"picker.txt\", &[], |_| vec![]); }\n}\n",
    );
    let check = f.port(&["check"]);
    assert_ok(&check);
    assert!(text(&check).contains("0 error(s)"), "{}", text(&check));
}

#[test]
fn plans_a_wave_of_batches_that_parallel_agents_can_port_without_colliding() {
    // Setup: two ready modules in different directories both import the type
    // `Store` from unported store.ts, so each batch must declare it
    // types-first in store.rs. Ported in parallel, they would both write
    // store.rs.
    let f = fixture();
    f.write(
        "src/ts/app/src/views/summary.ts",
        "import type { Store } from \"../store\";\nexport function describe(s?: Store): string { return \"store\"; }\n",
    );
    let batch_with = |batches: &Value, rel: &str| {
        batches
            .as_array()
            .unwrap()
            .iter()
            .position(|b| files_of(b).iter().any(|f| f == rel))
    };

    // Plain ranking lists both batches.
    let ranked = f.port_json(&["next", "--count", "10"]);
    assert!(batch_with(&ranked, "src/ts/app/src/types.ts").is_some());
    assert!(batch_with(&ranked, "src/ts/app/src/views/summary.ts").is_some());

    // A wave keeps only one of them: no two batches may write the same file.
    let wave = f.port_json(&["next", "--independent", "--count", "10"]);
    let with_types = batch_with(&wave, "src/ts/app/src/types.ts");
    let with_summary = batch_with(&wave, "src/ts/app/src/views/summary.ts");
    assert!(
        with_types.is_some() != with_summary.is_some(),
        "exactly one store.rs writer per wave: {wave:#}"
    );
    let writes: Vec<Vec<String>> = wave
        .as_array()
        .unwrap()
        .iter()
        .map(|b| {
            b["footprint"]["writes"]
                .as_array()
                .unwrap()
                .iter()
                .map(|w| w.as_str().unwrap().to_string())
                .collect()
        })
        .collect();
    for (i, a) in writes.iter().enumerate() {
        for b in &writes[i + 1..] {
            assert!(a.iter().all(|w| !b.contains(w)), "{a:?} overlaps {b:?}");
        }
    }
    let store_writer = with_types.or(with_summary).unwrap();
    assert!(
        writes[store_writer].contains(&"app/store.rs".to_string()),
        "{wave:#}"
    );

    // Shared parent `mod` lines do not block a wave: root-level batches all
    // register into lib.rs and still port side by side.
    let registering_lib = wave
        .as_array()
        .unwrap()
        .iter()
        .filter(|b| {
            b["footprint"]["registers"]
                .as_array()
                .unwrap()
                .iter()
                .any(|r| r == "app/lib.rs")
        })
        .count();
    assert!(registering_lib >= 2, "{wave:#}");

    // Outcome: each batch's brief is written as a standalone subagent prompt.
    let briefs = f.root.join("briefs");
    let out = f.port(&[
        "next",
        "--independent",
        "--count",
        "10",
        "--write-briefs",
        briefs.to_str().unwrap(),
    ]);
    assert_ok(&out);
    for n in 1..=wave.as_array().unwrap().len() {
        let brief = std::fs::read_to_string(briefs.join(format!("batch-{n}.md"))).unwrap();
        assert!(brief.starts_with("# Port brief"), "{brief}");
    }
}

#[test]
fn catches_up_with_upstream_in_dependency_order_after_ported_ts_changes() {
    // Setup: util.ts and greet.ts (which imports util) are ported from the
    // first upstream commit.
    let f = fixture();
    f.write(
        "src/ts/app/src/greet.ts",
        "import { slugify } from \"./util\";\nexport function greet(n: string): string { return slugify(n); }\n",
    );
    f.land_upstream("add greet");
    f.rust("lib.rs", "pub mod greet;\npub mod util;\n");
    f.rust(
        "util.rs",
        "pub fn slugify(s: &str) -> String { s.to_lowercase() }\n",
    );
    f.rust(
        "greet.rs",
        "pub fn greet(n: &str) -> String { crate::util::slugify(n) }\n",
    );
    assert_ok(&f.port(&["start", "src/ts/app/src/util.ts", "src/ts/app/src/greet.ts"]));
    assert_ok(&f.port(&[
        "done",
        "src/ts/app/src/util.ts",
        "src/ts/app/src/greet.ts",
        "--test",
        "src/ts/app/src/__tests__/util.test.ts",
    ]));
    let caught_up = f.port_json(&["next", "--catch-up"]);
    assert_eq!(caught_up, serde_json::json!([]), "nothing drifted yet");

    // Upstream lands later PRs: util.ts is edited twice, greet.ts once, and
    // a new module that imports util.ts appears.
    f.write(
        "src/ts/app/src/util.ts",
        "export function slugify(s: string): string { return s.trim().toLowerCase(); }\n",
    );
    f.land_upstream("util: trim");
    f.write(
        "src/ts/app/src/util.ts",
        "export function slugify(s: string): string { return s.trim().toLowerCase().replace(/ /g, \"-\"); }\n",
    );
    f.write(
        "src/ts/app/src/greet.ts",
        "import { slugify } from \"./util\";\nexport function greet(n: string): string { return `hi ${slugify(n)}`; }\n",
    );
    f.write(
        "src/ts/app/src/shout.ts",
        "import { slugify } from \"./util\";\nexport const shout = (s: string) => slugify(s).toUpperCase();\n",
    );
    // Another new module builds on kit/helpers.ts, which was never ported:
    // that is ordinary porting work, not catch-up.
    f.write(
        "src/ts/app/src/deep.ts",
        "import { helper } from \"@x/kit/helpers\";\nexport const deep = () => helper();\n",
    );
    f.land_upstream("util: dashes; greet: prefix; add shout and deep");

    // Catch-up offers only util.ts first: greet.ts and the new shout.ts
    // import it, so they wait until its update is recorded.
    let first = f.port_json(&["next", "--catch-up", "--count", "5"]);
    let first = first.as_array().unwrap();
    assert_eq!(first.len(), 1, "{first:#?}");
    assert_eq!(files_of(&first[0]), ["src/ts/app/src/util.ts"]);
    let base = first[0]["files"][0]["update_since"].as_str().unwrap();
    assert_eq!(
        first[0]["tests"][0]["ts"],
        "src/ts/app/src/__tests__/util.test.ts"
    );
    // The brief carries both upstream edits as one diff from the recorded base.
    let brief = text(&f.port(&["next", "--catch-up", "--brief"]));
    assert!(brief.contains("Update, not a fresh port"), "{brief}");
    assert!(brief.contains("replace(/ /g"), "{brief}");
    assert!(brief.contains(&base[..12]), "{brief}");
    // Recording and checking are unaffected by staleness.
    assert_ok(&f.port(&["check"]));

    // Port util's update and re-stamp it: its dependents become ready, the
    // stale greet.ts as an update and the new shout.ts as a fresh port, in
    // separate batches.
    assert_ok(&f.port(&["start", "src/ts/app/src/util.ts"]));
    // Claimed for its update, it still briefs as one.
    let claimed = text(&f.port(&["brief", "src/ts/app/src/util.ts"]));
    assert!(claimed.contains("Update, not a fresh port"), "{claimed}");
    assert_ok(&f.port(&["done", "src/ts/app/src/util.ts"]));
    let second = f.port_json(&["next", "--catch-up", "--count", "5"]);
    let second = second.as_array().unwrap();
    let greet = second
        .iter()
        .find(|b| files_of(b) == ["src/ts/app/src/greet.ts"])
        .unwrap_or_else(|| panic!("greet.ts offered alone: {second:#?}"));
    assert!(greet["files"][0]["update_since"].is_string());
    let shout = second
        .iter()
        .find(|b| files_of(b) == ["src/ts/app/src/shout.ts"])
        .unwrap_or_else(|| panic!("shout.ts offered alone: {second:#?}"));
    assert_eq!(shout["files"][0]["added_upstream"], true);
    assert!(shout["files"][0]["update_since"].is_null());
    assert!(
        second
            .iter()
            .all(|b| !files_of(b).contains(&"src/ts/app/src/deep.ts".to_string())),
        "{second:#?}"
    );
    // Outside catch-up, unrelated unported modules are offered as well.
    let everything = f.port_json(&["next", "--count", "10"]);
    assert!(
        everything
            .as_array()
            .unwrap()
            .iter()
            .any(|b| files_of(b).contains(&"src/ts/kit/src/helpers.ts".to_string())),
        "{everything:#}"
    );
}

#[test]
fn briefs_an_unchanged_cycle_partner_of_a_stale_module_as_needing_no_edit() {
    // Setup: the cycle-a.ts / cycle-b.ts import cycle is ported as one unit.
    let f = fixture();
    f.rust("lib.rs", "pub mod cycle_a;\npub mod cycle_b;\n");
    f.rust("cycle_a.rs", "pub fn a() {}\n");
    f.rust("cycle_b.rs", "pub fn b() {}\n");
    let cycle = ["src/ts/app/src/cycle-a.ts", "src/ts/app/src/cycle-b.ts"];
    assert_ok(&f.port(&["start", cycle[0], cycle[1]]));
    assert_ok(&f.port(&["done", cycle[0], cycle[1]]));

    // Upstream edits only cycle-a.ts.
    f.write(
        "src/ts/app/src/cycle-a.ts",
        "import { b } from \"./cycle-b\";\nexport const a = () => b() && 1;\n",
    );
    f.land_upstream("cycle-a: return 1");

    // The unit comes back as one batch: cycle-a as an update, cycle-b
    // flagged as unchanged rather than briefed like a fresh port.
    let brief = text(&f.port(&["next", "--catch-up", "--brief"]));
    assert!(brief.contains("Update, not a fresh port"), "{brief}");
    assert!(
        brief.contains("**Already ported, unchanged upstream.**"),
        "{brief}"
    );
    assert_eq!(
        brief.matches("Update, not a fresh port").count(),
        1,
        "{brief}"
    );
}

/// Ports `util.ts` and `greet.ts` (with util's test) from the fixture's
/// first commit and lands the index upstream, as a merged port PR would.
fn fixture_with_util_and_greet_ported() -> Fixture {
    let f = fixture();
    f.write(
        "src/ts/app/src/greet.ts",
        "import { slugify } from \"./util\";\nexport function greet(n: string): string { return slugify(n); }\n",
    );
    f.land_upstream("add greet");
    f.rust("lib.rs", "pub mod greet;\npub mod util;\n");
    f.rust(
        "util.rs",
        "pub fn slugify(s: &str) -> String { s.to_lowercase() }\n",
    );
    f.rust(
        "greet.rs",
        "pub fn greet(n: &str) -> String { crate::util::slugify(n) }\n",
    );
    assert_ok(&f.port(&["start", "src/ts/app/src/util.ts", "src/ts/app/src/greet.ts"]));
    assert_ok(&f.port(&[
        "done",
        "src/ts/app/src/util.ts",
        "src/ts/app/src/greet.ts",
        "--test",
        "src/ts/app/src/__tests__/util.test.ts",
    ]));
    f
}

#[test]
fn stamp_blobs_backfills_the_ported_ts_from_each_entrys_ts_base() {
    // Setup: an index written before `ts_blobs` existed (the field removed
    // by hand), whose TS then changed upstream.
    let f = fixture_with_util_and_greet_ported();
    let index_path = "port/index.toml";
    let recorded = std::fs::read_to_string(f.root.join(index_path)).unwrap();
    assert!(recorded.contains("[module.ts_blobs]"), "done records blobs");
    let legacy: String = recorded
        .lines()
        .filter(|l| !l.starts_with("[module.ts_blobs]") && !l.starts_with("\"src/ts/"))
        .map(|l| format!("{l}\n"))
        .collect();
    f.write(index_path, &legacy);
    f.land_upstream("legacy index");
    f.write(
        "src/ts/app/src/util.ts",
        "export function slugify(s: string): string { return s.trim(); }\n",
    );
    f.land_upstream("util edited after it was ported");

    // Action: backfill from ts_base, then drift is computed from blobs.
    assert_ok(&f.port(&["stamp-blobs"]));

    // Outcome: the blobs are the ported content (not the edited file), so
    // util.ts reads as drifted and greet.ts does not.
    let backfilled = std::fs::read_to_string(f.root.join(index_path)).unwrap();
    assert_eq!(backfilled.matches("[module.ts_blobs]").count(), 2);
    let drift = f.port_json(&["drift"]);
    let drifted: Vec<&str> = drift["drifted"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["ts"].as_str().unwrap())
        .collect();
    assert_eq!(drifted, ["src/ts/app/src/util.ts"]);
}

#[test]
fn a_pr_that_edits_ported_typescript_must_port_it_and_run_done_before_ratchet_passes() {
    // Setup: util.ts and greet.ts are ported on main. Later, main itself
    // changes greet.ts without porting it: greet.ts is already stale where
    // the PR branches, so it is grandfathered.
    let f = fixture_with_util_and_greet_ported();
    f.land_upstream("port util and greet");
    f.write(
        "src/ts/app/src/greet.ts",
        "import { slugify } from \"./util\";\nexport function greet(n: string): string { return `hi ${slugify(n)}`; }\n",
    );
    f.land_upstream("greet: prefix (not ported)");
    f.git(&["checkout", "-q", "-b", "pr"]);

    // A branch that changes nothing passes: the stale set did not grow.
    let untouched = f.port(&["ratchet", "--base", "origin/main"]);
    assert_ok(&untouched);
    assert!(
        text(&untouched).contains("1 stale at the merge base"),
        "{}",
        text(&untouched)
    );

    // Action: the PR edits util.ts (a fresh module) and greet.ts again, but
    // does not touch the Rust or the index.
    f.write(
        "src/ts/app/src/util.ts",
        "export function slugify(s: string): string { return s.trim().toLowerCase(); }\n",
    );
    f.write(
        "src/ts/app/src/greet.ts",
        "import { slugify } from \"./util\";\nexport function greet(n: string): string { return `hello ${slugify(n)}`; }\n",
    );
    f.git(&["add", "-A"]);
    f.git(&["commit", "-qm", "ts: util trims, greet says hello"]);

    // Outcome: the ratchet fails naming util.ts, what changed, the Rust file
    // to update, and the fix. greet.ts stays grandfathered.
    let failed = f.port(&["ratchet", "--base", "origin/main"]);
    let report = text(&failed);
    assert_eq!(failed.status.code(), Some(1), "{report}");
    assert!(report.contains("error: src/ts/app/src/util.ts"), "{report}");
    assert!(
        report.contains("changed: src/ts/app/src/util.ts"),
        "{report}"
    );
    assert!(report.contains("crates/app/src/util.rs"), "{report}");
    assert!(
        report.contains("rustify done src/ts/app/src/util.ts"),
        "{report}"
    );
    assert!(
        !report.contains("error: src/ts/app/src/greet.ts"),
        "{report}"
    );

    // Action: the PR ports the change into Rust and records it with `done`
    // (which hashes the working-tree TS it was ported against).
    f.rust(
        "util.rs",
        "pub fn slugify(s: &str) -> String { s.trim().to_lowercase() }\n",
    );
    assert_ok(&f.port(&[
        "done",
        "src/ts/app/src/util.ts",
        "--test",
        "src/ts/app/src/__tests__/util.test.ts",
    ]));
    f.git(&["add", "-A"]);
    f.git(&["commit", "-qm", "port util trim into Rust"]);

    // Outcome: ratchet passes; the grandfathered greet.ts is still stale
    // but no staler than at the merge base.
    let passed = f.port(&["ratchet", "--base", "origin/main"]);
    assert_ok(&passed);
    assert!(text(&passed).starts_with("OK:"), "{}", text(&passed));

    // Action: after the PR rebase-merges, main holds the same TS content
    // under new commit ids and the PR's index. Staleness follows content,
    // so util.ts is not stale on main (greet.ts, still unported, is).
    f.git(&["update-ref", "refs/remotes/origin/main", "HEAD"]);
    let drift = f.port_json(&["drift"]);
    let drifted: Vec<&str> = drift["drifted"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["ts"].as_str().unwrap())
        .collect();
    assert_eq!(drifted, ["src/ts/app/src/greet.ts"]);
}

#[test]
fn a_pr_cannot_escape_the_ratchet_by_dropping_an_entry_but_may_delete_the_ts_with_it() {
    // Setup: util.ts and greet.ts are fresh ports on main; the PR branches.
    let f = fixture_with_util_and_greet_ported();
    f.land_upstream("port util and greet");
    f.git(&["checkout", "-q", "-b", "pr"]);
    let index_path = f.root.join("port/index.toml");
    let original = std::fs::read_to_string(&index_path).unwrap();

    // Action: the PR edits util.ts and deletes its index entry instead of
    // porting the change.
    f.write(
        "src/ts/app/src/util.ts",
        "export function slugify(s: string): string { return s.trim().toLowerCase(); }\n",
    );
    let without_util: String = original
        .split("[[module]]\n")
        .filter(|block| !block.contains("ts = \"src/ts/app/src/util.ts\""))
        .collect::<Vec<_>>()
        .join("[[module]]\n");
    std::fs::write(&index_path, without_util).unwrap();
    f.git(&["add", "-A"]);
    f.git(&["commit", "-qm", "edit util, drop its entry"]);

    // Outcome: the ratchet names the dropped entry and how to fix it.
    let failed = f.port(&["ratchet", "--base", "origin/main"]);
    let report = text(&failed);
    assert_eq!(failed.status.code(), Some(1), "{report}");
    assert!(
        report.contains("error: src/ts/app/src/util.ts was a fresh ported module")
            && report.contains("removed from the index")
            && report.contains("restore the entry"),
        "{report}"
    );

    // Action: restore the index and delete greet.ts (nothing imports it).
    std::fs::write(&index_path, &original).unwrap();
    f.git(&["checkout", "-q", "HEAD~1", "--", "src/ts/app/src/util.ts"]);
    std::fs::remove_file(f.root.join("src/ts/app/src/greet.ts")).unwrap();
    f.git(&["add", "-A"]);
    f.git(&["commit", "-qm", "delete greet.ts, keep its entry"]);

    // Outcome: a tracked entry whose TS vanished says to remove or re-point
    // it, not to run `done`.
    let stale = f.port(&["ratchet", "--base", "origin/main"]);
    let report = text(&stale);
    assert_eq!(stale.status.code(), Some(1), "{report}");
    assert!(
        report.contains(
            "greet.ts is a ported module whose TypeScript this branch deleted or renamed"
        ) && report.contains("remove its entry"),
        "{report}"
    );

    // Action: delete the entry with the file.
    let without_greet: String = original
        .split("[[module]]\n")
        .filter(|block| !block.contains("ts = \"src/ts/app/src/greet.ts\""))
        .collect::<Vec<_>>()
        .join("[[module]]\n");
    std::fs::write(&index_path, without_greet).unwrap();
    f.git(&["add", "-A"]);
    f.git(&["commit", "-qm", "drop greet entry"]);

    // Outcome: passes.
    assert_ok(&f.port(&["ratchet", "--base", "origin/main"]));
}

#[test]
fn the_ratchet_passes_on_the_branch_that_adopts_rustify() {
    // Setup: upstream has the TypeScript but no rustify files yet; this
    // branch adds rustify.toml and its state. The merge base has no config to
    // build its graph with.
    let f = fixture();
    f.git(&["rm", "-rq", "--cached", "rustify.toml", "port"]);
    f.git(&["commit", "-qm", "upstream without rustify"]);
    f.git(&["update-ref", "refs/remotes/origin/main", "HEAD"]);
    f.git(&["add", "-A"]);
    f.git(&["commit", "-qm", "adopt rustify"]);

    // Outcome: the base graph is read with the branch's config, so the gate
    // reports instead of failing to load.
    let out = f.port(&["ratchet"]);
    assert_ok(&out);
    assert!(text(&out).starts_with("OK:"), "{}", text(&out));
}

#[test]
fn ports_javascript_modules_when_their_extensions_are_configured() {
    // Setup: a CLI package written in plain `.mjs` that imports a TS module
    // from the same repository and another `.mjs` file by its full name.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let f = Fixture { _dir: dir, root };
    let config = |extensions: &str| {
        format!(
            r#"
roots = ["packages/cli/bin"]
packages_dir = "packages"
primary_package = "cli"
rust_crate = "rust/cli"
rust_crate_name = "cli"
{extensions}
test_markers = [".test."]
[batch]
max_files = 6
max_lines = 800
"#
        )
    };
    f.write("rustify.toml", &config(""));
    f.write("mappings.toml", "");
    f.write("packages/cli/package.json", r#"{"name":"cli"}"#);
    f.write(
        "packages/cli/bin/cli.mjs",
        "import { doctor } from \"./doctor.mjs\";\nimport { slug } from \"../src/slug\";\nexport function main() { doctor(); slug(); }\n",
    );
    f.write(
        "packages/cli/bin/doctor.mjs",
        "export function doctor() {}\n",
    );
    f.write("packages/cli/src/slug.ts", "export function slug() {}\n");
    f.write(
        "packages/cli/bin/cli.test.mjs",
        "import { main } from \"./cli.mjs\";\n",
    );
    f.write("rust/cli/src/lib.rs", "");
    f.git(&["init", "-q", "-b", "main"]);
    f.land_upstream("js cli");

    // By default only TypeScript is in the graph: the roots hold no sources.
    let graph = f.port_json(&["graph"]);
    assert_eq!(graph["files"].as_array().unwrap().len(), 0, "{graph:#}");

    // With `mjs` configured, the .mjs files join the graph, resolve each
    // other and the TS module, and map to Rust files without the extension.
    f.write(
        "rustify.toml",
        &config(r#"extensions = ["ts", "tsx", "mjs"]"#),
    );
    let graph = f.port_json(&["graph"]);
    let files: Vec<&str> = graph["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["ts"].as_str().unwrap())
        .collect();
    assert_eq!(
        files,
        [
            "packages/cli/bin/cli.mjs",
            "packages/cli/bin/doctor.mjs",
            "packages/cli/src/slug.ts"
        ]
    );
    let brief = text(&f.port(&["brief", "packages/cli/bin/cli.mjs"]));
    assert!(
        brief.contains("| `packages/cli/bin/cli.mjs` | `bin/cli.rs` | `cli::bin::cli` |"),
        "{brief}"
    );
    assert!(brief.contains("`packages/cli/bin/cli.test.mjs`"), "{brief}");

    // Outcome: the JS module ports and records like a TS one.
    f.write("rust/cli/src/lib.rs", "pub mod bin;\npub mod slug;\n");
    f.write("rust/cli/src/slug.rs", "pub fn slug() {}\n");
    f.write("rust/cli/src/bin/mod.rs", "pub mod doctor;\n");
    f.write("rust/cli/src/bin/doctor.rs", "pub fn doctor() {}\n");
    assert_ok(&f.port(&["start", "packages/cli/bin/doctor.mjs"]));
    assert_ok(&f.port(&["done", "packages/cli/bin/doctor.mjs"]));
    let index = std::fs::read_to_string(f.root.join("index.toml")).unwrap();
    assert!(
        index.contains("doctor = \"cli::bin::doctor::doctor\""),
        "{index}"
    );
}

#[test]
fn e2e_separates_tests_that_only_run_the_program_from_tests_that_import_its_source() {
    let f = fixture();
    // Runs the program through a helper and imports no source: it can run
    // against the Rust binary unchanged. It sits outside the configured
    // binary_test_markers directory, which the report points out.
    f.write(
        "src/ts/app/src/__tests__/helpers/run.ts",
        "import { spawnSync } from \"node:child_process\";\nexport const run = (args: string[]) => spawnSync(process.env.APP_BIN!, args);\n",
    );
    f.write(
        "src/ts/app/src/__tests__/protocol.test.ts",
        "import { expect, test } from \"vitest\";\nimport stripAnsi from \"strip-ansi\";\nimport { run } from \"./helpers/run\";\ntest(\"help\", () => { expect(run([\"--help\"]).status).toBe(0); });\n",
    );
    // Runs the program but also reaches into its source.
    f.write(
        "src/ts/app/src/__tests__/e2e/mixed.e2e.test.ts",
        "import { execa } from \"execa\";\nimport { slugify } from \"../../util\";\nimport { helper } from \"@x/kit/helpers\";\n",
    );
    // Reaches source two less direct ways: a workspace package it does not
    // declare as `workspace:*`, and another package's build output.
    f.write(
        "src/ts/app/src/__tests__/e2e/built.e2e.test.ts",
        "import { spawn } from \"child_process\";\nimport { Client } from \"@real/sdk\";\nimport { helper } from \"../../../../kit/dist/helpers.js\";\n",
    );
    // A type import is erased, so this one still needs no source. It lives in
    // a package of its own that no source file imports, and starts the
    // program through a runtime global instead of an import.
    f.write(
        "src/ts/acceptance/package.json",
        r#"{"name":"@x/acceptance"}"#,
    );
    f.write(
        "src/ts/acceptance/__tests__/smoke.test.ts",
        "import type { Options } from \"../../app/src/types\";\nimport { join } from \"path/posix\";\nawait Bun.spawn([process.env.APP_BIN!]).exited;\n",
    );
    // A tsconfig path alias reaches source even though it looks like a
    // package name.
    f.write(
        "src/ts/app/src/__tests__/e2e/alias.e2e.test.ts",
        "import { execa } from \"execa\";\nimport { slugify } from \"@/util\";\n",
    );

    let report = f.port_json(&["e2e"]);
    let names = |key: &str| -> Vec<String> {
        report[key]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["ts"].as_str().unwrap().to_string())
            .collect()
    };
    assert_eq!(
        names("black_box"),
        [
            "src/ts/acceptance/__tests__/smoke.test.ts",
            "src/ts/app/src/__tests__/protocol.test.ts"
        ]
    );
    assert_eq!(report["black_box"][0]["needs"], serde_json::json!([]));
    let protocol = &report["black_box"][1];
    assert_eq!(protocol["binary_marked"], false);
    assert_eq!(protocol["needs"], serde_json::json!(["strip-ansi"]));
    assert_eq!(
        names("mixed"),
        [
            "src/ts/app/src/__tests__/e2e/alias.e2e.test.ts",
            "src/ts/app/src/__tests__/e2e/built.e2e.test.ts",
            "src/ts/app/src/__tests__/e2e/mixed.e2e.test.ts"
        ]
    );
    assert_eq!(
        report["mixed"][0]["imports_source"],
        serde_json::json!(["@/util"])
    );
    assert_eq!(
        report["mixed"][1]["imports_source"],
        serde_json::json!(["@real/sdk", "src/ts/kit/src/helpers.ts"])
    );
    assert_eq!(
        report["mixed"][2]["imports_source"],
        serde_json::json!(["@x/kit", "src/ts/app/src/util.ts"])
    );
    // util.test, store.test, and the fixture's cli.e2e.test never spawn.
    assert_eq!(report["in_process"], 3);

    let out = f.port(&["e2e"]);
    assert_ok(&out);
    let printed = text(&out);
    assert!(
        printed.contains("2 run the program as a process without importing its source"),
        "{printed}"
    );
    assert!(
        printed.contains("imports src/ts/app/src/util.ts"),
        "{printed}"
    );
    assert!(
        printed.contains("match no `binary_test_markers` entry"),
        "{printed}"
    );

    // Without such a test the report says the suite is missing.
    std::fs::remove_file(f.root.join("src/ts/app/src/__tests__/protocol.test.ts")).unwrap();
    std::fs::remove_file(f.root.join("src/ts/acceptance/__tests__/smoke.test.ts")).unwrap();
    let printed = text(&f.port(&["e2e"]));
    assert!(
        printed.contains("No test covers the program through its process boundary alone"),
        "{printed}"
    );
}

#[cfg(unix)]
#[test]
fn compare_runs_both_programs_and_fails_only_on_differences_that_are_not_accepted() {
    let f = fixture();
    // Stand-ins for the TypeScript and Rust builds of one CLI. They agree
    // except where each case below says otherwise.
    let program = |greeting: &str, extra: &str| {
        format!(
            r#"case "$1" in
  hello) echo "{greeting}" ;;
  now) echo "at 2026-10-07T01:02:03Z in $PWD" ;;
  write) cat input.txt > out.txt; printf '\000\001' > image.bin; {extra} ;;
  echo) cat ;;
  fail) echo "bad flag" >&2; exit 2 ;;
  hang) sleep 30 ;;
esac
"#
        )
    };
    f.write("bin/old.sh", &program("hello", "date +%s%N > stamp.txt"));
    f.write(
        "bin/new.sh",
        &program("Hello", "date +%s%N > stamp.txt; echo x > extra.txt"),
    );
    f.write("port/fixtures/basic/input.txt", "payload\n");
    let spec = |cases: &str| {
        format!(
            r#"old = ["sh", "{{root}}/bin/old.sh"]
new = ["sh", "{{root}}/bin/new.sh"]
timeout_secs = 2

[[normalize]]
pattern = '\d{{4}}-\d\d-\d\dT[\d:]+Z'
replace = "<TIME>"

[[normalize]]
pattern = '^\d+\n$'
replace = "<NANOS>\n"
{cases}"#
        )
    };

    // Timestamps and the working directory are normalized away; stdin, the
    // exit code, and stderr are carried through.
    f.write(
        "port/compare.toml",
        &spec(
            r#"
[[case]]
name = "now"
args = ["now"]

[[case]]
name = "echo"
args = ["echo"]
stdin = "typed\n"

[[case]]
name = "fail"
args = ["fail"]
"#,
        ),
    );
    let out = f.port(&["compare"]);
    assert_ok(&out);
    assert!(
        text(&out).contains("3 case(s): 3 same, 0 accepted, 0 differ"),
        "{}",
        text(&out)
    );

    // A difference in stdout or in the files written fails the run and
    // names the first differing line.
    f.write(
        "port/compare.toml",
        &spec(
            r#"
[[case]]
name = "hello"
args = ["hello"]

[[case]]
name = "write"
args = ["write"]
fixture = "fixtures/basic"
"#,
        ),
    );
    let out = f.port(&["compare", "--keep", "kept"]);
    assert_eq!(out.status.code(), Some(1));
    let printed = text(&out);
    assert!(
        printed.contains("DIFF hello: stdout (old exit 0, new exit 0)"),
        "{printed}"
    );
    assert!(
        printed.contains("old: hello") && printed.contains("new: Hello"),
        "{printed}"
    );
    assert!(printed.contains("DIFF write: files"), "{printed}");
    assert!(printed.contains("only new wrote extra.txt"), "{printed}");
    assert!(
        printed.contains("2 case(s): 0 same, 0 accepted, 2 differ"),
        "{printed}"
    );
    // The copied fixture and the binary file are listed for both sides.
    let kept = std::fs::read_to_string(f.root.join("kept/write/old.files")).unwrap();
    assert_eq!(kept, "image.bin\ninput.txt\nout.txt\nstamp.txt\n");

    // An accepted difference passes; an acceptance that no longer applies
    // and a program that hangs are both reported.
    f.write(
        "port/compare.toml",
        &spec(
            r#"
[[case]]
name = "hello"
args = ["hello"]
accept = "the Rust build capitalizes the greeting"

[[case]]
name = "fail"
args = ["fail"]
accept = "left over from an old difference"

[[case]]
name = "hang"
args = ["hang"]
"#,
        ),
    );
    // Both sides hanging is a failure, not agreement.
    let out = f.port(&["--json", "compare"]);
    assert_eq!(out.status.code(), Some(1));
    let report: Value = serde_json::from_slice(&out.stdout).expect("stdout is JSON");
    let statuses: Vec<(&str, &str)> = report["cases"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| (c["name"].as_str().unwrap(), c["status"].as_str().unwrap()))
        .collect();
    assert_eq!(
        statuses,
        [("hello", "accepted"), ("fail", "stale"), ("hang", "differ")]
    );
    assert_eq!(report["cases"][1]["old_exit"], "2");
    assert_eq!(report["cases"][2]["old_exit"], "timeout");
    assert_eq!(
        report["cases"][2]["differs"],
        serde_json::json!(["timeout"])
    );
    let printed = text(&f.port(&["compare", "--case", "hello", "--case", "fail"]));
    assert!(
        printed.contains("accepted: the Rust build capitalizes the greeting"),
        "{printed}"
    );
    assert!(printed.contains("STALE fail"), "{printed}");

    // A case name that does not exist is an error, not an empty pass.
    let out = f.port(&["compare", "--case", "nope"]);
    assert!(!out.status.success());
    assert!(
        text(&out).contains("no case named \"nope\""),
        "{}",
        text(&out)
    );
}

#[cfg(unix)]
#[test]
fn compare_bounds_leftover_processes_and_sees_what_kind_of_file_each_side_wrote() {
    let f = fixture();
    // `linger` exits at once but leaves a child holding stdout open for far
    // longer than the timeout. `kinds` leaves the same names behind as
    // different kinds of file. `signal` dies from a different signal on
    // each side.
    let program = |dir_or_file: &str, signal: &str| {
        format!(
            r#"case "$1" in
  linger) sleep 30 & echo started ;;
  kinds) {dir_or_file}; cat seed/link > copied.txt ;;
  signal) kill -{signal} $$ ;;
esac
"#
        )
    };
    f.write("bin/old.sh", &program("mkdir out", "KILL"));
    f.write("bin/new.sh", &program("echo x > out", "TERM"));
    // A fixture with a working and a broken symlink is copied as it is.
    f.write("port/fixtures/links/seed/real.txt", "linked\n");
    std::os::unix::fs::symlink("real.txt", f.root.join("port/fixtures/links/seed/link")).unwrap();
    std::os::unix::fs::symlink("missing", f.root.join("port/fixtures/links/seed/broken")).unwrap();
    f.write(
        "port/compare.toml",
        r#"old = ["sh", "{root}/bin/old.sh"]
new = ["sh", "{root}/bin/new.sh"]
timeout_secs = 1

[[case]]
name = "linger"
args = ["linger"]

[[case]]
name = "kinds"
args = ["kinds"]
fixture = "fixtures/links"

[[case]]
name = "signal"
args = ["signal"]
"#,
    );
    let started = std::time::Instant::now();
    let out = f.port(&["compare", "--keep", "kept"]);
    // Four seconds of grace per side at most, not the 30 the child sleeps.
    assert!(
        started.elapsed() < std::time::Duration::from_secs(15),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(out.status.code(), Some(1));
    let printed = text(&out);
    assert!(
        printed.contains("DIFF linger: timeout (old exit timeout, new exit timeout)"),
        "{printed}"
    );
    assert!(printed.contains("DIFF kinds: file out"), "{printed}");
    assert!(
        printed.contains("old: a directory") && printed.contains("new: a text file"),
        "{printed}"
    );
    assert!(
        printed.contains("DIFF signal: exit (old exit signal 9, new exit signal 15)"),
        "{printed}"
    );
    // The links arrived as links, and the program read through the good one.
    let kept = std::fs::read_to_string(f.root.join("kept/kinds/old.files")).unwrap();
    assert_eq!(
        kept,
        "copied.txt\nout\nseed\nseed/broken\nseed/link\nseed/real.txt\n"
    );

    // A case name is a directory name under --keep, so it cannot leave it.
    f.write(
        "port/compare.toml",
        "old = [\"true\"]\nnew = [\"true\"]\n[[case]]\nname = \"../escape\"\n",
    );
    let out = f.port(&["compare", "--keep", "kept"]);
    assert!(!out.status.success());
    assert!(
        text(&out).contains("may only use letters, digits"),
        "{}",
        text(&out)
    );
}
