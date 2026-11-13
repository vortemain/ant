#!/usr/bin/env bash
# Android benchmark (P4): Gradle vs `ant remote run` on a real Android app
# (JetNews from android/compose-samples, pinned). Gradle is measured at its
# best: a warm dependency cache, its daemon, and its build cache on. Ant runs
# through a sandboxed Linux worker (bubblewrap, no network) with the shared
# remote cache (D-019, D-024). Dependencies and the Gradle distribution are
# made project inputs first (`.gradle-deps`, a local wrapper zip), because a
# worker without network cannot download them. Every observation is written
# to results.csv. Requires: Java 17+, the Android SDK (ANDROID_HOME),
# bubblewrap + unprivileged user namespaces, a release `ant` binary.
set -euo pipefail

REPO=${REPO:-$(cd "$(dirname "$0")/../.." && pwd)}
ANT=${ANT:-$REPO/target/release/ant}
OUT=${OUT:-$REPO/benchmarks/android/out}
REPS=${REPS:-3}
SAMPLES_SHA=${SAMPLES_SHA:-25631e8f544a9a1e3f01e296eaf1b4c28428196c}
TASK=assembleDebug
APK=app/build/outputs/apk/debug/app-debug.apk
WORK=$(mktemp -d)
mkdir -p "$OUT"
CSV="$OUT/results.csv"
echo "scenario,rep,seconds,executed,reused,notes" > "$CSV"

now() { date +%s.%N; }
elapsed() { awk -v s="$1" -v e="$2" 'BEGIN { printf "%.2f", e - s }'; }
record() { echo "$1,$2,$3,$4,$5,$6" >> "$CSV"; echo "  $1 rep $2: $3 s (executed=$4 reused=$5) $6"; }

{
  echo "date_utc=$(date -u +%Y-%m-%dT%H:%M:%SZ)"
  echo "ant_commit=$(git -C "$REPO" rev-parse --short HEAD)"
  echo "subject=android/compose-samples@$SAMPLES_SHA JetNews :app $TASK"
  echo "cpu=$(grep -m1 'model name' /proc/cpuinfo | cut -d: -f2- | xargs)"
  echo "cores=$(nproc)"
  echo "ram_gib=$(awk '/MemTotal/ { printf "%.1f", $2/1048576 }' /proc/meminfo)"
  echo "os=$(. /etc/os-release && echo "$PRETTY_NAME") kernel $(uname -r)"
  echo "java=$(java -version 2>&1 | head -1)"
  echo "java_home=${JAVA_HOME:-}"
  echo "android_home=${ANDROID_HOME:-}"
  echo "bwrap=$(bwrap --version)"
  echo "reps=$REPS"
} > "$OUT/environment.txt"
cat "$OUT/environment.txt"

# --- subject ----------------------------------------------------------------
git clone -q --filter=blob:none --no-checkout https://github.com/android/compose-samples "$WORK/samples"
git -C "$WORK/samples" sparse-checkout set JetNews
git -C "$WORK/samples" checkout -q "$SAMPLES_SHA"
SUBJECT="$WORK/subject"
cp -r "$WORK/samples/JetNews" "$SUBJECT"

# Gradle distribution as a project file (a relative distributionUrl resolves
# against gradle-wrapper.properties).
PROPS="$SUBJECT/gradle/wrapper/gradle-wrapper.properties"
DIST_URL=$(grep '^distributionUrl=' "$PROPS" | cut -d= -f2- | tr -d '\\')
DIST_ZIP=$(basename "$DIST_URL")
curl -sSfL "$DIST_URL" -o "$SUBJECT/gradle/wrapper/$DIST_ZIP"
sed -i "s|^distributionUrl=.*|distributionUrl=$DIST_ZIP|" "$PROPS"
echo "gradle_distribution=$DIST_ZIP" >> "$OUT/environment.txt"

# Online preparation build (not a comparison) fills a dependency cache; its
# module cache becomes the project's read-only dependency cache
# (GRADLE_RO_DEP_CACHE layout, lock files removed).
PREP="$WORK/prep"
cp -r "$SUBJECT" "$PREP"
s=$(now)
(cd "$PREP" && GRADLE_USER_HOME="$WORK/prep-home" ./gradlew --no-daemon --console=plain -q $TASK) > "$OUT/prep.log" 2>&1 \
  || { tail -50 "$OUT/prep.log"; exit 1; }
