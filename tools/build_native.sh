#!/bin/sh
set -eu

root="$(CDPATH= cd -- "$(dirname "$0")/.." && pwd)"
output="${1:?Usage: tools/build_native.sh NEW_OUTPUT_DIR}"
[ "$#" -eq 1 ] || { echo 'Expected one output directory' >&2; exit 1; }
[ ! -e "$output" ] || { echo "Output already exists: $output" >&2; exit 1; }
case "$output" in /*) ;; *) output="$PWD/$output" ;; esac
build="$root/target/native-build"
mkdir -p "$build/bin"
pin() {
  node --input-type=commonjs -e 'const fs=require("node:fs"); console.log(JSON.parse(fs.readFileSync(process.argv[1], "utf8"))[process.argv[2]]);' "$root/native/runtime.json" "$1"
}
checkout() {
  name="$1"
  repository="$(pin "${name}_repository")"
  commit="$(pin "${name}_commit")"
  directory="$build/$name"
  if [ ! -e "$directory" ]; then
    git init "$directory"
    git -C "$directory" remote add origin "$repository"
    git -C "$directory" fetch --depth 1 origin "$commit"
    git -C "$directory" checkout --detach FETCH_HEAD
  fi
  [ "$(git -C "$directory" rev-parse HEAD)" = "$commit" ] || { echo "$name build checkout has the wrong commit" >&2; exit 1; }
  [ -z "$(git -C "$directory" status --porcelain)" ] || { echo "$name build checkout has local modifications" >&2; exit 1; }
}
checkout zakura
checkout lightwalletd
(
  cd "$build/zakura"
  cargo +1.98.0 build --locked --release --target-dir "$build/zakura/target" -p zakura --bin zakurad --features internal-miner
)
cp "$build/zakura/target/release/zakurad" "$build/bin/zakurad"
(
  cd "$build/lightwalletd"
  CGO_ENABLED=0 go build -trimpath -ldflags='-s -w' -o "$build/bin/lightwalletd" ./
)
(
  cd "$root"
  cargo +1.98.0 build --locked --release --target-dir "$root/target" -p ths-server
  npm ci --prefix web
  npm run build --prefix web
)
cp "$root/target/release/ths-server" "$build/bin/ths-server"
node "$root/tools/package_native.mjs" "$build/bin" "$root/web/dist" "$output"
