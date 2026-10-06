//! Workspace packages and how a bare specifier resolves to a source file.
//!
//! A TypeScript monorepo imports sibling packages by name (`@acme/kit/
//! runtime/client`). Their `package.json` `exports` point at compiled
//! `dist/*.js`; the port needs the `src/*.ts` that produced them.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde_json::Value;

#[derive(Debug, Clone)]
pub struct Package {
    pub name: String,
    /// Repository-relative directory, e.g. `packages/kit`.
    pub dir: String,
    main: Option<String>,
    exports: Option<Value>,
    /// Declared dependency name → version spec, across dependencies,
    /// devDependencies, and peerDependencies.
    deps: BTreeMap<String, String>,
}

/// How a bare specifier imported from a package resolves, per its
/// package.json (the way pnpm links it).
#[derive(Debug)]
pub enum Link<'a> {
    Workspace(&'a Package, String),
    /// npm package name (aliases such as `npm:@acme/sdk@0.19.0`
    /// resolve to the real name).
    Npm(String),
}

#[derive(Debug, Default)]
pub struct Packages {
    by_name: BTreeMap<String, Package>,
}

impl Packages {
    pub fn discover(root: &Path, packages_dir: &str) -> Result<Self> {
        let mut by_name = BTreeMap::new();
        let dir = root.join(packages_dir);
        let entries = std::fs::read_dir(&dir).with_context(|| format!("list {}", dir.display()))?;
        for entry in entries {
            let manifest = entry?.path().join("package.json");
            if !manifest.is_file() {
                continue;
            }
            let json: Value = serde_json::from_str(&std::fs::read_to_string(&manifest)?)
                .with_context(|| format!("parse {}", manifest.display()))?;
            let Some(name) = json.get("name").and_then(Value::as_str) else {
                continue;
            };
            let rel = manifest
                .parent()
                .and_then(|p| p.strip_prefix(root).ok())
                .map(|p| p.to_string_lossy().replace('\\', "/"))
                .unwrap_or_default();
            let mut deps = BTreeMap::new();
            for field in ["peerDependencies", "devDependencies", "dependencies"] {
                if let Some(map) = json.get(field).and_then(Value::as_object) {
                    for (dep, spec) in map {
                        if let Some(spec) = spec.as_str() {
                            deps.insert(dep.clone(), spec.to_string());
                        }
                    }
                }
            }
            by_name.insert(
                name.to_string(),
                Package {
                    name: name.to_string(),
                    dir: rel,
                    main: json.get("main").and_then(Value::as_str).map(str::to_string),
                    exports: json.get("exports").cloned(),
                    deps,
                },
            );
        }
        Ok(Self { by_name })
    }

    pub fn iter(&self) -> impl Iterator<Item = &Package> {
        self.by_name.values()
    }

    /// Package owning a repository-relative path, if any.
    pub fn owner(&self, rel: &str) -> Option<&Package> {
        self.by_name
            .values()
            .filter(|p| rel.starts_with(&format!("{}/", p.dir)))
            .max_by_key(|p| p.dir.len())
    }

    /// Resolve a bare specifier imported from `importer` (the package that
    /// owns the importing file). A name is a workspace link only when the
    /// importer declares it as `workspace:*`; anything else is npm, even if
    /// a workspace package happens to share the name.
    pub fn link(&self, importer: Option<&Package>, specifier: &str) -> Link<'_> {
        let (name, sub) = split_name(specifier);
        let spec = importer.and_then(|p| p.deps.get(name));
        match spec {
            Some(spec) if spec.starts_with("workspace:") => match self.by_name.get(name) {
                Some(package) => Link::Workspace(package, sub.to_string()),
                None => Link::Npm(name.to_string()),
            },
            Some(spec) if spec.starts_with("npm:") => {
                let target = &spec["npm:".len()..];
                // `@scope/pkg@1.2.3` or `pkg@1.2.3`
                let bare = match target.rfind('@') {
                    Some(0) | None => target,
                    Some(i) => &target[..i],
                };
                Link::Npm(bare.to_string())
            }
            _ => Link::Npm(name.to_string()),
        }
    }
}

/// `@scope/name/sub/path` → (`@scope/name`, `sub/path`); `name/sub` →
/// (`name`, `sub`).
pub fn split_name(specifier: &str) -> (&str, &str) {
    let mut cut = 0;
    let mut slashes = 0;
    let needed = if specifier.starts_with('@') { 2 } else { 1 };
    for (i, c) in specifier.char_indices() {
        if c == '/' {
            slashes += 1;
            if slashes == needed {
                cut = i;
                break;
            }
        }
    }
    if cut == 0 {
        (specifier, "")
    } else {
        (&specifier[..cut], &specifier[cut + 1..])
    }
}

impl Packages {
    /// Source file candidates (repository-relative, without checking the
    /// filesystem) for `subpath` of `package`.
    pub fn candidates(&self, package: &Package, subpath: &str) -> Vec<String> {
        let key = if subpath.is_empty() {
            ".".to_string()
        } else {
            format!("./{subpath}")
        };
        let mut targets = Vec::new();
        if let Some(exports) = &package.exports {
            targets.extend(export_targets(exports, &key));
        }
        if targets.is_empty() {
            if subpath.is_empty() {
                targets.push(
                    package
                        .main
                        .clone()
                        .unwrap_or_else(|| "src/index.ts".into()),
                );
            } else {
                targets.push(format!("src/{subpath}"));
            }
        }
        let mut out = Vec::new();
        for target in targets {
            for candidate in source_candidates(&target) {
                let full = format!("{}/{}", package.dir, candidate);
                if !out.contains(&full) {
                    out.push(full);
                }
            }
        }
        out
    }
}

