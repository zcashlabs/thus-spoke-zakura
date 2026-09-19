#!/bin/sh
set -eu

root="$(CDPATH= cd -- "$(dirname "$0")/.." && pwd)"
tmp="$(mktemp -d -t tsz-install-test.XXXXXX)"
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

cat > "$fixtures/payload/ths" <<'EOF'
#!/bin/sh
case "${1:-}" in
  --version) echo "ths 9.8.7" ;;
  pull) [ "${FAIL_PULL:-0}" != "1" ] ;;
  *) exit 2 ;;
esac
EOF
chmod +x "$fixtures/payload/ths"
tar -czf "$fixtures/$asset" -C "$fixtures/payload" ths
if command -v sha256sum >/dev/null 2>&1; then
  hash="$(sha256sum "$fixtures/$asset" | awk '{print $1}')"
else
  hash="$(shasum -a 256 "$fixtures/$asset" | awk '{print $1}')"
fi
printf '%s  %s\n' "$hash" "$asset" > "$fixtures/SHA256SUMS"

cat > "$bin/curl" <<'EOF'
#!/bin/sh
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

printf '%s\n' old > "$install_dir/ths"
PATH="$bin:$PATH" FIXTURES="$fixtures" TSZ_INSTALL_DIR="$install_dir" \
  TSZ_VERSION=v9.8.7 TSZ_SKIP_IMAGE_PULL=1 "$root/install.sh"
test "$("$install_dir/ths" --version)" = "ths 9.8.7"
test ! -e "$install_dir/thus-spoke-zakura"

sed 's/9\.8\.7/8.0.0/' "$fixtures/payload/ths" \
  > "$fixtures/payload/ths.next"
mv "$fixtures/payload/ths.next" "$fixtures/payload/ths"
chmod +x "$fixtures/payload/ths"
tar -czf "$fixtures/$asset" -C "$fixtures/payload" ths
if command -v sha256sum >/dev/null 2>&1; then
  hash="$(sha256sum "$fixtures/$asset" | awk '{print $1}')"
else
  hash="$(shasum -a 256 "$fixtures/$asset" | awk '{print $1}')"
fi
printf '%s  %s\n' "$hash" "$asset" > "$fixtures/SHA256SUMS"
PATH="$bin:$PATH" FIXTURES="$fixtures" TSZ_INSTALL_DIR="$install_dir" \
  TSZ_VERSION=8.0.0 TSZ_SKIP_IMAGE_PULL=1 "$root/install.sh"
test "$("$install_dir/ths" --version)" = "ths 8.0.0"
PATH="$bin:$PATH" FIXTURES="$fixtures" TSZ_INSTALL_DIR="$install_dir" \
  TSZ_VERSION=8.0.0 TSZ_SKIP_IMAGE_PULL=1 "$root/install.sh"
test "$("$install_dir/ths" --version)" = "ths 8.0.0"

printf '%s\n' old > "$install_dir/ths"
if PATH="$bin:$PATH" FIXTURES="$fixtures" FAIL_PULL=1 TSZ_INSTALL_DIR="$install_dir" \
  TSZ_VERSION=8.0.0 "$root/install.sh"; then
  echo "installer unexpectedly succeeded when image pull failed" >&2
  exit 1
fi
test "$(cat "$install_dir/ths")" = old

printf '%s\n' "deadbeef  $asset" > "$fixtures/SHA256SUMS"
if PATH="$bin:$PATH" FIXTURES="$fixtures" TSZ_INSTALL_DIR="$install_dir" \
  TSZ_VERSION=v9.8.7 TSZ_SKIP_IMAGE_PULL=1 "$root/install.sh"; then
  echo "installer unexpectedly accepted a bad checksum" >&2
  exit 1
fi
test "$(cat "$install_dir/ths")" = old

printf '%s\n' incomplete > "$fixtures/$asset"
if command -v sha256sum >/dev/null 2>&1; then
  hash="$(sha256sum "$fixtures/$asset" | awk '{print $1}')"
else
  hash="$(shasum -a 256 "$fixtures/$asset" | awk '{print $1}')"
fi
printf '%s  %s\n' "$hash" "$asset" > "$fixtures/SHA256SUMS"
if PATH="$bin:$PATH" FIXTURES="$fixtures" TSZ_INSTALL_DIR="$install_dir" \
  TSZ_VERSION=v9.8.7 TSZ_SKIP_IMAGE_PULL=1 "$root/install.sh"; then
  echo "installer unexpectedly accepted an incomplete archive" >&2
  exit 1
fi
test "$(cat "$install_dir/ths")" = old

echo "installer tests passed"
