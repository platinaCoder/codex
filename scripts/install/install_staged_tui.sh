#!/usr/bin/env bash

set -euo pipefail

usage() {
  cat <<'EOF'
Usage: install_staged_tui.sh --destination PATH [--source PATH] [--install]

Validates the staged codex-tui binary and prints the planned atomic replacement.
Nothing is installed unless --install is supplied. Stop any Codex process using
the destination before running with --install.
EOF
}

die() {
  printf 'ERROR: %s\n' "$*" >&2
  exit 1
}

script_dir="$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)"
repository_root="$(CDPATH= cd -- "$script_dir/../.." && pwd)"
source_path="$repository_root/codex-rs/target/debug/codex-tui"
destination_path=""
install_requested=0

while (($#)); do
  case "$1" in
    --source)
      (($# >= 2)) || die "--source requires a value"
      source_path="$2"
      shift 2
      ;;
    --destination)
      (($# >= 2)) || die "--destination requires a value"
      destination_path="$2"
      shift 2
      ;;
    --install)
      install_requested=1
      shift
      ;;
    --help|-h)
      usage
      exit 0
      ;;
    *)
      die "unknown argument: $1"
      ;;
  esac
done

[[ -n "$destination_path" ]] || die "--destination is required"
source_path="$(realpath "$source_path")"
[[ -f "$source_path" && -x "$source_path" ]] || die "staged binary is not executable: $source_path"
destination_parent="$(dirname -- "$destination_path")"
[[ -d "$destination_parent" ]] || die "destination directory does not exist: $destination_parent"
destination_path="$(realpath -m "$destination_path")"
[[ "$source_path" != "$destination_path" ]] || die "source and destination are identical"

version="$($source_path --version)"
checksum="$(sha256sum "$source_path" | awk '{print $1}')"
printf 'Staged candidate verified:\n'
printf '  source:      %s\n' "$source_path"
printf '  destination: %s\n' "$destination_path"
printf '  version:     %s\n' "$version"
printf '  sha256:      %s\n' "$checksum"

if ((install_requested == 0)); then
  printf '\nValidation only. Re-run with --install after stopping the destination process.\n'
  exit 0
fi

temporary_path="$(mktemp "$destination_parent/.codex-tui.ticket-9.XXXXXX")"
cleanup() {
  [[ ! -e "$temporary_path" ]] || rm -f -- "$temporary_path"
}
trap cleanup EXIT HUP INT TERM
install -m 0755 "$source_path" "$temporary_path"
mv -Tf "$temporary_path" "$destination_path"
trap - EXIT HUP INT TERM
printf 'Installed staged TUI atomically at %s\n' "$destination_path"
