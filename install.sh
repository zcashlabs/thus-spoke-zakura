#!/bin/sh
set -eu

repo="zcashlabs/thus-spoke-zakura"
requested_version="${THS_VERSION:-latest}"
install_runtime="${THS_INSTALL_RUNTIME:-auto}"
case "$install_runtime" in auto|native|docker|none) ;; *) echo 'THS_INSTALL_RUNTIME must be auto, native, docker, or none' >&2; exit 1 ;; esac
case "$(uname -s)-$(uname -m)" in
  Linux-x86_64) target="x86_64-unknown-linux-gnu" ;;
  Linux-aarch64|Linux-arm64) target="aarch64-unknown-linux-gnu" ;;
  Darwin-x86_64) target="x86_64-apple-darwin" ;;
  Darwin-arm64) target="aarch64-apple-darwin" ;;
  *) echo "Unsupported platform: $(uname -s) $(uname -m)" >&2; exit 1 ;;
esac

destination="${THS_INSTALL_DIR:-$HOME/.local/bin}"
mkdir -p "$destination"
lock="$destination/.ths-install.lock"
mkdir "$lock" 2>/dev/null || {
  echo "Another installer owns $lock. If it was interrupted, remove that empty lock directory and retry." >&2
  exit 1
}
stage=""
cleanup() {
  if [ -n "$stage" ]; then rm -rf "$stage"; fi
  rmdir "$lock"
}
trap cleanup EXIT
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM
stage="$(mktemp -d "$destination/.ths.XXXXXX")"

if [ "$requested_version" = latest ]; then
  curl --proto '=https' --tlsv1.2 -fsSL "https://api.github.com/repos/$repo/releases/latest" -o "$stage/release.json"
  tag="$(sed -n 's/.*"tag_name"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' "$stage/release.json")"
else
  case "$requested_version" in v*) tag="$requested_version" ;; *) tag="v$requested_version" ;; esac
fi
case "$tag" in v[0-9]*) ;; *) echo 'Release version must start with v and a digit' >&2; exit 1 ;; esac
case "$tag" in *[!a-zA-Z0-9.+-]*) echo 'Invalid release version' >&2; exit 1 ;; esac
release_url="https://github.com/$repo/releases/download/$tag"
curl --proto '=https' --tlsv1.2 -fsSL "$release_url/SHA256SUMS" -o "$stage/SHA256SUMS"
if [ "$install_runtime" = auto ]; then
  # Preserve native installations across updates; other installs follow the CLI default.
  if [ -d "$destination/.ths-runtime" ]; then install_runtime=native
  else install_runtime=docker; fi
fi

fetch_verified() {
  asset="$1"
  curl --proto '=https' --tlsv1.2 -fsSL "$release_url/$asset" -o "$stage/$asset"
  expected="$(awk -v asset="$asset" '
    { name=$2; sub(/^\*/, "", name); if (name == asset) print $1 }
  ' "$stage/SHA256SUMS")"
  [ "${#expected}" -eq 64 ] || { echo "Missing or ambiguous checksum for $asset" >&2; exit 1; }
  case "$expected" in *[!0-9a-fA-F]*) echo "Invalid checksum for $asset" >&2; exit 1 ;; esac
  if command -v sha256sum >/dev/null 2>&1; then
    actual="$(sha256sum "$stage/$asset" | awk '{print $1}')"
  elif command -v shasum >/dev/null 2>&1; then
    actual="$(shasum -a 256 "$stage/$asset" | awk '{print $1}')"
  else
    echo 'A SHA-256 utility (sha256sum or shasum) is required' >&2; exit 1
  fi
  [ "$actual" = "$expected" ] || { echo "Checksum verification failed for $asset" >&2; exit 1; }
  listing="$(tar -tvzf "$stage/$asset")"
  printf '%s\n' "$listing" | awk 'substr($0,1,1) != "-" && substr($0,1,1) != "d" { exit 1 }' || {
    echo 'Release archives must contain only regular files and directories' >&2; exit 1
  }
}

launcher_asset="ths-$target.tar.gz"
fetch_verified "$launcher_asset"
members="$(tar -tzf "$stage/$launcher_asset")"
[ "$members" = ths ] || { echo 'Launcher archive contains unexpected files' >&2; exit 1; }
tar -xzf "$stage/$launcher_asset" -C "$stage" ths
chmod 755 "$stage/ths"
installed_version="$("$stage/ths" --version | awk '{print $2}')"
[ "$installed_version" = "${tag#v}" ] || {
  echo "Downloaded launcher is $installed_version, expected ${tag#v}" >&2; exit 1
}

if [ "$install_runtime" = native ]; then
  runtime_asset="ths-runtime-$target.tar.gz"
  fetch_verified "$runtime_asset"
  members="$(tar -tzf "$stage/$runtime_asset")"
  printf '%s\n' "$members" | awk '
    {
      name=$0; sub(/\/$/, "", name)
      if (name !~ /^ths-runtime(\/|$)/ || name ~ /(^|\/)\.\.?($|\/)/ ||
          name ~ /[^A-Za-z0-9_.\/@+-]/ || seen[name]++) exit 1
    }
  ' || { echo 'Native archive contains unsafe or duplicate paths' >&2; exit 1; }
  [ -n "$members" ] || { echo 'Native archive is empty' >&2; exit 1; }
  tar -xzf "$stage/$runtime_asset" -C "$stage"
  "$stage/ths" doctor --native --runtime-dir "$stage/ths-runtime"
  runtime_root="$destination/.ths-runtime"
  [ ! -L "$runtime_root" ] || { echo 'Runtime installation directory must not be a symbolic link' >&2; exit 1; }
  mkdir -p "$runtime_root"
  runtime_destination="$runtime_root/$installed_version"
  if [ -e "$runtime_destination" ] || [ -L "$runtime_destination" ]; then
    [ -d "$runtime_destination" ] && [ ! -L "$runtime_destination" ] || {
      echo "Invalid existing native runtime: $runtime_destination" >&2; exit 1
    }
    [ -z "$(find "$runtime_destination" ! -type f ! -type d -print)" ] || {
      echo 'Existing native runtime contains links or special files' >&2; exit 1
    }
    if ! diff -r "$stage/ths-runtime" "$runtime_destination" > "$stage/runtime-diff"; then
      echo "Existing runtime $runtime_destination differs from the release; it was not overwritten. Move it aside after stopping its instances, then reinstall." >&2
      exit 1
    fi
    "$stage/ths" doctor --native --runtime-dir "$runtime_destination"
  else
    # Commit immutable runtime first. The old launcher stays usable if interrupted.
    mv "$stage/ths-runtime" "$runtime_destination"
  fi
elif [ "$install_runtime" = docker ] && [ "${THS_SKIP_IMAGE_PULL:-0}" != 1 ]; then
  "$stage/ths" pull
fi

mv -f "$stage/ths" "$destination/ths"
echo "Installed ths $installed_version to $destination ($install_runtime runtime)"
