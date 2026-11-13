#!/bin/sh
set -eu
export THS_INSTALL_RUNTIME=docker

root="$(CDPATH= cd -- "$(dirname "$0")/.." && pwd)"
tmp="$(mktemp -d -t ths-install-test.XXXXXX)"
trap 'rm -rf "$tmp"' EXIT HUP INT TERM

target="x86_64-unknown-linux-gnu"
case "$(uname -s)-$(uname -m)" in
  Linux-aarch64|Linux-arm64) target="aarch64-unknown-linux-gnu" ;;
  Darwin-x86_64) target="x86_64-apple-darwin" ;;
  Darwin-arm64) target="aarch64-apple-darwin" ;;
esac
asset="ths-$target.tar.gz"
fixtures="$tmp/fixtures"
bin="$tmp/bin"
install_dir="$tmp/install"
mkdir -p "$fixtures/payload" "$bin" "$install_dir"

write_checksum() {
  if command -v sha256sum >/dev/null 2>&1; then
    hash="$(sha256sum "$fixtures/$asset" | awk '{print $1}')"
  else
    hash="$(shasum -a 256 "$fixtures/$asset" | awk '{print $1}')"
  fi
  printf '%s  %s\n' "$hash" "$asset" > "$fixtures/SHA256SUMS"
}

package_payload() {
  tar -czf "$fixtures/$asset" -C "$fixtures/payload" ths
  write_checksum
}

cat > "$fixtures/payload/ths" <<'EOF'
#!/bin/sh
case "${1:-}" in
  --version) echo "ths 9.8.7" ;;
  pull) [ "${FAIL_PULL:-0}" != "1" ] ;;
  *) exit 2 ;;
esac
EOF
chmod +x "$fixtures/payload/ths"
package_payload

cat > "$bin/curl" <<'EOF'
#!/bin/sh
[ -z "${CURL_LOG:-}" ] || echo download >> "$CURL_LOG"
if [ -n "${LATE_TARGET:-}" ]; then
  mkdir -p "$LATE_TARGET"
  printf '%s\n' sentinel > "$LATE_TARGET/ths"
  if [ "$LATE_TARGET" != "$THS_INSTALL_DIR/ths" ] && [ ! -L "$THS_INSTALL_DIR/ths" ]; then
    ln -s "$LATE_TARGET" "$THS_INSTALL_DIR/ths"
  fi
fi
for arg do
  case "$arg" in
    */SHA256SUMS) source="$FIXTURES/SHA256SUMS" ;;
    */ths-*.tar.gz) source="$FIXTURES/${arg##*/}" ;;
  esac
done
while [ "$#" -gt 0 ]; do
  if [ "$1" = "-o" ]; then cp "$source" "$2"; exit; fi
  shift
done
exit 2
EOF
chmod +x "$bin/curl"

# Check both preflight rejection and a target changed during download.
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
    if PATH="$bin:$PATH" FIXTURES="$fixtures" CURL_LOG="$conflict/downloads" \
      LATE_TARGET="$late_target" THS_INSTALL_DIR="$conflict" \
      THS_VERSION=v9.8.7 THS_SKIP_IMAGE_PULL=1 "$root/install.sh" > "$conflict/output" 2>&1; then
      echo "installer accepted $timing $kind target" >&2
      exit 1
    fi
    grep -q 'Launcher destination is a directory' "$conflict/output"
    if grep -q 'Installed ths' "$conflict/output"; then exit 1; fi
    test "$(cat "$target_dir/ths")" = sentinel
    test -d "$conflict/ths"
    if [ "$kind" = symlink ]; then test "$(readlink "$conflict/ths")" = "$target_dir"; fi
    if [ "$timing" = before ]; then test ! -e "$conflict/downloads"; fi
    test -z "$(find "$conflict" -name '.ths.*' -print)"
    test ! -e "$conflict/.ths-install.lock"
  done
done

printf '%s\n' old > "$install_dir/ths"
PATH="$bin:$PATH" FIXTURES="$fixtures" THS_INSTALL_DIR="$install_dir" \
  THS_VERSION=v9.8.7 THS_SKIP_IMAGE_PULL=1 "$root/install.sh"
test "$("$install_dir/ths" --version)" = "ths 9.8.7"
test ! -e "$install_dir/thus-spoke-zakura"

sed 's/9\.8\.7/8.0.0/' "$fixtures/payload/ths" \
  > "$fixtures/payload/ths.next"
mv "$fixtures/payload/ths.next" "$fixtures/payload/ths"
chmod +x "$fixtures/payload/ths"
package_payload
PATH="$bin:$PATH" FIXTURES="$fixtures" THS_INSTALL_DIR="$install_dir" \
  THS_VERSION=8.0.0 THS_SKIP_IMAGE_PULL=1 "$root/install.sh"
test "$("$install_dir/ths" --version)" = "ths 8.0.0"
PATH="$bin:$PATH" FIXTURES="$fixtures" THS_INSTALL_DIR="$install_dir" \
  THS_VERSION=8.0.0 THS_SKIP_IMAGE_PULL=1 "$root/install.sh"
test "$("$install_dir/ths" --version)" = "ths 8.0.0"

printf '%s\n' old > "$install_dir/ths"
if PATH="$bin:$PATH" FIXTURES="$fixtures" FAIL_PULL=1 THS_INSTALL_DIR="$install_dir" \
  THS_VERSION=8.0.0 "$root/install.sh"; then
  echo "installer unexpectedly succeeded when image pull failed" >&2
  exit 1
fi
test "$(cat "$install_dir/ths")" = old

printf '%s\n' "deadbeef  $asset" > "$fixtures/SHA256SUMS"
if PATH="$bin:$PATH" FIXTURES="$fixtures" THS_INSTALL_DIR="$install_dir" \
  THS_VERSION=v9.8.7 THS_SKIP_IMAGE_PULL=1 "$root/install.sh"; then
  echo "installer unexpectedly accepted a bad checksum" >&2
  exit 1
fi
test "$(cat "$install_dir/ths")" = old

printf '%s\n' incomplete > "$fixtures/$asset"
write_checksum
if PATH="$bin:$PATH" FIXTURES="$fixtures" THS_INSTALL_DIR="$install_dir" \
  THS_VERSION=v9.8.7 THS_SKIP_IMAGE_PULL=1 "$root/install.sh"; then
  echo "installer unexpectedly accepted an incomplete archive" >&2
  exit 1
fi
test "$(cat "$install_dir/ths")" = old

echo "installer tests passed"
"$root/tests/native_install.sh"