e=$(now)
record prep-online 1 "$(elapsed "$s" "$e")" - - "fills the dependency cache; not a comparison"
mkdir -p "$SUBJECT/.gradle-deps"
cp -r "$WORK/prep-home/caches/modules-2" "$SUBJECT/.gradle-deps/modules-2"
find "$SUBJECT/.gradle-deps" -name '*.lock' -delete
rm -f "$SUBJECT/.gradle-deps/modules-2/gc.properties"
echo "subject: $(du -sh "$SUBJECT" | cut -f1), $(find "$SUBJECT" -type f | wc -l) files (deps $(du -sh "$SUBJECT/.gradle-deps" | cut -f1))" | tee -a "$OUT/environment.txt"
checkout() { cp -r "$SUBJECT" "$1"; }

# --- Gradle baselines -------------------------------------------------------
# Fresh Gradle home (deps through the same read-only cache), daemon on,
# build cache on (the project sets org.gradle.caching=true).
gradle() {
  (cd "$1" && GRADLE_USER_HOME="$WORK/gradle-home" GRADLE_RO_DEP_CACHE="$SUBJECT/.gradle-deps" \
    ./gradlew --offline --console=plain -q $TASK) >> "$OUT/gradle.log" 2>&1 \
    || { tail -50 "$OUT/gradle.log"; exit 1; }
}
checkout "$WORK/gradle-a"
s=$(now); gradle "$WORK/gradle-a"; e=$(now)
record gradle-cold 1 "$(elapsed "$s" "$e")" - - "empty build cache; daemon start; distribution unpack"
test -f "$WORK/gradle-a/$APK"
for rep in $(seq 1 "$REPS"); do
  s=$(now); gradle "$WORK/gradle-a"; e=$(now)
  record gradle-no-change "$rep" "$(elapsed "$s" "$e")" - - "same checkout, warm daemon"
done
for rep in $(seq 1 "$REPS"); do
  checkout "$WORK/gradle-fresh-$rep"
  s=$(now); gradle "$WORK/gradle-fresh-$rep"; e=$(now)
  record gradle-fresh-checkout "$rep" "$(elapsed "$s" "$e")" - - "new checkout; warm daemon + local build cache"
done
(cd "$WORK/gradle-a" && GRADLE_USER_HOME="$WORK/gradle-home" ./gradlew --stop -q) || true

# --- coordinator + one sandboxed worker on loopback --------------------------
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
trap 'cp "$WORK/worker.out" "$OUT/worker.log" 2>/dev/null; kill $WORKER $COORD 2>/dev/null || true' EXIT

remote() {
  "$ANT" remote run --coordinator "$URL" --token-file "$WORK/client.token" \
    --workspace "$1" --plan "$WORK/plan.json" > "$2" 2>&1
}
counts() { echo "$(grep -c 'status=succeeded' "$1" || true) $(grep -c 'status=reused' "$1" || true)"; }

checkout "$WORK/ant-a"
s=$(now); "$ANT" adapt --adapter gradle --project "$WORK/ant-a" --out "$WORK/plan.json" > /dev/null; e=$(now)
record ant-adapt 1 "$(elapsed "$s" "$e")" - - "plan generation"
cp "$WORK/plan.json" "$OUT/plan.json"
grep -q '"cache_policy": "eligible"' "$WORK/plan.json" || { echo "plan is not cache-eligible"; exit 1; }

s=$(now)
remote "$WORK/ant-a" "$OUT/remote-cold.log" || { cat "$OUT/remote-cold.log"; tail -80 "$WORK/worker.out"; exit 1; }
e=$(now)
read -r ex re < <(counts "$OUT/remote-cold.log")
record ant-remote-cold 1 "$(elapsed "$s" "$e")" "$ex" "$re" "empty shared cache; sandboxed, offline"
test -f "$WORK/ant-a/$APK"
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

# The APK installed from the shared cache is a real Android package.
FRESH_APK="$WORK/ant-fresh-1/$APK"
unzip -l "$FRESH_APK" | grep -q AndroidManifest.xml || { echo "cached APK is not an Android package"; exit 1; }
ls -l "$FRESH_APK" | sed 's/^/apk from shared cache: /'

echo
echo "results: $CSV"
column -s, -t "$CSV"
