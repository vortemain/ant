//! Property tests for action validation, using a seeded xorshift generator
//! (reproducible from the printed seed; no extra dependencies).

use build_action::{Action, CachePolicy, Input, InputKind, ResourceRequest, ValidationError};
use std::path::PathBuf;

const CASES: u64 = 500;

struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }

    fn below(&mut self, bound: u64) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x % bound
    }

    fn pick<'a>(&mut self, items: &[&'a str]) -> &'a str {
        items[self.below(items.len() as u64) as usize]
    }
}

fn base() -> Action {
    Action {
        schema_version: 1,
        id: "a".to_owned(),
        argv: vec!["tool".to_owned()],
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

/// Random path built from a mix of safe and unsafe components. Returns the
/// path and whether the documented rule (non-empty, relative, no `..`)
/// accepts it.
fn random_path(rng: &mut Rng) -> (PathBuf, bool) {
    let absolute = rng.below(6) == 0;
    let mut text = if absolute {
        if cfg!(windows) { r"C:\".to_owned() } else { "/".to_owned() }
    } else {
        String::new()
    };
    let parts = rng.below(4);
    let mut has_parent = false;
    for i in 0..parts {
        let part = rng.pick(&["src", "a.txt", "..", ".", "out", "nested"]);
        has_parent |= part == "..";
        if i > 0 {
            text.push('/');
        }
        text.push_str(part);
    }
    let empty = text.is_empty();
    (PathBuf::from(text), !empty && !absolute && !has_parent)
}

#[test]
fn input_paths_are_accepted_exactly_when_relative_and_contained() {
    for seed in 0..CASES {
        let mut rng = Rng::new(seed);
        let (path, expected_valid) = random_path(&mut rng);
        let mut action = base();
        action.inputs.push(Input { path: path.clone(), kind: InputKind::File });
        let result = action.validate();
        if expected_valid {
            assert_eq!(result, Ok(()), "seed {seed}: {path:?}");
        } else {
            assert_eq!(result, Err(ValidationError::InvalidPath(path.clone())), "seed {seed}: {path:?}");
        }
    }
}

#[test]
fn environment_keys_follow_the_documented_rules() {
    for seed in 0..CASES {
        let mut rng = Rng::new(seed);
        let key = format!(
            "{}{}{}",
            rng.pick(&["", "PATH", "OPT", " "]),
            rng.pick(&["", "=", "_X", "\0"]),
            rng.pick(&["", "1", "V"])
        );
        let mut action = base();
        action.environment.push((key.clone(), "value".to_owned()));
        let expected = if key.trim().is_empty() {
            Err(ValidationError::EmptyEnvironmentKey)
        } else if key.contains('=') || key.contains('\0') {
            Err(ValidationError::InvalidEnvironmentKey(key.clone()))
        } else {
            Ok(())
        };
        assert_eq!(action.validate(), expected, "seed {seed}: {key:?}");
    }
}

#[test]
fn resource_requests_are_valid_exactly_when_positive() {
    for seed in 0..CASES {
        let mut rng = Rng::new(seed);
        let cpu = rng.below(3) as u32;
        let memory_mb = match rng.below(3) {
            0 => None,
            1 => Some(0),
            _ => Some(1 + rng.below(1 << 20)),
        };
        let mut action = base();
        action.resources = Some(ResourceRequest { cpu, memory_mb });
        let expected = if cpu == 0 || memory_mb == Some(0) {
            Err(ValidationError::InvalidResources)
        } else {
            Ok(())
        };
        assert_eq!(action.validate(), expected, "seed {seed}: cpu={cpu} memory={memory_mb:?}");
        // Serialization roundtrip preserves the request exactly.
        let json = serde_json::to_string(&action).expect("serialize");
        let parsed: Action = serde_json::from_str(&json).expect("parse");
        assert_eq!(parsed, action, "seed {seed}");
    }
}
