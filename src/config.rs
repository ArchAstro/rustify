//! `rustify.toml`: which TypeScript is in scope, where Rust lands, and where
//! the port keeps its checked-in state.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::components::Components;

/// File name `discover` looks for in the current directory and its parents.
pub const CONFIG_FILE: &str = "rustify.toml";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Directory holding `index.toml`, `mappings.toml`, and (optionally)
    /// `components.toml`, relative to the directory of `rustify.toml`.
    /// Defaults to that directory itself.
    #[serde(default)]
    pub state_dir: Option<String>,
    /// The upstream ref the port tracks: `done` stamps entries with its merge
    /// base, `drift` and `next --catch-up` compare against it, and `ratchet`
    /// defaults its `--base` to it.
    #[serde(default = "default_upstream")]
    pub upstream: String,
    /// Porting conventions document (repository-relative) the brief points
    /// agents at, if the project has one.
    #[serde(default)]
    pub conventions: Option<String>,
    /// Source directories whose non-test files seed the graph. Workspace
    /// packages they import are pulled in file by file.
    pub roots: Vec<String>,
    /// Directory whose `*/package.json` files name the workspace packages.
    pub packages_dir: String,
    /// Package whose modules become the crate root; every other workspace
    /// package becomes a top-level module named after it.
    pub primary_package: String,
    /// Crate directory ported modules are written into.
    pub rust_crate: String,
    /// Rust module path of the crate root, e.g. `my_app`.
    pub rust_crate_name: String,
    /// Globs (repository-relative) never added to the graph.
    #[serde(default)]
    pub exclude: Vec<String>,
    /// Source file extensions in the graph, without the dot. Add `js`,
    /// `mjs`, `cjs`, or `jsx` to port JavaScript files as well; they are
    /// parsed with the TypeScript (or, for `jsx`, TSX) grammar.
    #[serde(default = "default_extensions")]
    pub extensions: Vec<String>,
    /// A path is a test when any of these substrings occurs in it.
    pub test_markers: Vec<String>,
    /// Tests matching these substrings run against the binary, not a module
    /// (e2e contract tests); they are not assigned to port batches.
    #[serde(default)]
    pub binary_test_markers: Vec<String>,
    /// Workspace packages that are not ported but replaced wholesale by a
    /// Rust crate (a generated SDK, say). They are external leaves with a
    /// [[package]] mapping.
    #[serde(default)]
    pub external_packages: Vec<String>,
    pub batch: BatchConfig,
}

impl Config {
    /// The state directory, repository-relative and normalized (`.` for the
    /// root), for this configuration read from `config_path`.
    pub fn state_dir_for(&self, config_path: &str) -> String {
        let config_dir = Path::new(config_path)
            .parent()
            .unwrap_or_else(|| Path::new(""));
        let joined = match &self.state_dir {
            Some(state) => config_dir.join(state),
            None => config_dir.to_path_buf(),
        };
        normalize(&joined)
    }
}

/// Lexically normalize a relative path to `a/b` form: no `.`, empty, or
/// trailing components, `..` applied. Git looks paths up exactly, so
/// `port/./index.toml` or `port//index.toml` would be reported missing.
fn normalize(path: &Path) -> String {
    let mut parts: Vec<String> = Vec::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                parts.pop();
            }
            other => parts.push(other.as_os_str().to_string_lossy().into_owned()),
        }
    }
    if parts.is_empty() {
        ".".to_owned()
    } else {
        parts.join("/")
    }
}

fn default_extensions() -> Vec<String> {
    vec!["ts".to_owned(), "tsx".to_owned()]
}

