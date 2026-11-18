#!/bin/sh
set -eu

root="$(CDPATH= cd -- "$(dirname "$0")/.." && pwd)"
tmp="$(mktemp -d -t ths-native-install.XXXXXX)"
trap 'rm -rf "$tmp"' EXIT HUP INT TERM
case "$(uname -s)-$(uname -m)" in
  Linux-x86_64) target=x86_64-unknown-linux-gnu ;;
  Linux-aarch64|Linux-arm64) target=aarch64-unknown-linux-gnu ;;
  Darwin-x86_64) target=x86_64-apple-darwin ;;
  Darwin-arm64) target=aarch64-apple-darwin ;;
esac
fixtures="$tmp/fixtures"
bin="$tmp/bin"
destination="$tmp/install"
mkdir -p "$fixtures" "$bin" "$destination"
export FIXTURES="$fixtures" THS_INSTALL_DIR="$destination" THS_INSTALL_RUNTIME=native
export NATIVE_INSTALL_REQUESTS="$tmp/requests"
export NATIVE_INSTALL_PULLS="$tmp/pulls"
cat > "$bin/curl" <<'EOF'
#!/bin/sh
for arg do
  case "$arg" in
    https://*) url="$arg" ;;
  esac
done
printf '%s\n' "$url" >> "$NATIVE_INSTALL_REQUESTS"
case "$url" in
  */releases/latest) source="$FIXTURES/latest.json" ;;
  */releases/download/v*/*) source="$FIXTURES/${url##*/}" ;;
  *) echo "unexpected URL $url" >&2; exit 2 ;;
esac
while [ "$#" -gt 0 ]; do
  if [ "$1" = -o ]; then cp "$source" "$2"; exit; fi
  shift
done
exit 2
EOF
chmod +x "$bin/curl"
export PATH="$bin:$PATH"

checksum() {
  if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1" | awk '{print $1}'
  else shasum -a 256 "$1" | awk '{print $1}'; fi
}
checksums() {
  : > "$fixtures/SHA256SUMS"
  for name in "ths-$target.tar.gz" "ths-runtime-$target.tar.gz"; do
    printf '%s  %s\n' "$(checksum "$fixtures/$name")" "$name" >> "$fixtures/SHA256SUMS"
  done
}
package() {
  tar -czf "$fixtures/ths-runtime-$target.tar.gz" -C "$fixtures/payload" ths-runtime
  tar -czf "$fixtures/ths-$target.tar.gz" -C "$fixtures/payload" ths
  checksums
}
fixture() {
  version="$1"
  rm -rf "$fixtures/payload"
  mkdir -p "$fixtures/payload/ths-runtime/bin" "$fixtures/payload/ths-runtime/web"
  cat > "$fixtures/payload/ths" <<EOF
#!/bin/sh
set -eu
case "\${1:-}" in
  --version) echo 'ths $version' ;;
  pull)
    test "\${NATIVE_INSTALL_ALLOW_PULL:-0}" = 1 || exit 97
    printf 'pull\n' >> "\$NATIVE_INSTALL_PULLS"
    ;;
  doctor)
    test "\$2" = --native && test "\$3" = --runtime-dir
    test "\$(cat "\$4/manifest.json")" = '$version'
    for name in zakurad lightwalletd ths-server; do test -x "\$4/bin/\$name" || exit 1; done
    test -f "\$4/web/index.html"
    if [ -n "\${NATIVE_LATE_TARGET:-}" ]; then
      mkdir -p "\$NATIVE_LATE_TARGET"
      printf '%s\n' sentinel > "\$NATIVE_LATE_TARGET/ths"
      if [ "\$NATIVE_LATE_TARGET" != "\$THS_INSTALL_DIR/ths" ] && [ ! -L "\$THS_INSTALL_DIR/ths" ]; then
        ln -s "\$NATIVE_LATE_TARGET" "\$THS_INSTALL_DIR/ths"
      fi
    fi
    ;;
  *) echo 'native install must not invoke Docker' >&2; exit 97 ;;
esac
EOF
  chmod +x "$fixtures/payload/ths"
  for name in zakurad lightwalletd ths-server; do
    printf '#!/bin/sh\nexit 0\n' > "$fixtures/payload/ths-runtime/bin/$name"
    chmod +x "$fixtures/payload/ths-runtime/bin/$name"
  done
  printf '%s\n' "$version" > "$fixtures/payload/ths-runtime/manifest.json"
  printf '%s\n' dashboard > "$fixtures/payload/ths-runtime/web/index.html"
  printf '{"tag_name":"v%s"}\n' "$version" > "$fixtures/latest.json"
  package
}
must_fail() {
  if THS_VERSION=v9.8.7 "$root/install.sh" > "$tmp/failure.log" 2>&1; then
    echo "native installer unexpectedly succeeded: $1" >&2; exit 1
  fi
  test "$(cat "$destination/ths")" = old
}

