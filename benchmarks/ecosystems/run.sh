#!/usr/bin/env bash
# Local-first benchmark (P3) beyond Rust: plain `ant build` vs the native
# tool on the developer's own machine, no server, for a real npm web app
# (Vite + React + TypeScript, scaffolded fresh) and a real Android app
# (JetNews from android/compose-samples, pinned). Scenarios, per tool, each
# in its own copy of the project:
#   cold         first build (dependencies already installed/downloaded
#                once beforehand, so this measures building, not fetching)
#   no-change    build again with nothing changed
#   edit         one source file changed (a comment line appended)
#   revert       the edit undone (back to an already-built state)
# Every observation goes to results.csv; nothing is discarded.
# Usage: ANT=/path/to/ant benchmarks/ecosystems/run.sh [npm] [android]
set -euo pipefail

REPO=${REPO:-$(cd "$(dirname "$0")/../.." && pwd)}
ANT=${ANT:?set ANT to an ant binary (a release build)}
OUT=${OUT:-$REPO/benchmarks/ecosystems/out}
REPS=${REPS:-3}
SAMPLES_SHA=${SAMPLES_SHA:-25631e8f544a9a1e3f01e296eaf1b4c28428196c}
WORK=${WORK:-$(mktemp -d)}
mkdir -p "$OUT"
CSV="$OUT/results.csv"
[ -f "$CSV" ] || echo "subject,tool,scenario,rep,seconds,notes" > "$CSV"

now() { date +%s%N; }
elapsed() { awk -v s="$1" -v e="$2" 'BEGIN { printf "%.3f", (e - s) / 1e9 }'; }
record() { echo "$1,$2,$3,$4,$5,$6" >> "$CSV"; echo "  $1 $2 $3 rep $4: $5 s $6"; }

{
  echo "date_utc=$(date -u +%Y-%m-%dT%H:%M:%SZ)"
  echo "ant_commit=$(git -C "$REPO" rev-parse --short HEAD)"
  echo "cpu=$(grep -m1 'model name' /proc/cpuinfo 2>/dev/null | cut -d: -f2- | xargs || true)"
  echo "cores=$(nproc 2>/dev/null || true)"
  echo "os=$(. /etc/os-release 2>/dev/null && echo "$PRETTY_NAME") $(uname -sr)"
  echo "node=$(node --version 2>/dev/null || true) npm=$(npm --version 2>/dev/null || true)"
  echo "java=$(java -version 2>&1 | head -1 || true)"
  echo "ant=$("$ANT" --version)"
  echo "reps=$REPS"
} > "$OUT/environment.txt"
cat "$OUT/environment.txt"

# measure SUBJECT TOOL DIR EDIT_FILE COMMAND...
measure() {
  local subject=$1 tool=$2 dir=$3 edit=$4
  shift 4
  local s e
  run() { (cd "$dir" && "$@" > "$OUT/$subject-$tool.log" 2>&1) || { tail -40 "$OUT/$subject-$tool.log"; exit 1; }; }
  s=$(now); run "$@"; e=$(now)
  record "$subject" "$tool" cold 1 "$(elapsed "$s" "$e")" "dependencies pre-fetched"
  for rep in $(seq 1 "$REPS"); do
    s=$(now); run "$@"; e=$(now)
    record "$subject" "$tool" no-change "$rep" "$(elapsed "$s" "$e")" ""
  done
  cp "$dir/$edit" "$WORK/edit-backup"
  for rep in $(seq 1 "$REPS"); do
    printf '\n// ant benchmark edit %s\n' "$rep" >> "$dir/$edit"
    s=$(now); run "$@"; e=$(now)
    record "$subject" "$tool" edit "$rep" "$(elapsed "$s" "$e")" "$edit"
    cp "$WORK/edit-backup" "$dir/$edit"
    s=$(now); run "$@"; e=$(now)
    record "$subject" "$tool" revert "$rep" "$(elapsed "$s" "$e")" "$edit"
  done
}

bench_npm() {
  echo "=== npm: Vite + React + TypeScript"
  local base="$WORK/npm-base"
  (cd "$WORK" && npm create --yes vite@6 npm-base -- --template react-ts > "$OUT/npm-create.log" 2>&1)
  (cd "$base" && npm install --no-audit --no-fund > "$OUT/npm-install.log" 2>&1)
  echo "subject_npm=create-vite@6 react-ts; vite $(cd "$base" && npx vite --version)" >> "$OUT/environment.txt"
  cp -R "$base" "$WORK/npm-native"
  cp -R "$base" "$WORK/npm-ant"
  measure npm native "$WORK/npm-native" src/App.tsx npm run build
  ANT_CACHE_HOME="$WORK/ant-cache" measure npm ant "$WORK/npm-ant" src/App.tsx "$ANT" build
}

bench_android() {
  echo "=== Android: JetNews"
  git clone -q --filter=blob:none --no-checkout https://github.com/android/compose-samples "$WORK/samples"
  git -C "$WORK/samples" sparse-checkout set JetNews
  git -C "$WORK/samples" checkout -q "$SAMPLES_SHA"
  echo "subject_android=android/compose-samples@$SAMPLES_SHA JetNews assembleDebug" >> "$OUT/environment.txt"
  cp -R "$WORK/samples/JetNews" "$WORK/android-native"
  cp -R "$WORK/samples/JetNews" "$WORK/android-ant"
  # Download dependencies and the Gradle distribution once, outside the
  # measurements (both tools use the developer's own ~/.gradle).
  (cd "$WORK/samples/JetNews" && ./gradlew --console=plain -q assembleDebug > "$OUT/android-prefetch.log" 2>&1)
  local edit
  edit=$(cd "$WORK/android-native" && find app/src/main -name '*.kt' | sort | head -1)
  # Gradle at its best: its daemon stays warm between builds.
  measure android native "$WORK/android-native" "$edit" ./gradlew --console=plain -q assembleDebug
  ANT_CACHE_HOME="$WORK/ant-cache" measure android ant "$WORK/android-ant" "$edit" "$ANT" build
  (cd "$WORK/android-native" && ./gradlew --stop -q) || true
}

subjects=("$@")
[ ${#subjects[@]} -gt 0 ] || subjects=(npm android)
for subject in "${subjects[@]}"; do
  case "$subject" in
    npm) bench_npm ;;
    android) bench_android ;;
    *) echo "unknown subject $subject" >&2; exit 2 ;;
  esac
done
echo
column -s, -t "$CSV" 2>/dev/null || cat "$CSV"