/// Targets an `exports` map yields for `key`, preferring source (`bun`) over
/// compiled output. Handles single `*` wildcards.
fn export_targets(exports: &Value, key: &str) -> Vec<String> {
    let map = match exports {
        Value::String(s) if key == "." => return vec![s.clone()],
        Value::Object(map) => map,
        _ => return Vec::new(),
    };
    if let Some(entry) = map.get(key) {
        return condition_targets(entry, None);
    }
    for (pattern, entry) in map {
        if let Some((prefix, suffix)) = pattern.split_once('*')
            && key.starts_with(prefix)
            && key.ends_with(suffix)
            && key.len() >= prefix.len() + suffix.len()
        {
            let matched = &key[prefix.len()..key.len() - suffix.len()];
            return condition_targets(entry, Some(matched));
        }
    }
    Vec::new()
}

fn condition_targets(entry: &Value, wildcard: Option<&str>) -> Vec<String> {
    let fill = |s: &str| match wildcard {
        Some(w) => s.replace('*', w),
        None => s.to_string(),
    };
    match entry {
        Value::String(s) => vec![fill(s)],
        Value::Object(conditions) => ["bun", "source", "default", "import", "require", "types"]
            .iter()
            .filter_map(|c| conditions.get(*c).and_then(Value::as_str))
            .map(fill)
            .collect(),
        _ => Vec::new(),
    }
}

/// `./dist/runtime/cli-client.js` → `src/runtime/cli-client.ts`, plus the
/// `.tsx` and `index` variants. TypeScript sources come first; the JavaScript
/// file itself (outside `dist/`) and JavaScript variants follow, for projects
/// that port `.js`/`.mjs` too. The graph keeps only candidates whose
/// extension is configured (`extensions`).
fn source_candidates(target: &str) -> Vec<String> {
    let original = target.trim_start_matches("./").to_string();
    let mut t = original.clone();
    let compiled = t.starts_with("dist/");
    if let Some(rest) = t.strip_prefix("dist/") {
        t = format!("src/{rest}");
    }
    for ext in [".d.ts", ".js", ".mjs", ".cjs", ".jsx"] {
        if let Some(stem) = t.strip_suffix(ext) {
            t = stem.to_string();
            break;
        }
    }
    if t.ends_with(".ts") || t.ends_with(".tsx") {
        return vec![t];
    }
    let mut out = vec![
        format!("{t}.ts"),
        format!("{t}.tsx"),
        format!("{t}/index.ts"),
        format!("{t}/index.tsx"),
    ];
    if !compiled && t != original {
        out.push(original);
    }
    for ext in ["js", "mjs", "cjs", "jsx"] {
        for candidate in [format!("{t}.{ext}"), format!("{t}/index.{ext}")] {
            if !out.contains(&candidate) {
                out.push(candidate);
            }
        }
    }
    out
}

/// Candidates for a relative import from `from_dir` (repository-relative).
pub fn relative_candidates(from_dir: &str, specifier: &str) -> Vec<String> {
    let joined = normalize(&PathBuf::from(from_dir).join(specifier));
    source_candidates(&joined)
}

fn normalize(path: &Path) -> String {
    let mut parts: Vec<String> = Vec::new();
    for component in path.components() {
        match component {
            std::path::Component::ParentDir => {
                parts.pop();
            }
            std::path::Component::CurDir => {}
            other => parts.push(other.as_os_str().to_string_lossy().into_owned()),
        }
    }
    parts.join("/")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_compiled_targets_back_to_sources() {
        assert_eq!(
            source_candidates("./dist/runtime/cli-client.js")[..4],
            [
                "src/runtime/cli-client.ts",
                "src/runtime/cli-client.tsx",
                "src/runtime/cli-client/index.ts",
                "src/runtime/cli-client/index.tsx",
            ]
        );
        // Compiled output itself is never a source.
        assert!(
            !source_candidates("./dist/runtime/cli-client.js")
                .iter()
                .any(|c| c.starts_with("dist/"))
        );
        assert_eq!(source_candidates("./src/index.tsx"), vec!["src/index.tsx"]);
    }

    #[test]
    fn a_javascript_import_falls_back_to_the_javascript_file_after_typescript() {
        let got = source_candidates("bin/doctor.mjs");
        assert_eq!(got[0], "bin/doctor.ts");
        assert!(got.contains(&"bin/doctor.mjs".to_owned()), "{got:?}");
        let bare = source_candidates("lib/util");
        assert!(bare.contains(&"lib/util.js".to_owned()), "{bare:?}");
        assert!(bare.contains(&"lib/util/index.mjs".to_owned()), "{bare:?}");
    }

    #[test]
    fn resolves_wildcard_exports_preferring_source_conditions() {
        let exports: Value = serde_json::json!({
            ".": {"types": "./dist/index.d.ts", "default": "./dist/index.js"},
            "./runtime/*": {"default": "./dist/runtime/*.js"},
            "./tui-log": {"bun": "./src/tui-log.ts", "default": "./dist/tui-log.js"}
        });
        assert_eq!(
            export_targets(&exports, "./runtime/cli-client"),
            vec!["./dist/runtime/cli-client.js"]
        );
        assert_eq!(export_targets(&exports, "./tui-log")[0], "./src/tui-log.ts");
    }

    #[test]
    fn splits_package_names() {
        assert_eq!(
            split_name("@acme/kit/runtime/x"),
            ("@acme/kit", "runtime/x")
        );
        assert_eq!(split_name("@acme/sdk"), ("@acme/sdk", ""));
        assert_eq!(split_name("zod/v4"), ("zod", "v4"));
    }

    #[test]
    fn normalizes_relative_imports() {
        assert_eq!(
            relative_candidates("packages/app/src/storage", "../config.js")[0],
            "packages/app/src/config.ts"
        );
    }
}
