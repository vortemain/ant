//! Dependency graph for planned build actions.
//!
//! Ordering is deterministic by action ID among currently-ready nodes.
//! The graph stores dependencies by action ID so serialized or adapter-facing
//! models do not depend on transient vector positions.
//!
//! This crate also owns the canonical versioned plan-file envelope
//! (`PlanFile`): the serialized form `ant run --plan` loads and `ant adapt`
//! writes. One contract, one home.

use build_action::{Action, ValidationError};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::fmt;

/// Schema version of the plan-file envelope. History:
/// - 1: `{"schema_version": 1, "actions": [{"action": ..., "dependencies": [...]}]}`.
pub const PLAN_SCHEMA_VERSION: u32 = 1;

/// The versioned plan-file envelope: a list of actions with their declared
/// dependency edges. Serializing it produces exactly the document
/// `ant run --plan` accepts.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanFile {
    pub schema_version: u32,
    pub actions: Vec<PlanAction>,
}

/// One plan entry: an action plus its dependency edge list.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanAction {
    pub action: Action,
    #[serde(default)]
    pub dependencies: Vec<String>,
}

impl PlanFile {
    /// A fresh, versioned, empty plan file.
    pub fn new() -> Self {
        Self {
            schema_version: PLAN_SCHEMA_VERSION,
            actions: Vec::new(),
        }
    }

    /// Build a validated graph from this plan. Structural action validation,
    /// duplicate IDs, unknown/self dependencies, and cycles are all checked
    /// here; `ant run` performs the same conversion before executing.
    pub fn to_graph(&self) -> Result<BuildGraph, GraphError> {
        let mut graph = BuildGraph::new();
        for entry in &self.actions {
            graph.add_action(entry.action.clone(), entry.dependencies.iter().cloned())?;
        }
        graph.execution_order()?;
        Ok(graph)
    }
}

impl Default for PlanFile {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Debug)]
pub struct BuildNode {
    pub action: Action,
    /// IDs that must complete successfully before this action is ready.
    pub dependencies: Vec<String>,
}

#[derive(Clone, Debug, Default)]
pub struct BuildGraph {
    nodes: Vec<BuildNode>,
    index: HashMap<String, usize>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GraphError {
    InvalidAction { id: String, source: ValidationError },
    DuplicateActionId(String),
    UnknownDependency { action: String, dependency: String },
    SelfDependency(String),
    Cycle(Vec<String>),
}

impl fmt::Display for GraphError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidAction { id, source } => write!(f, "action '{id}' is invalid: {source:?}"),
            Self::DuplicateActionId(id) => write!(f, "duplicate action ID '{id}'"),
            Self::UnknownDependency { action, dependency } => {
                write!(
                    f,
                    "action '{action}' depends on unknown action '{dependency}'"
                )
            }
            Self::SelfDependency(id) => write!(f, "action '{id}' depends on itself"),
            Self::Cycle(ids) => write!(f, "dependency cycle includes: {}", ids.join(", ")),
        }
    }
}

impl std::error::Error for GraphError {}

impl BuildGraph {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add_action(
        &mut self,
        action: Action,
        dependencies: impl IntoIterator<Item = String>,
    ) -> Result<(), GraphError> {
        action
            .validate()
            .map_err(|source| GraphError::InvalidAction {
                id: action.id.clone(),
                source,
            })?;
        if self.index.contains_key(&action.id) {
            return Err(GraphError::DuplicateActionId(action.id));
        }
        let index = self.nodes.len();
        self.index.insert(action.id.clone(), index);
        self.nodes.push(BuildNode {
            action,
            dependencies: dependencies.into_iter().collect(),
        });
        Ok(())
    }

    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// Return action IDs in deterministic topological order.
    pub fn execution_order(&self) -> Result<Vec<&str>, GraphError> {
        let mut indegree = vec![0usize; self.nodes.len()];
        let mut dependents: Vec<Vec<usize>> = vec![Vec::new(); self.nodes.len()];
        for (node_index, node) in self.nodes.iter().enumerate() {
            let mut unique = HashSet::new();
            for dependency in &node.dependencies {
                if dependency == &node.action.id {
                    return Err(GraphError::SelfDependency(node.action.id.clone()));
                }
                if !unique.insert(dependency) {
                    continue;
                }
                let Some(&dependency_index) = self.index.get(dependency) else {
                    return Err(GraphError::UnknownDependency {
                        action: node.action.id.clone(),
                        dependency: dependency.clone(),
                    });
                };
                indegree[node_index] += 1;
                dependents[dependency_index].push(node_index);
            }
        }

        let mut ready = BTreeSet::new();
        for (index, degree) in indegree.iter().enumerate() {
            if *degree == 0 {
                ready.insert((self.nodes[index].action.id.as_str(), index));
            }
        }
        let mut order = Vec::with_capacity(self.nodes.len());
        while let Some((id, index)) = ready.pop_first() {
            order.push(id);
            for &dependent in &dependents[index] {
                indegree[dependent] -= 1;
                if indegree[dependent] == 0 {
                    ready.insert((self.nodes[dependent].action.id.as_str(), dependent));
                }
            }
        }
        if order.len() != self.nodes.len() {
            let mut cycle_ids: Vec<_> = self
                .nodes
                .iter()
                .enumerate()
                .filter(|(index, _)| indegree[*index] > 0)
                .map(|(_, node)| node.action.id.clone())
                .collect();
            cycle_ids.sort();
            return Err(GraphError::Cycle(cycle_ids));
        }
        Ok(order)
    }

