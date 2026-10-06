//! `components.toml` (optional): the UI component library crate the
//! project's TS components (Ink screens, say) are ported onto. It routes
//! library-owned TS modules into the library crate, names the widgets a port
//! should reuse, maps UI framework APIs (Ink, React) to their library
//! equivalents, and points at the golden frames that parity tests compare
//! against.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Components {
    pub library: Library,
    /// TS modules whose port lives in the library crate.
    #[serde(default, rename = "module")]
    pub modules: Vec<LibraryModule>,
    #[serde(default, rename = "widget")]
    pub widgets: Vec<Widget>,
    /// UI framework APIs (Ink / React) → library equivalent.
    #[serde(default, rename = "ink")]
    pub ink: Vec<InkRule>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Library {
    /// Crate directory, repository-relative.
    #[serde(rename = "crate")]
    pub krate: String,
    /// Rust path of the crate root, e.g. `my_app_ui`.
    pub crate_name: String,
    /// Golden frames directory, repository-relative.
    pub goldens: String,
    /// The TS test that renders fixtures into `goldens`.
    pub golden_test: String,
    /// Command that regenerates the golden frames, run from the golden
    /// test's package directory. `{test}` is replaced with the test path
    /// relative to that directory.
    #[serde(default = "default_golden_command")]
    pub golden_command: String,
    /// Import specifiers whose names the `[[ink]]` rules map.
    #[serde(default = "default_ui_packages")]
    pub ui_packages: Vec<String>,
}

fn default_golden_command() -> String {
    "npx vitest run {test} -u".to_owned()
}

fn default_ui_packages() -> Vec<String> {
    vec!["ink".to_owned(), "react".to_owned()]
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LibraryModule {
    pub ts: String,
    /// File relative to the library crate's `src/`.
    pub rust: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WidgetStatus {
    /// Exists in the library with a parity test.
    Available,
    /// Needed by screens but not built yet.
    Planned,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Widget {
    pub name: String,
    /// Full Rust path, e.g. `my_app_ui::widgets::selector::SelectorList`.
    pub rust: String,
    /// File relative to the library crate's `src/`.
    pub file: String,
    /// TS files this widget replaces in whole or in part.
    #[serde(default)]
    pub from: Vec<String>,
    /// Golden file under `library.goldens` its parity test reads.
    #[serde(default)]
    pub golden: Option<String>,
    pub status: WidgetStatus,
    pub summary: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InkRule {
    /// Names as imported from a `ui_packages` specifier (`Box`, `useInput`, …).
    pub names: Vec<String>,
    pub rust: String,
    #[serde(default)]
    pub hazards: Vec<String>,
}

impl Components {
    /// `None` when the repository has no `components.toml`.
    pub fn load(path: PathBuf) -> Result<Option<Self>> {
        if !path.exists() {
            return Ok(None);
        }
        let text =
            std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
        let components: Components =
            toml::from_str(&text).with_context(|| format!("parse {}", path.display()))?;
        let mut seen = std::collections::BTreeSet::new();
        for m in &components.modules {
            if !seen.insert(&m.ts) {
                bail!("{} appears twice in components.toml", m.ts);
            }
        }
        Ok(Some(components))
    }

    pub fn module_for(&self, ts: &str) -> Option<&LibraryModule> {
        self.modules.iter().find(|m| m.ts == ts)
    }

    pub fn ink_rule(&self, name: &str) -> Option<&InkRule> {
        self.ink.iter().find(|r| r.names.iter().any(|n| n == name))
    }

    /// Widgets that replace `ts` (in whole or part).
    pub fn widgets_from<'a>(&'a self, ts: &'a str) -> impl Iterator<Item = &'a Widget> + 'a {
        self.widgets
            .iter()
            .filter(move |w| w.from.iter().any(|f| f == ts))
    }
}