fixture 9.8.7
# Native preparation must not turn a launcher directory into an mv container.
for timing in before during; do
  for kind in directory symlink; do
    conflict="$tmp/$timing-$kind"
    mkdir -p "$conflict"
    target_dir="$conflict/ths"
    if [ "$kind" = symlink ]; then target_dir="$tmp/$timing-external"; fi
    late_target=""
    if [ "$timing" = before ]; then
      mkdir -p "$target_dir"
      printf '%s\n' sentinel > "$target_dir/ths"
      if [ "$kind" = symlink ]; then ln -s "$target_dir" "$conflict/ths"; fi
    else
      late_target="$target_dir"
    fi
    : > "$NATIVE_INSTALL_REQUESTS"
    if THS_INSTALL_DIR="$conflict" NATIVE_LATE_TARGET="$late_target" \
      THS_VERSION=v9.8.7 "$root/install.sh" > "$tmp/conflict.log" 2>&1; then
      echo "native installer accepted $timing $kind target" >&2; exit 1
    fi
    grep -q 'Launcher destination is a directory' "$tmp/conflict.log"
    if grep -q 'Installed ths' "$tmp/conflict.log"; then exit 1; fi
    test "$(cat "$target_dir/ths")" = sentinel
    if [ "$kind" = symlink ]; then test "$(readlink "$conflict/ths")" = "$target_dir"; fi
    if [ "$timing" = before ]; then test ! -s "$NATIVE_INSTALL_REQUESTS"
    else test -x "$conflict/.ths-runtime/9.8.7/bin/zakurad"; fi
    test ! -e "$conflict/.ths-install.lock"
    test -z "$(find "$conflict" -name '.ths.*' -print)"
  done
done

fixture 9.8.7
printf '%s\n' old > "$destination/ths"
THS_VERSION=latest "$root/install.sh"
test "$("$destination/ths" --version)" = 'ths 9.8.7'
test -x "$destination/.ths-runtime/9.8.7/bin/zakurad"
test "$(grep -c '/releases/latest$' "$NATIVE_INSTALL_REQUESTS")" -eq 1
if grep -q '/latest/download/' "$NATIVE_INSTALL_REQUESTS"; then exit 1; fi
# An identical version can be installed twice without mutating its runtime.
THS_VERSION=v9.8.7 "$root/install.sh"
# A native installation must keep receiving matching bundles on default
# updates, whether Docker is available or not.
for selection in default auto; do
  if [ "$selection" = default ]; then version=9.8.8; allow_pull=0
  else version=9.8.9; allow_pull=1; fi
  fixture "$version"
  : > "$NATIVE_INSTALL_REQUESTS"
  : > "$NATIVE_INSTALL_PULLS"
  (
    if [ "$selection" = default ]; then unset THS_INSTALL_RUNTIME
    else export THS_INSTALL_RUNTIME=auto; fi
    THS_VERSION="$version" THS_SKIP_IMAGE_PULL=0 NATIVE_INSTALL_ALLOW_PULL="$allow_pull" "$root/install.sh"
  )
  test "$("$destination/ths" --version)" = "ths $version"
  test "$(cat "$destination/.ths-runtime/$version/manifest.json")" = "$version"
  test "$(cat "$destination/.ths-runtime/9.8.7/manifest.json")" = 9.8.7
  test ! -s "$NATIVE_INSTALL_PULLS"
done

# A missing or damaged native upgrade must not silently switch to Docker
# or replace the working launcher and its previous runtime.
fixture 9.8.10
cp "$destination/ths" "$tmp/previous-launcher"
for failure in missing damaged; do
  if [ "$failure" = missing ]; then
    sed '/ths-runtime-/d' "$fixtures/SHA256SUMS" > "$fixtures/checksums-old"
    mv "$fixtures/checksums-old" "$fixtures/SHA256SUMS"
    expected='Missing or ambiguous checksum for ths-runtime-'
  else
    checksums
    printf 'bad runtime\n' > "$fixtures/ths-runtime-$target.tar.gz"
    expected='Checksum verification failed for ths-runtime-'
  fi
  if (unset THS_INSTALL_RUNTIME; THS_VERSION=9.8.10 NATIVE_INSTALL_ALLOW_PULL=1 "$root/install.sh") > "$tmp/failure.log" 2>&1; then
    echo "default native upgrade unexpectedly succeeded: $failure bundle" >&2; exit 1
  fi
  grep -q "$expected" "$tmp/failure.log"
  cmp "$destination/ths" "$tmp/previous-launcher"
  test "$(cat "$destination/.ths-runtime/9.8.9/manifest.json")" = 9.8.9
  test ! -e "$destination/.ths-runtime/9.8.10"
  test ! -s "$NATIVE_INSTALL_PULLS"
done

fixture 9.8.7
printf '%s\n' old > "$destination/ths"
printf '%s\n' modified > "$destination/.ths-runtime/9.8.7/web/index.html"
must_fail 'replacing a different same-version runtime'
test "$(cat "$destination/.ths-runtime/9.8.7/web/index.html")" = modified
rm -rf "$destination/.ths-runtime"