    /// Resolve IDs from `execution_order` to action references for an executor.
    pub fn ordered_actions(&self) -> Result<Vec<&Action>, GraphError> {
        self.execution_order()?
            .into_iter()
            .map(|id| {
                self.index
                    .get(id)
                    .map(|&index| &self.nodes[index].action)
                    .ok_or_else(|| GraphError::UnknownDependency {
                        action: "<graph>".to_owned(),
                        dependency: id.to_owned(),
                    })
            })
            .collect()
    }

    /// Return complete graph nodes in execution order for inspection and schedulers.
    pub fn ordered_nodes(&self) -> Result<Vec<&BuildNode>, GraphError> {
        self.execution_order()?
            .into_iter()
            .map(|id| {
                self.index
                    .get(id)
                    .map(|&index| &self.nodes[index])
                    .ok_or_else(|| GraphError::UnknownDependency {
                        action: "<graph>".to_owned(),
                        dependency: id.to_owned(),
                    })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use build_action::CachePolicy;
    use std::path::PathBuf;

    fn action(id: &str) -> Action {
        Action {
            schema_version: 1,
            id: id.to_owned(),
            argv: vec!["echo".to_owned(), id.to_owned()],
            working_directory: PathBuf::from("."),
            inputs: Vec::new(),
            outputs: Vec::new(),
            environment: Vec::new(),
            cache_policy: CachePolicy::Never,
            toolchain: None,
            timeout_ms: None,
            resources: None,
        }
    }

    fn graph_with(entries: &[(&str, &[&str])]) -> Result<BuildGraph, GraphError> {
        let mut graph = BuildGraph::new();
        for (id, dependencies) in entries {
            graph.add_action(action(id), dependencies.iter().map(|d| d.to_string()))?;
        }
        Ok(graph)
    }

    #[test]
    fn orders_independently_of_insertion_order() {
        let first =
            graph_with(&[("a", &[]), ("b", &["a"]), ("c", &["a"]), ("d", &["b", "c"])]).unwrap();
        let second =
            graph_with(&[("d", &["b", "c"]), ("c", &["a"]), ("b", &["a"]), ("a", &[])]).unwrap();
        let left = first.execution_order().unwrap();
        let right = second.execution_order().unwrap();
        assert_eq!(left, vec!["a", "b", "c", "d"]);
        assert_eq!(left, right, "ordering must not depend on insertion order");
    }

    #[test]
    fn ties_break_by_action_id() {
        let graph = graph_with(&[("z", &[]), ("m", &[]), ("a", &[])]).unwrap();
        assert_eq!(graph.execution_order().unwrap(), vec!["a", "m", "z"]);
    }

    #[test]
    fn rejects_duplicate_action_id() {
        let mut graph = BuildGraph::new();
        graph
            .add_action(action("x"), Vec::<String>::new())
            .expect("first insert");
        let error = graph
            .add_action(action("x"), Vec::<String>::new())
            .unwrap_err();
        assert_eq!(error, GraphError::DuplicateActionId("x".to_owned()));
    }

    #[test]
    fn rejects_unknown_dependency() {
        let mut graph = BuildGraph::new();
        graph
            .add_action(action("a"), vec!["ghost".to_owned()])
            .expect("insert");
        assert_eq!(
            graph.execution_order(),
            Err(GraphError::UnknownDependency {
                action: "a".to_owned(),
                dependency: "ghost".to_owned()
            })
        );
    }

    #[test]
    fn rejects_self_dependency() {
        let mut graph = BuildGraph::new();
        graph
            .add_action(action("a"), vec!["a".to_owned()])
            .expect("insert");
        assert_eq!(
            graph.execution_order(),
            Err(GraphError::SelfDependency("a".to_owned()))
        );
    }

    #[test]
    fn detects_cycle_and_names_members() {
        let mut graph = BuildGraph::new();
        graph
            .add_action(action("a"), vec!["c".to_owned()])
            .expect("insert a");
        graph
            .add_action(action("b"), vec!["a".to_owned()])
            .expect("insert b");
        graph
            .add_action(action("c"), vec!["b".to_owned()])
            .expect("insert c");
        match graph.execution_order() {
            Err(GraphError::Cycle(ids)) => assert_eq!(ids, vec!["a", "b", "c"]),
            other => panic!("expected cycle error, got {other:?}"),
        }
    }

    #[test]
    fn duplicate_dependency_edges_count_once() {
        let graph = graph_with(&[("a", &[]), ("b", &["a", "a"])]).unwrap();
        assert_eq!(graph.execution_order().unwrap(), vec!["a", "b"]);
    }

    #[test]
    fn ordered_actions_match_execution_order() {
        let graph = graph_with(&[("a", &[]), ("b", &["a"])]).unwrap();
        let actions = graph.ordered_actions().unwrap();
        assert_eq!(
            actions.iter().map(|a| a.id.as_str()).collect::<Vec<_>>(),
            vec!["a", "b"]
        );
    }

    #[test]
    fn ordered_nodes_expose_dependencies() {
        let graph = graph_with(&[("a", &[]), ("b", &["a"])]).unwrap();
        let nodes = graph.ordered_nodes().unwrap();
        assert_eq!(nodes.len(), 2);
        assert!(nodes[0].dependencies.is_empty());
        assert_eq!(nodes[1].dependencies, vec!["a".to_owned()]);
    }

    #[test]
    fn empty_graph_has_empty_order() {
        let graph = BuildGraph::new();
        assert!(graph.is_empty());
        assert_eq!(graph.execution_order().unwrap(), Vec::<&str>::new());
    }

    #[test]
    fn plan_file_serializes_with_version_and_entries() {
        let mut file = PlanFile::new();
        file.actions.push(PlanAction {
            action: action("a"),
            dependencies: Vec::new(),
        });
        file.actions.push(PlanAction {
            action: action("b"),
            dependencies: vec!["a".to_owned()],
        });
        let json = serde_json::to_value(&file).unwrap();
        assert_eq!(json["schema_version"], 1);
        assert_eq!(json["actions"][0]["action"]["id"], "a");
        assert_eq!(json["actions"][1]["dependencies"][0], "a");
        // And it parses back losslessly.
        let parsed: PlanFile = serde_json::from_value(json).unwrap();
        assert_eq!(parsed, file);
    }

    #[test]
    fn plan_file_to_graph_validates_and_orders() {
        let mut file = PlanFile::new();
        file.actions.push(PlanAction {
            action: action("b"),
            dependencies: vec!["a".to_owned()],
        });
        file.actions.push(PlanAction {
            action: action("a"),
            dependencies: Vec::new(),
        });
        let graph = file.to_graph().unwrap();
        assert_eq!(graph.execution_order().unwrap(), vec!["a", "b"]);
    }

    #[test]
    fn plan_file_to_graph_rejects_cycles_and_bad_actions() {
        let mut file = PlanFile::new();
        file.actions.push(PlanAction {
            action: action("a"),
            dependencies: vec!["a".to_owned()],
        });
        assert!(matches!(
            file.to_graph(),
            Err(GraphError::SelfDependency(_))
        ));
        let mut file = PlanFile::new();
        let mut broken = action("x");
        broken.schema_version = 2;
        file.actions.push(PlanAction {
            action: broken,
            dependencies: Vec::new(),
        });
        assert!(matches!(
            file.to_graph(),
            Err(GraphError::InvalidAction { .. })
        ));
    }

    #[test]
    fn plan_file_rejects_unknown_fields_and_missing_version() {
        let parsed: Result<PlanFile, _> =
            serde_json::from_str(r#"{"schema_version":1,"actions":[],"extra":1}"#);
        assert!(parsed.is_err(), "unknown fields must be rejected");
        let parsed: Result<PlanFile, _> = serde_json::from_str(r#"{"actions":[]}"#);
        assert!(parsed.is_err(), "schema_version is required");
    }
}
