# Benchmark plan (baseline)

**Status:** Method defined. First synthetic results: [2026-11-13 cold vs. warm cache under network isolation](2026-11-13-cache-cold-warm.md) (single machine; characterizes overheads; no speedup claims); [2026-11-13 shared remote cache vs. plain cargo on Ant itself](2026-11-13-remote-cache.md) (first real-project numbers, Linux); [2026-11-14 Ant vs. plain cargo on Ant itself, local build states](2026-11-14-local-rust.md) (single machine; no-change 3.6×, switchback 103×, real-edit +11%, cold +3%).

## Workload selection

Choose pinned representative public or owner-provided projects only after initial adapter/user priorities are known. For each, record repository revision, licenses, source size, task graph, toolchain and SDK versions, OS/architecture, dependencies, build flags, and artifact outputs. Keep a small synthetic action workload for graph/scheduler overhead separate from real compilation.

## Environment record

Record CPU model/core count, RAM, storage type/free space, OS/kernel, power/thermal mode where known, network bandwidth/latency for remote tests, Rust/compiler versions, configuration, worker topology, and cache state. Repeat on constrained and common developer-machine profiles where feasible.

## Run protocol

- Separate cold dependency/cache state, warm cache, no-change, and controlled small-change runs.
- Pin all inputs and ensure competitor baselines use identical source, toolchains, flags, and hardware.
- Perform warmups separately; repeat runs sufficiently to report median, spread, and outliers. Preserve raw observations and scripts.
- Include failures and incomplete runs in reliability reporting; do not cherry-pick successful timings.
- For remote/hybrid, include upload, queue, worker startup, execution, and artifact download in end-to-end wall time.
- Avoid concurrent workloads that contaminate resource measurements unless concurrency itself is under test.

## Metrics

Wall time; CPU time/utilization; peak resident memory; disk read/write volume and latency; network transfer; process/startup overhead; queue delay; cache hit/miss; artifact size; success/failure rate; cost per successful build when costs are known. Distinguish direct measurements from estimates.

## Deliverables and acceptance

Store machine-readable raw results, environment manifest, exact commands, analysis script, and concise report. Report cold and warm separately with variance. A speedup claim requires repeated controlled comparisons and a stated method. No baseline numbers exist yet.
