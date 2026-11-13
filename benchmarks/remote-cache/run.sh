#!/usr/bin/env bash
# Remote shared-cache benchmark on Linux (P2.4). Subject: this repository at
# HEAD (Ant itself), dependencies vendored with `cargo vendor` so every
# source is a declared input (D-020). Compares plain `cargo build --release`
# with `ant remote run` through a sandboxed Linux worker and the shared
# remote cache (D-019). Every observation is written to results.csv; nothing
# is discarded. Requires: cargo, bubblewrap + unprivileged user namespaces,
# a release `ant` binary.
set -euo pipefail

REPO=${REPO:-$(cd "$(dirname "$0")/../.." && pwd)}
ANT=${ANT:-$REPO/target/release/ant}
OUT=${OUT:-$REPO/benchmarks/remote-cache/out}
REPS=${REPS:-3}
WORK=$(mktemp -d)
mkdir -p "$OUT"
CSV="$OUT/results.csv"
echo "scenario,rep,seconds,executed,reused,notes" > "$CSV"

now() { date +%s.%N; }
elapsed() { awk -v s="$1" -v e="$2" 'BEGIN { printf "%.2f", e - s }'; }
record() { echo "$1,$2,$3,$4,$5,$6" >> "$CSV"; echo "  $1 rep $2: $3 s (executed=$4 reused=$5) $6"; }

# --- environment manifest -------------------------------------------------
{
  echo "date_utc=$(date -u +%Y-%m-%dT%H:%M:%SZ)"
  echo "commit=$(git -C "$REPO" rev-parse --short HEAD)"
  echo "cpu=$(grep -m1 'model name' /proc/cpuinfo | cut -d: -f2- | xargs)"
  echo "cores=$(nproc)"
  echo "ram_gib=$(awk '/MemTotal/ { printf "%.1f", $2/1048576 }' /proc/meminfo)"
  echo "os=$(. /etc/os-release && echo "$PRETTY_NAME") kernel $(uname -r)"
  echo "rustc=$(rustc --version)"
  echo "bwrap=$(bwrap --version)"
  echo "reps=$REPS"
} > "$OUT/environment.txt"
cat "$OUT/environment.txt"

# --- subject: a clean, vendored copy of this repository -------------------
SUBJECT="$WORK/subject"
mkdir -p "$SUBJECT"
git -C "$REPO" archive --format=tar HEAD | tar -x -C "$SUBJECT"
(cd "$SUBJECT" && cargo vendor --locked vendor > /dev/null)
mkdir -p "$SUBJECT/.cargo"
cat > "$SUBJECT/.cargo/config.toml" <<'EOF'
[source.crates-io]
replace-with = "vendored-sources"

[source.vendored-sources]
directory = "vendor"
EOF
echo "subject: $(du -sh "$SUBJECT" | cut -f1), $(find "$SUBJECT" -type f | wc -l) files"
checkout() { cp -r "$SUBJECT" "$1"; }

# --- plain cargo baselines -------------------------------------------------
checkout "$WORK/native-a"
s=$(now); (cd "$WORK/native-a" && cargo build --release --offline --quiet); e=$(now)
record native-cold 1 "$(elapsed "$s" "$e")" - - "fresh target dir"
for rep in $(seq 1 "$REPS"); do
  s=$(now); (cd "$WORK/native-a" && cargo build --release --offline --quiet); e=$(now)
  record native-no-change "$rep" "$(elapsed "$s" "$e")" - - "same checkout"
done
checkout "$WORK/native-b"
s=$(now); (cd "$WORK/native-b" && cargo build --release --offline --quiet); e=$(now)
record native-fresh-checkout 1 "$(elapsed "$s" "$e")" - - "second checkout; no cross-checkout reuse"

# --- coordinator + one sandboxed worker on loopback ------------------------
head -c 48 /dev/urandom | base64 | tr -d '/+=' > "$WORK/client.token"
head -c 48 /dev/urandom | base64 | tr -d '/+=' > "$WORK/worker.token"
"$ANT" coordinator --listen 127.0.0.1:0 --data-dir "$WORK/coord" \
  --client-token-file "$WORK/client.token" --worker-token-file "$WORK/worker.token" > "$WORK/coord.out" &
COORD=$!
for _ in $(seq 1 50); do grep -q listening= "$WORK/coord.out" 2>/dev/null && break; sleep 0.1; done
URL="http://$(grep -o 'listening=[^ ]*' "$WORK/coord.out" | cut -d= -f2)"
"$ANT" worker --coordinator "$URL" --token-file "$WORK/worker.token" --work-dir "$WORK/work" \
  --id bench-worker --sandbox > "$WORK/worker.out" 2>&1 &
WORKER=$!
# Keep the worker's log (cargo's own output lands there) whatever happens.
trap 'cp "$WORK/worker.out" "$OUT/worker.log" 2>/dev/null; kill $WORKER $COORD 2>/dev/null || true' EXIT

remote() {
  local ws=$1 log=$2
  "$ANT" remote run --coordinator "$URL" --token-file "$WORK/client.token" \
    --workspace "$ws" --plan "$WORK/plan.json" > "$log" 2>&1
}
counts() { echo "$(grep -c 'status=succeeded' "$1" || true) $(grep -c 'status=reused' "$1" || true)"; }

checkout "$WORK/ant-a"
s=$(now)
"$ANT" adapt --adapter cargo --project "$WORK/ant-a" --option profile=release   --option granularity=workspace --out "$WORK/plan.json" > /dev/null
e=$(now)
record ant-adapt 1 "$(elapsed "$s" "$e")" - - "plan generation"
grep -c '"cache_policy": "eligible"' "$WORK/plan.json" | xargs echo "eligible actions:"

s=$(now); remote "$WORK/ant-a" "$OUT/remote-cold.log"; e=$(now)
read -r ex re < <(counts "$OUT/remote-cold.log")
record ant-remote-cold 1 "$(elapsed "$s" "$e")" "$ex" "$re" "empty shared cache"
for rep in $(seq 1 "$REPS"); do
  s=$(now); remote "$WORK/ant-a" "$OUT/remote-nochange-$rep.log"; e=$(now)
  read -r ex re < <(counts "$OUT/remote-nochange-$rep.log")
  record ant-remote-no-change "$rep" "$(elapsed "$s" "$e")" "$ex" "$re" "same checkout"
done
for rep in $(seq 1 "$REPS"); do
  checkout "$WORK/ant-fresh-$rep"
  s=$(now); remote "$WORK/ant-fresh-$rep" "$OUT/remote-fresh-$rep.log"; e=$(now)
  read -r ex re < <(counts "$OUT/remote-fresh-$rep.log")
  record ant-remote-fresh-checkout "$rep" "$(elapsed "$s" "$e")" "$ex" "$re" "new checkout, other path, new mtimes"
done

# The remotely built CLI is a working binary.
# The remotely built CLI (installed from the shared cache) is executable
# and runs.
BIN="$WORK/ant-fresh-1/target/release/ant"
test -x "$BIN" || { echo "remote-built binary is not executable"; exit 1; }
"$BIN" --version | sed 's/^/remote-built binary: /'

echo
echo "results: $CSV"
column -s, -t "$CSV"
