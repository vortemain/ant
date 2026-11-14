#!/usr/bin/env bash
# Local-first benchmark (P3): plain `ant build` vs plain `cargo build` on this
# repository, on the developer's own machine, no server. Scenarios:
#   cold         first build of a fresh clone (empty target dir, empty Ant cache)
#   no-change    build again with nothing changed
#   switch-back  check out an older commit, build, check the newer one out
#                again and build (the branch-switching edit loop)
# Both tools build the debug profile of the whole workspace in their own
# clone. Every observation goes to results.csv; nothing is discarded.
# Works on Linux and on Windows (Git Bash). Requires cargo, git, and an `ant`
# binary (ANT=...). OLD/NEW pick the two commits (default HEAD~2 and HEAD).
set -euo pipefail

REPO=${REPO:-$(cd "$(dirname "$0")/../.." && pwd)}
ANT=${ANT:?set ANT to an ant binary (a release build)}
OUT=${OUT:-$REPO/benchmarks/local/out}
REPS=${REPS:-3}
NEW=${NEW:-$(git -C "$REPO" rev-parse --short HEAD)}
OLD=${OLD:-$(git -C "$REPO" rev-parse --short HEAD~2)}
WORK=${WORK:-$(mktemp -d)}
mkdir -p "$OUT"
CSV="$OUT/results.csv"
echo "tool,scenario,rep,seconds,notes" > "$CSV"

now() { date +%s%N; }
elapsed() { awk -v s="$1" -v e="$2" 'BEGIN { printf "%.3f", (e - s) / 1e9 }'; }
record() { echo "$1,$2,$3,$4,$5" >> "$CSV"; echo "  $1 $2 rep $3: $4 s $5"; }

{
  echo "date_utc=$(date -u +%Y-%m-%dT%H:%M:%SZ)"
  echo "subject=$(git -C "$REPO" remote get-url origin 2>/dev/null || echo local) old=$OLD new=$NEW"
  if [ -r /etc/os-release ] && ! uname -s | grep -qiE "mingw|msys|cygwin"; then
    echo "cpu=$(grep -m1 'model name' /proc/cpuinfo | cut -d: -f2- | xargs)"
    echo "cores=$(nproc)"
    echo "ram_gib=$(awk '/MemTotal/ { printf "%.1f", $2/1048576 }' /proc/meminfo)"
    echo "os=$(. /etc/os-release && echo "$PRETTY_NAME") kernel $(uname -r)"
  else
    echo "cpu=$(powershell.exe -NoProfile -Command '(Get-CimInstance Win32_Processor).Name' | tr -d '\r')"
    echo "cores=$(powershell.exe -NoProfile -Command '(Get-CimInstance Win32_Processor).NumberOfLogicalProcessors' | tr -d '\r')"
    echo "ram_gib=$(powershell.exe -NoProfile -Command '[math]::Round((Get-CimInstance Win32_ComputerSystem).TotalPhysicalMemory/1GB,1)' | tr -d '\r')"
    echo "os=$(powershell.exe -NoProfile -Command '(Get-CimInstance Win32_OperatingSystem).Caption + \" \" + (Get-CimInstance Win32_OperatingSystem).Version' | tr -d '\r')"
  fi
  echo "rustc=$(rustc --version)"
  echo "ant=$("$ANT" --version)"
  echo "reps=$REPS"
} > "$OUT/environment.txt"
cat "$OUT/environment.txt"

clone() { git clone --quiet "$REPO" "$1"; git -C "$1" checkout --quiet "$NEW"; }

run_scenarios() {
  local tool=$1 dir=$2
  shift 2
  local build=("$@")
  local s e
  s=$(now); (cd "$dir" && "${build[@]}" > /dev/null 2>&1); e=$(now)
  record "$tool" cold 1 "$(elapsed "$s" "$e")" "fresh clone at $NEW"
  # Warm the older commit once so switching back has something to reuse.
  git -C "$dir" checkout --quiet "$OLD"
  (cd "$dir" && "${build[@]}" > /dev/null 2>&1)
  git -C "$dir" checkout --quiet "$NEW"
  (cd "$dir" && "${build[@]}" > /dev/null 2>&1)
  for rep in $(seq 1 "$REPS"); do
    s=$(now); (cd "$dir" && "${build[@]}" > /dev/null 2>&1); e=$(now)
    record "$tool" no-change "$rep" "$(elapsed "$s" "$e")" "same checkout"
  done
  for rep in $(seq 1 "$REPS"); do
    git -C "$dir" checkout --quiet "$OLD"
    s=$(now); (cd "$dir" && "${build[@]}" > /dev/null 2>&1); e=$(now)
    record "$tool" switch-to-old "$rep" "$(elapsed "$s" "$e")" "$NEW -> $OLD"
    git -C "$dir" checkout --quiet "$NEW"
    s=$(now); (cd "$dir" && "${build[@]}" > /dev/null 2>&1); e=$(now)
    record "$tool" switch-back "$rep" "$(elapsed "$s" "$e")" "$OLD -> $NEW"
  done
}

clone "$WORK/cargo"
run_scenarios cargo "$WORK/cargo" cargo build --workspace

clone "$WORK/ant"
export ANT_CACHE_HOME="$WORK/ant-cache"
run_scenarios ant "$WORK/ant" "$ANT" build --dev

echo "raw results: $CSV"
