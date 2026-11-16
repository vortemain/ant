#!/usr/bin/env sh
# Build each example with `ant build` and run what lands in dist/
# (P6/P5 verification; used by .github/workflows/verify-adapters.yml).
# Usage: scripts/verify-adapters.sh ANT_BINARY ECOSYSTEM...
set -eu
ant=$(cd "$(dirname "$1")" && pwd)/$(basename "$1")
shift
repo=$(cd "$(dirname "$0")/.." && pwd)
work=${RUNNER_TEMP:-${TMPDIR:-/tmp}}/ant-verify-adapters
rm -rf "$work"
mkdir -p "$work"
export ANT_CACHE_HOME="$work/cache"

fresh() {
  cp -R "$repo/examples/$1" "$work/$1"
  cd "$work/$1"
}

# Run a build twice: the second must succeed too (reuse or a clean rebuild).
build() {
  "$ant" build "$@"
  "$ant" build "$@"
}

for ecosystem in "$@"; do
  echo "=== $ecosystem"
  case "$ecosystem" in
    maven)
      fresh maven-hello
      build
      java -jar dist/hello-1.0.0.jar | grep -q "Hello from Maven, built by Ant"
      ;;
    python)
      fresh python-hello
      build
      ls dist/*.whl
      python -m venv "$work/venv"
      if [ -x "$work/venv/bin/python" ]; then py="$work/venv/bin/python"; else py="$work/venv/Scripts/python"; fi
      "$py" -m pip install -q dist/*.whl
      "$py" -c "import hello; print(hello.greet())" | grep -q "Hello from Python, built by Ant"
      ;;
    docker)
      fresh docker-hello
      build
      docker run --rm docker-hello | grep -q "Hello from a container, built by Ant"
      printf '[options]\noutput = "tar"\n' > ant.toml
      "$ant" build
      docker image rm -f docker-hello >/dev/null
      docker load -i dist/image.tar
      docker run --rm docker-hello | grep -q "Hello from a container, built by Ant"
      ;;
    dotnet)
      fresh dotnet-hello
      build
      dotnet dist/Hello/Hello.dll | grep -q "Hello from .NET, built by Ant"
      ;;
    flutter)
      cd "$work" && flutter create --platforms web --project-name flutter_hello flutter_hello >/dev/null
      cd flutter_hello
      build
      for file in index.html main.dart.js flutter.js flutter_bootstrap.js; do test -f "dist/web/$file"; done
      ;;
    ios)
      fresh ios-hello
      xcodegen generate --quiet
      build
      ls -d dist/*.app
      ;;
    *)
      echo "unknown ecosystem $ecosystem" >&2
      exit 2
      ;;
  esac
  echo "ok: $ecosystem"
done