fn default_upstream() -> String {
    "origin/main".to_owned()
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BatchConfig {
    pub max_files: usize,
    pub max_lines: usize,
}

/// Repository root plus parsed configuration.
pub struct Workspace {
    pub root: PathBuf,
    pub config: Config,
    /// `rustify.toml`, repository-relative.
    pub config_path: String,
    /// The state directory, repository-relative.
    pub state_dir: String,
    /// The UI component library, when `components.toml` exists.
    pub ui: Option<Components>,
}

impl Workspace {
    /// Load `config` (a path to `rustify.toml`) when given; otherwise walk up
    /// from `start` to the first directory containing `rustify.toml`. The
    /// repository root is the git work tree that contains it.
    pub fn discover(start: &Path, config: Option<&Path>) -> Result<Self> {
        let file = match config {
            Some(path) => {
                let path = if path.is_absolute() {
                    path.to_path_buf()
                } else {
                    start.join(path)
                };
                path.canonicalize()
                    .with_context(|| format!("resolve {}", path.display()))?
            }
            None => {
                let start = start
                    .canonicalize()
                    .with_context(|| format!("resolve {}", start.display()))?;
                match start
                    .ancestors()
                    .map(|dir| dir.join(CONFIG_FILE))
                    .find(|candidate| candidate.is_file())
                {
                    Some(found) => found,
                    None => bail!(
                        "no {CONFIG_FILE} in {} or its parents; pass --config <path>",
                        start.display()
                    ),
                }
            }
        };
        let dir = file.parent().context("rustify.toml has a parent")?;
        let root = git_root(dir).unwrap_or_else(|| dir.to_path_buf());
        let rel = file
            .strip_prefix(&root)
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .with_context(|| format!("{} is outside {}", file.display(), root.display()))?;
        Self::load(&root, &rel)
    }

    /// Load the configuration at `config_path` (repository-relative) for the
    /// tree rooted at `root`.
    pub fn load(root: &Path, config_path: &str) -> Result<Self> {
        let path = root.join(config_path);
        let text =
            std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
        let config: Config =
            toml::from_str(&text).with_context(|| format!("parse {}", path.display()))?;
        let state_dir = config.state_dir_for(config_path);
        let ui = Components::load(root.join(&state_dir).join("components.toml"))?;
        Ok(Self {
            root: root.to_path_buf(),
            config,
            config_path: config_path.to_owned(),
            state_dir,
            ui,
        })
    }

    /// The upstream ref the port tracks.
    pub fn upstream(&self) -> &str {
        &self.config.upstream
    }

    /// `src/` of the crate that owns `module` (a Rust path such as
    /// `my_app_ui::theme` or `my_app::extract`).
    pub fn crate_src(&self, module: &str) -> PathBuf {
        let first = module.split("::").next().unwrap_or(module);
        let dir = match &self.ui {
            Some(ui) if ui.library.crate_name == first => &ui.library.krate,
            _ => &self.config.rust_crate,
        };
        self.root.join(dir).join("src")
    }

    /// Every crate the port writes into, as (crate name, `src/` directory).
    pub fn crates(&self) -> Vec<(String, PathBuf)> {
        let mut out = vec![(
            self.config.rust_crate_name.clone(),
            self.root.join(&self.config.rust_crate).join("src"),
        )];
        if let Some(ui) = &self.ui {
            out.push((
                ui.library.crate_name.clone(),
                self.root.join(&ui.library.krate).join("src"),
            ));
        }
        out
    }

    pub fn port_file(&self, name: &str) -> PathBuf {
        self.root.join(&self.state_dir).join(name)
    }

    /// `name` in the state directory, repository-relative (for messages and
    /// `git show`).
    pub fn port_rel(&self, name: &str) -> String {
        if self.state_dir == "." {
            name.to_owned()
        } else {
            format!("{}/{name}", self.state_dir)
        }
    }

    /// Whether `rel` is a source file the graph includes, by extension
    /// (declaration files never are).
    pub fn is_source(&self, rel: &str) -> bool {
        if rel.ends_with(".d.ts") {
            return false;
        }
        let name = rel.rsplit('/').next().unwrap_or(rel);
        name.rsplit_once('.')
            .is_some_and(|(_, ext)| self.config.extensions.iter().any(|e| e == ext))
    }

    pub fn is_test(&self, rel: &str) -> bool {
        self.config.test_markers.iter().any(|m| rel.contains(m))
    }

    pub fn is_binary_test(&self, rel: &str) -> bool {
        self.config
            .binary_test_markers
            .iter()
            .any(|m| rel.contains(m))
    }
}

/// `git rev-parse --show-toplevel` from `dir`, if it is in a work tree.
fn git_root(dir: &Path) -> Option<PathBuf> {
    let out = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(dir)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let top = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim());
    top.canonicalize().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state_dir(config_path: &str, state: Option<&str>) -> String {
        let mut config: Config = toml::from_str(
            "roots = []\npackages_dir = \"p\"\nprimary_package = \"a\"\nrust_crate = \"r\"\nrust_crate_name = \"r\"\ntest_markers = []\n[batch]\nmax_files = 1\nmax_lines = 1\n",
        )
        .unwrap();
        config.state_dir = state.map(str::to_owned);
        config.state_dir_for(config_path)
    }

    #[test]
    fn state_dir_is_a_clean_repository_relative_path() {
        assert_eq!(state_dir("rustify.toml", None), ".");
        assert_eq!(state_dir("rustify.toml", Some("port/")), "port");
        assert_eq!(state_dir("tools/rustify.toml", None), "tools");
        assert_eq!(state_dir("tools/rustify.toml", Some(".")), "tools");
        assert_eq!(
            state_dir("tools/rustify.toml", Some("./port//")),
            "tools/port"
        );
        assert_eq!(
            state_dir("tools/a/rustify.toml", Some("../port")),
            "tools/port"
        );
    }
}
