#!/usr/bin/env sh
set -eu

die() {
    printf '%s\n' "install.sh: $*" >&2
    exit 1
}

usage() {
    printf '%s\n' "Usage: ./install.sh [BIN_DIR]" \
        "       ./install.sh --bin-dir BIN_DIR" \
        "Default BIN_DIR: \$HOME/.local/bin"
}

bin_dir=${MCP_POOL_BIN_DIR:-"$HOME/.local/bin"}
case "$#" in
    0) ;;
    1)
        case "$1" in
            -h|--help)
                usage
                exit 0
                ;;
            *)
                bin_dir=$1
                ;;
        esac
        ;;
    2)
        [ "$1" = "--bin-dir" ] || die "expected one BIN_DIR or --bin-dir BIN_DIR"
        bin_dir=$2
        ;;
    *)
        die "expected one BIN_DIR or --bin-dir BIN_DIR"
        ;;
esac

repo_root=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd -P) ||
    die "could not resolve the repository directory"
manifest_path=$repo_root/Cargo.toml
target_dir=$repo_root/target
mkdir -p "$bin_dir" || die "could not create install directory: $bin_dir"
bin_dir=$(CDPATH= cd -- "$bin_dir" && pwd -P) ||
    die "could not resolve install directory: $bin_dir"

host_target=$(rustc -vV 2>/dev/null | sed -n 's/^host: //p')
[ -n "$host_target" ] ||
    die "could not determine the Rust host target; the existing executable was not changed"
source_path=$target_dir/$host_target/release/mcp-pool
destination_path=$bin_dir/mcp-pool

if ! (
    cd "$repo_root" &&
        cargo build \
            --manifest-path "$manifest_path" \
            --target-dir "$target_dir" \
            --target "$host_target" \
            --release \
            --locked
); then
    die "cargo build --release --locked failed; the existing executable was not changed"
fi
[ -f "$source_path" ] || die "release executable was not produced at $source_path"
[ -x "$source_path" ] || die "release executable is not executable: $source_path"

built_version=$("$source_path" --version) ||
    die "release executable did not report a version; the existing executable was not changed"
printf 'Built version: %s\nBuild target: %s\nBuild target directory: %s\n' \
    "$built_version" "$host_target" "$target_dir"

if [ -L "$destination_path" ]; then
    die "install destination is a symbolic link; refusing to replace it: $destination_path"
fi
if [ -e "$destination_path" ]; then
    [ -f "$destination_path" ] ||
        die "install destination is not a regular file: $destination_path"
    [ -w "$destination_path" ] ||
        die "install destination is unwritable; the existing executable was retained: $destination_path"
fi

temporary_path=$(mktemp "$bin_dir/.mcp-pool.install.XXXXXX") ||
    die "could not create a temporary install file in $bin_dir"
cleanup() {
    if [ -n "${temporary_path:-}" ] && [ -e "$temporary_path" ]; then
        rm -f "$temporary_path"
    fi
}
trap cleanup 0
trap 'exit 1' HUP INT TERM

cp "$source_path" "$temporary_path" ||
    die "could not copy the release executable; the existing executable was retained"
file_sha256() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | awk '{print $1}'
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$1" | awk '{print $1}'
    else
        die "sha256sum or shasum is required to verify the staged executable"
    fi
}
source_hash=$(file_sha256 "$source_path") ||
    die "could not hash the release executable; the existing executable was retained"
temporary_hash=$(file_sha256 "$temporary_path") ||
    die "could not hash the staged executable; the existing executable was retained"
[ "$source_hash" = "$temporary_hash" ] ||
    die "staged executable hash did not match the build artifact; the existing executable was retained"
if mode=$(stat -c '%a' "$source_path" 2>/dev/null); then
    :
elif mode=$(stat -f '%Lp' "$source_path" 2>/dev/null); then
    :
else
    die "could not read executable permissions; the existing executable was retained"
fi
chmod "$mode" "$temporary_path" ||
    die "could not preserve executable permissions; the existing executable was retained"
[ -x "$temporary_path" ] ||
    die "temporary executable lost executable permissions; the existing executable was retained"

temporary_version=$("$temporary_path" --version) ||
    die "temporary executable did not report a version; the existing executable was retained"
[ "$temporary_version" = "$built_version" ] ||
    die "temporary executable version did not match the build artifact; the existing executable was retained"
printf 'Verified version: %s\n' "$temporary_version"

mv -f "$temporary_path" "$destination_path" ||
    die "could not replace the executable; the existing executable was retained: $destination_path"
temporary_path=
trap - 0
trap - HUP INT TERM

installed_version=$("$destination_path" --version) ||
    die "installed executable could not report its version: $destination_path"
printf 'Installed executable: %s\nInstalled version: %s\n' \
    "$destination_path" "$installed_version"

selected_command=$(command -v mcp-pool 2>/dev/null || true)
if [ -z "$selected_command" ]; then
    printf '%s\n' "install.sh: warning: mcp-pool is not currently on PATH; no PATH changes were made." >&2
elif [ "$selected_command" != "$destination_path" ]; then
    printf '%s\n' \
        "install.sh: warning: PATH mismatch: selected '$selected_command', installed '$destination_path'." >&2
fi