printf 'bad runtime\n' > "$fixtures/ths-runtime-$target.tar.gz"
must_fail 'runtime checksum mismatch'
test ! -e "$destination/.ths-runtime/9.8.7"

fixture 9.8.7
ln -s "$tmp/outside" "$fixtures/payload/ths-runtime/web/link"
package
must_fail 'symbolic link in native archive'
test ! -e "$tmp/outside"

fixture 9.8.7
ln "$fixtures/payload/ths-runtime/bin/zakurad" "$fixtures/payload/ths-runtime/bin/hardlink"
package
must_fail 'hard link in native archive'

fixture 9.8.7
printf '%s\n' unrelated > "$fixtures/payload/outside"
tar -czf "$fixtures/ths-runtime-$target.tar.gz" -C "$fixtures/payload" ths-runtime outside
checksums
must_fail 'unexpected archive root'

fixture 9.8.7
rm "$fixtures/payload/ths-runtime/bin/lightwalletd"
package
must_fail 'missing executable'

fixture 9.8.7
printf '%s\n' wrong > "$fixtures/payload/ths-runtime/manifest.json"
package
must_fail 'mismatched runtime version'

fixture 9.8.7
mkdir "$destination/.ths-install.lock"
must_fail 'another installer owns the destination'
rmdir "$destination/.ths-install.lock"
test ! -e "$destination/.ths-runtime/9.8.7"

fixture 9.8.7
printf '%s\n' archive-content > "$tmp/absolute-entry"
tar -czf "$fixtures/ths-runtime-$target.tar.gz" -P "$tmp/absolute-entry"
checksums
printf '%s\n' original > "$tmp/absolute-entry"
must_fail 'absolute archive path'
test "$(cat "$tmp/absolute-entry")" = original

# Failure at the launcher commit leaves the previous executable usable.
fixture 9.8.7
export NATIVE_INSTALL_REAL_MV="$(command -v mv)"
cat > "$bin/mv" <<'EOF'
#!/bin/sh
if [ "$#" -eq 3 ] && [ "$1" = -f ] && [ "$3" = "$THS_INSTALL_DIR/ths" ]; then exit 75; fi
exec "$NATIVE_INSTALL_REAL_MV" "$@"
EOF
chmod +x "$bin/mv"
must_fail 'launcher replacement failure after runtime installation'
test -x "$destination/.ths-runtime/9.8.7/bin/zakurad"
rm "$bin/mv"
THS_VERSION=v9.8.7 "$root/install.sh"
test "$("$destination/ths" --version)" = 'ths 9.8.7'

# Without a native installation, default installs and updates prepare Docker,
# even when the release also offers native bundles.
for selection in auto default; do
  for version in 9.8.7 9.8.8; do
    fixture "$version"
    : > "$NATIVE_INSTALL_REQUESTS"
    : > "$NATIVE_INSTALL_PULLS"
    (
      if [ "$selection" = default ]; then unset THS_INSTALL_RUNTIME
      else export THS_INSTALL_RUNTIME=auto; fi
      THS_INSTALL_DIR="$tmp/docker-$selection" THS_VERSION="$version" THS_SKIP_IMAGE_PULL=0 NATIVE_INSTALL_ALLOW_PULL=1 "$root/install.sh"
    )
    test "$(cat "$NATIVE_INSTALL_PULLS")" = pull
    test "$("$tmp/docker-$selection/ths" --version)" = "ths $version"
    if grep -q '/ths-runtime-' "$NATIVE_INSTALL_REQUESTS"; then
      echo 'default Docker installation unexpectedly downloaded a native runtime' >&2; exit 1
    fi
    test ! -e "$tmp/docker-$selection/.ths-runtime"
  done
done
# Explicit choices still override native detection, without removing bundles.
fixture 9.8.7
: > "$NATIVE_INSTALL_REQUESTS"
: > "$NATIVE_INSTALL_PULLS"
THS_VERSION=v9.8.7 THS_INSTALL_RUNTIME=docker THS_SKIP_IMAGE_PULL=0 NATIVE_INSTALL_ALLOW_PULL=1 "$root/install.sh"
test "$(cat "$NATIVE_INSTALL_PULLS")" = pull
THS_VERSION=v9.8.7 THS_INSTALL_RUNTIME=none NATIVE_INSTALL_ALLOW_PULL=0 "$root/install.sh"
if grep -q '/ths-runtime-' "$NATIVE_INSTALL_REQUESTS"; then exit 1; fi
test -f "$destination/.ths-runtime/9.8.7/manifest.json"
# Historical releases still support Docker or launcher-only installation.
sed '/ths-runtime-/d' "$fixtures/SHA256SUMS" > "$fixtures/checksums-old"
mv "$fixtures/checksums-old" "$fixtures/SHA256SUMS"
THS_INSTALL_DIR="$tmp/historical" THS_VERSION=v9.8.7 THS_INSTALL_RUNTIME=auto THS_SKIP_IMAGE_PULL=1 "$root/install.sh"
printf '%s\n' old > "$destination/ths"
must_fail 'explicit native request on a historical release'
echo 'native installer tests passed'
