//! Property tests for graph validation and ordering.
//!
//! Generated with a small seeded xorshift generator instead of a property
//! testing dependency: every case is reproducible from its printed seed, and
//! the suite adds no crates to the workspace. Each property runs over a few
//! hundred random graphs per test invocation.

use build_action::{Action, CachePolicy};
use build_graph::{BuildGraph, GraphError, PlanAction, PlanFile};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

const CASES: u64 = 300;

struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        // Avoid the all-zero state, which xorshift cannot leave.
        Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }

    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    fn below(&mut self, bound: u64) -> u64 {
        self.next() % bound
    }

    fn shuffle<T>(&mut self, items: &mut [T]) {
        for i in (1..items.len()).rev() {
            let j = self.below(i as u64 + 1) as usize;
            items.swap(i, j);
        }
    }
}

fn action(id: &str) -> Action {
    Action {
        schema_version: 1,
        id: id.to_owned(),
        argv: vec!["tool".to_owned(), id.to_owned()],
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

/// A random DAG: node `i` may depend only on nodes `< i` in a hidden
/// generation order, which guarantees acyclicity. IDs are random so the
/// generation order is unrelated to ID order. Duplicate edges are included
/// on purpose (the graph must tolerate them).
fn random_dag(rng: &mut Rng) -> Vec<(String, Vec<String>)> {
    let count = 1 + rng.below(24) as usize;
    let mut ids: Vec<String> = (0..count).map(|i| format!("n{:03}-{}", rng.below(1000), i)).collect();
    rng.shuffle(&mut ids);
    let mut nodes = Vec::with_capacity(count);
    for i in 0..count {
        let mut dependencies = Vec::new();
        if i > 0 {
            for _ in 0..rng.below(4) {
                dependencies.push(ids[rng.below(i as u64) as usize].clone());
            }
        }
        nodes.push((ids[i].clone(), dependencies));
    }
    nodes
}

fn build(nodes: &[(String, Vec<String>)]) -> Result<BuildGraph, GraphError> {
    let mut graph = BuildGraph::new();
    for (id, dependencies) in nodes {
        graph.add_action(action(id), dependencies.clone())?;
    }
    Ok(graph)
}

fn order_of(graph: &BuildGraph) -> Vec<String> {
    graph
        .execution_order()
        .expect("acyclic graph orders")
        .into_iter()
        .map(str::to_owned)
        .collect()
}

#[test]
fn order_is_a_topological_permutation() {
    for seed in 0..CASES {
        let mut rng = Rng::new(seed);
        let nodes = random_dag(&mut rng);
        let order = order_of(&build(&nodes).expect("valid DAG builds"));
        assert_eq!(order.len(), nodes.len(), "seed {seed}: every action appears");
        let position: HashMap<&str, usize> =
            order.iter().enumerate().map(|(i, id)| (id.as_str(), i)).collect();
        assert_eq!(position.len(), nodes.len(), "seed {seed}: no action appears twice");
        for (id, dependencies) in &nodes {
            for dependency in dependencies {
                assert!(
                    position[dependency.as_str()] < position[id.as_str()],
                    "seed {seed}: {dependency} must precede {id}"
                );
            }
        }
    }
}

#[test]
fn order_is_independent_of_insertion_order() {
    for seed in 0..CASES {
        let mut rng = Rng::new(seed);
        let nodes = random_dag(&mut rng);
        let expected = order_of(&build(&nodes).expect("builds"));
        let mut shuffled = nodes.clone();
        rng.shuffle(&mut shuffled);
        assert_eq!(order_of(&build(&shuffled).expect("builds")), expected, "seed {seed}");
    }
}

#[test]
fn order_starts_ready_actions_smallest_id_first() {
    // At every step the emitted action is the smallest ID among the actions
    // whose dependencies have all been emitted (the documented tie-break).
    for seed in 0..CASES {
        let mut rng = Rng::new(seed);
        let nodes = random_dag(&mut rng);
        let order = order_of(&build(&nodes).expect("builds"));
        let mut done: HashSet<&str> = HashSet::new();
        for id in &order {
            let smallest_ready = nodes
                .iter()
                .filter(|(candidate, deps)| {
                    !done.contains(candidate.as_str()) && deps.iter().all(|d| done.contains(d.as_str()))
                })
                .map(|(candidate, _)| candidate.as_str())
                .min()
                .expect("something is ready");
            assert_eq!(id, smallest_ready, "seed {seed}");
            done.insert(id);
        }
    }
}

#[test]
fn injected_cycles_are_rejected_and_reported() {
    for seed in 0..CASES {
        let mut rng = Rng::new(seed);
        let mut nodes = random_dag(&mut rng);
        if nodes.len() < 2 {
            continue;
        }
        // Close a cycle: pick a dependency chain a <- ... <- b (b reachable
        // from a by dependents) by taking a node with a dependency and making
        // that dependency depend back on it.
        let Some(index) = (0..nodes.len()).find(|&i| !nodes[i].1.is_empty()) else {
            continue;
        };
        let dependent = nodes[index].0.clone();
        let dependency = nodes[index].1[0].clone();
        let target = nodes.iter().position(|(id, _)| *id == dependency).expect("known");
        nodes[target].1.push(dependent.clone());
        match build(&nodes).expect("adding actions does not check cycles").execution_order() {
            Err(GraphError::Cycle(ids)) => {
                assert!(ids.contains(&dependent) && ids.contains(&dependency), "seed {seed}: {ids:?}");
                let mut sorted = ids.clone();
                sorted.sort();
                assert_eq!(ids, sorted, "seed {seed}: cycle report is sorted");
            }
            other => panic!("seed {seed}: expected a cycle, got {other:?}"),
        }
    }
}

#[test]
fn unknown_and_self_dependencies_are_rejected() {
    for seed in 0..CASES {
        let mut rng = Rng::new(seed);
        let mut nodes = random_dag(&mut rng);
        let victim = rng.below(nodes.len() as u64) as usize;
        if seed % 2 == 0 {
            nodes[victim].1.push("does-not-exist".to_owned());
            match build(&nodes).expect("builds").execution_order() {
                Err(GraphError::UnknownDependency { dependency, .. }) => {
                    assert_eq!(dependency, "does-not-exist", "seed {seed}")
                }
                other => panic!("seed {seed}: expected unknown dependency, got {other:?}"),
            }
        } else {
            let id = nodes[victim].0.clone();
            nodes[victim].1.push(id.clone());
            assert_eq!(
                build(&nodes).expect("builds").execution_order(),
                Err(GraphError::SelfDependency(id)),
                "seed {seed}"
            );
        }
    }
}

#[test]
fn duplicate_ids_are_rejected_at_insertion() {
    for seed in 0..CASES {
        let mut rng = Rng::new(seed);
        let mut nodes = random_dag(&mut rng);
        let duplicate = nodes[rng.below(nodes.len() as u64) as usize].0.clone();
        nodes.push((duplicate.clone(), Vec::new()));
        assert_eq!(build(&nodes).err(), Some(GraphError::DuplicateActionId(duplicate)), "seed {seed}");
    }
}

#[test]
fn plan_file_json_roundtrip_preserves_order() {
    for seed in 0..CASES {
        let mut rng = Rng::new(seed);
        let nodes = random_dag(&mut rng);
        let mut plan = PlanFile::new();
        for (id, dependencies) in &nodes {
            plan.actions.push(PlanAction { action: action(id), dependencies: dependencies.clone() });
        }
        let json = serde_json::to_string(&plan).expect("serialize");
        let parsed: PlanFile = serde_json::from_str(&json).expect("parse");
        assert_eq!(parsed, plan, "seed {seed}");
        let from_plan = parsed.to_graph().expect("valid plan converts");
        assert_eq!(order_of(&from_plan), order_of(&build(&nodes).expect("builds")), "seed {seed}");
    }
}
