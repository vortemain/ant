//! Plan cache for `ant build` (P3, decision D-029).
//!
//! Generating a plan can cost more than the build it describes when nothing
//! changed: the cargo adapter launches `cargo --version` and `cargo metadata`
//! through the rustup proxy (~0.5 s on Windows). Adapters that opt in through
//! [`BuildAdapter::plan_cache_probe`] get their plans reused while the key is
//! unchanged. The key is the SHA-256 of:
//!
//! - this cache's format version and Ant's version;
//! - the adapter name and its resolved options;
//! - the adapter's probe (toolchain resolution, global configuration, and
//!   environment outside the project; subprocess-free);
//! - the project's top-level listing (a new top-level file or directory can
//!   change what a plan declares), minus `.ant`, `.git`, the output
//!   directory, and the top-level directories of the plan's declared outputs
//!   (`target`), which builds themselves create;
//! - the content digest of every input the cached plan declares, read
//!   through the change-detection cache (D-027), so validating a hit reads
//!   only files whose stamps changed.
//!
//! The cache lives in the per-user cache directory (never in the project),
//! keeps the most recent [`KEEP`] plans per workspace (switching branches
//! back and forth keeps hitting), and any read or parse failure is a miss.
//! Trust model: the same as change detection and local reuse (D-027,
//! D-028); `ant build --fresh` bypasses it.

use build_action::InputKind;
use build_adapter::{AdapterOptions, BuildAdapter};
use build_graph::PlanFile;
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

const FORMAT: &str = "ant-plan-cache v1";
const KEEP: usize = 8;

#[derive(serde::Serialize, serde::Deserialize)]
struct Record {
    key: String,
    plan: PlanFile,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct Store {
    format: String,
    records: Vec<Record>,
}

pub struct PlanCache {
    file: PathBuf,
    root: PathBuf,
    header: String,
    out_dir: PathBuf,
}

impl PlanCache {
    /// A plan cache for `root` (canonical), or `None` when the adapter does
    /// not opt in.
    pub fn new(
        cache_dir: &Path,
        root: &Path,
        adapter: &dyn BuildAdapter,
        options: &AdapterOptions,
        out_dir: &Path,
    ) -> Option<Self> {
        let probe = adapter.plan_cache_probe(root)?;
        let mut header = format!(
            "{FORMAT}\nant {}\nadapter {}\noptions {options:?}\n",
            env!("CARGO_PKG_VERSION"),
            adapter.name()
        );
        for line in probe {
            header.push_str(&line);
            header.push('\n');
        }
        let mut listing: Vec<String> = fs::read_dir(root)
            .ok()?
            .filter_map(|entry| {
                let entry = entry.ok()?;
                let kind = entry.file_type().ok()?;
                Some(format!(
                    "top {} {}",
                    if kind.is_dir() { "d" } else { "f" },
                    entry.file_name().to_string_lossy()
                ))
            })
            .collect();
        listing.sort();
        for line in listing {
            header.push_str(&line);
            header.push('\n');
        }
        let name = build_digest::digest_bytes(root.to_string_lossy().as_bytes()).to_string();
        let file = cache_dir
            .join("plans")
            .join(format!("{}.json", name.trim_start_matches("sha256:")));
        Some(Self {
            file,
            root: root.to_path_buf(),
            header,
            out_dir: out_dir.to_path_buf(),
        })
    }

    /// The cached plan whose key still matches, if any.
    pub fn lookup(&self) -> Option<PlanFile> {
        let store = self.read()?;
        store
            .records
            .into_iter()
            .find(|record| self.key_for(&record.plan).as_deref() == Some(record.key.as_str()))
            .map(|record| record.plan)
    }

    /// Remember `plan` (best effort: failures only cost a regeneration).
    pub fn store(&self, plan: &PlanFile) {
        let Some(key) = self.key_for(plan) else {
            return;
        };
        let mut records = self.read().map(|store| store.records).unwrap_or_default();
        records.retain(|record| record.key != key);
        records.insert(
            0,
            Record {
                key,
                plan: plan.clone(),
            },
        );
        records.truncate(KEEP);
        let store = Store {
            format: FORMAT.to_owned(),
            records,
        };
        let Ok(text) = serde_json::to_vec(&store) else {
            return;
        };
        let Some(dir) = self.file.parent() else {
            return;
        };
        if fs::create_dir_all(dir).is_err() {
            return;
        }
        let temp = dir.join(format!(".plans-{}.tmp", std::process::id()));
        if fs::write(&temp, text).is_err() || fs::rename(&temp, &self.file).is_err() {
            let _ = fs::remove_file(&temp);
        }
    }

    fn read(&self) -> Option<Store> {
        let bytes = fs::read(&self.file).ok()?;
        let store: Store = serde_json::from_slice(&bytes).ok()?;
        (store.format == FORMAT).then_some(store)
    }

    /// The key `plan` would have in the current state of the machine and
    /// project; `None` when an input it declares cannot be digested.
    fn key_for(&self, plan: &PlanFile) -> Option<String> {
        let inputs: BTreeSet<(PathBuf, bool)> = plan
            .actions
            .iter()
            .flat_map(|planned| planned.action.inputs.iter())
            .map(|input| (input.path.clone(), input.kind == InputKind::Tree))
            .collect();
        let mut ignored: BTreeSet<std::ffi::OsString> =
            [".ant", ".git"].into_iter().map(Into::into).collect();
        let outputs = plan
            .actions
            .iter()
            .flat_map(|planned| planned.action.outputs.iter().map(|output| &output.path));
        for path in outputs.chain(std::iter::once(&self.out_dir)) {
            if let Some(std::path::Component::Normal(first)) = path.components().next() {
                ignored.insert(first.to_owned());
            }
        }
        let mut listing: Vec<String> = fs::read_dir(&self.root)
            .ok()?
            .filter_map(|entry| {
                let entry = entry.ok()?;
                if ignored.contains(&entry.file_name()) {
                    return None;
                }
                let kind = entry.file_type().ok()?;
                Some(format!(
                    "top {} {}
",
                    if kind.is_dir() { "d" } else { "f" },
                    entry.file_name().to_string_lossy()
                ))
            })
            .collect();
        listing.sort();
        let cache = build_digest::workspace_cache(&self.root);
        let mut material = self.header.clone();
        material.extend(listing);
        for (path, is_tree) in inputs {
            let absolute = self.root.join(&path);
            let digest = match (is_tree, &cache) {
                (true, Some(cache)) => build_digest::digest_tree_cached(&absolute, false, cache),
                (false, Some(cache)) => build_digest::digest_file_cached(&absolute, false, cache),
                (true, None) => build_digest::digest_tree(&absolute),
                (false, None) => build_digest::digest_file(&absolute),
            }
            .ok()?;
            material.push_str(&format!("input {} {digest}\n", path.display()));
        }
        Some(build_digest::digest_bytes(material.as_bytes()).to_string())
    }
}

/// Recreate the scratch directories a plan's actions expect (generation
/// creates them; a cached plan skips generation): every relative
/// `TMP`/`TEMP`/`TMPDIR` value, resolved against the action's directory.
pub fn ensure_scratch_dirs(root: &Path, plan: &PlanFile) {
    for planned in &plan.actions {
        let action = &planned.action;
        for (name, value) in &action.environment {
            if matches!(name.as_str(), "TMP" | "TEMP" | "TMPDIR") && Path::new(value).is_relative()
            {
                let _ = fs::create_dir_all(root.join(&action.working_directory).join(value));
            }
        }
    }
}
