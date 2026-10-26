#!/usr/bin/env bash

set -euo pipefail

usage() {
  cat <<'EOF'
Usage:
  promote_local_package.sh \
    --package-dir DIR \
    --release-name NAME \
    --checksums FILE \
    [--codex-home DIR] \
    [--bin-dir DIR] \
    [--promote]

Without --promote, validate the candidate and print the planned changes.

With --promote, atomically select the standalone package and schedule the
daemon package update in a detached user-systemd unit. The daemon update may
disconnect active Codex sessions, but the detached unit continues running.
EOF
}

die() {
  printf 'ERROR: %s\n' "$*" >&2
  exit 1
}

package_dir=""
release_name=""
checksums_file=""
codex_home_dir="${CODEX_HOME:-${HOME}/.codex}"
bin_dir="${CODEX_INSTALL_DIR:-${HOME}/.local/bin}"
promote=0
staging_dir=""

while (($#)); do
  case "$1" in
    --package-dir)
      (($# >= 2)) || die "--package-dir requires a value"
      package_dir="$2"
      shift 2
      ;;
    --release-name)
      (($# >= 2)) || die "--release-name requires a value"
      release_name="$2"
      shift 2
      ;;
    --checksums)
      (($# >= 2)) || die "--checksums requires a value"
      checksums_file="$2"
      shift 2
      ;;
    --codex-home)
      (($# >= 2)) || die "--codex-home requires a value"
      codex_home_dir="$2"
      shift 2
      ;;
    --bin-dir)
      (($# >= 2)) || die "--bin-dir requires a value"
      bin_dir="$2"
      shift 2
      ;;
    --promote)
      promote=1
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

[[ -n "$package_dir" ]] || die "--package-dir is required"
[[ -n "$release_name" ]] || die "--release-name is required"
[[ -n "$checksums_file" ]] || die "--checksums is required"
[[ "$release_name" =~ ^[A-Za-z0-9][A-Za-z0-9._-]*$ ]] || die "invalid release name"

command -v python3 >/dev/null || die "python3 is required"
command -v sha256sum >/dev/null || die "sha256sum is required"
command -v flock >/dev/null || die "flock is required"
command -v systemd-run >/dev/null || die "systemd-run is required"

package_dir="$(realpath "$package_dir")"
checksums_file="$(realpath "$checksums_file")"
[[ -d "$package_dir" ]] || die "package directory does not exist: $package_dir"
[[ -f "$checksums_file" ]] || die "checksums file does not exist: $checksums_file"
[[ -f "$package_dir/codex-package.json" ]] || die "missing codex-package.json"
[[ -x "$package_dir/bin/codex" ]] || die "missing executable bin/codex"
[[ -x "$package_dir/bin/codex-code-mode-host" ]] || \
  die "missing executable bin/codex-code-mode-host"

mapfile -t package_metadata < <(
  python3 - "$package_dir/codex-package.json" <<'PY'
import json
import pathlib
import sys

manifest = json.loads(pathlib.Path(sys.argv[1]).read_text())
for key in ("layoutVersion", "version", "target", "variant", "entrypoint"):
    print(manifest.get(key, ""))
PY
)
[[ "${package_metadata[0]:-}" == "1" ]] || die "unsupported package layout"
package_version="${package_metadata[1]:-}"
package_target="${package_metadata[2]:-}"
[[ "${package_metadata[3]:-}" == "codex" ]] || die "candidate is not a Codex CLI package"
[[ "${package_metadata[4]:-}" == "bin/codex" ]] || die "unexpected package entrypoint"
[[ "$release_name" == *"-$package_target" ]] || \
  die "release name must end in package target $package_target"

found_codex=0
found_host=0
while read -r digest relative_path extra; do
  [[ -n "${digest:-}" ]] || continue
  [[ -z "${extra:-}" ]] || die "invalid checksum line for $relative_path"
  relative_path="${relative_path#\*}"
  [[ "$digest" =~ ^[0-9a-fA-F]{64}$ ]] || die "invalid SHA-256 digest"
  case "$relative_path" in
    /*|..|../*|*/../*|*/..)
      die "checksum path escapes the package: $relative_path"
      ;;
  esac
  [[ -f "$package_dir/$relative_path" ]] || die "checksummed file is missing: $relative_path"
  [[ "$relative_path" == "bin/codex" ]] && found_codex=1
  [[ "$relative_path" == "bin/codex-code-mode-host" ]] && found_host=1
done < "$checksums_file"
((found_codex == 1)) || die "checksums do not include bin/codex"
((found_host == 1)) || die "checksums do not include bin/codex-code-mode-host"
(
  cd "$package_dir"
  sha256sum --strict --check "$checksums_file"
)

reported_version="$($package_dir/bin/codex --version)"
[[ "$reported_version" == "codex-cli $package_version" ]] || \
  die "binary version '$reported_version' does not match package version '$package_version'"

standalone_root="$codex_home_dir/packages/standalone"
releases_dir="$standalone_root/releases"
current_link="$standalone_root/current"
release_dir="$releases_dir/$release_name"
visible_codex="$bin_dir/codex"
auto_update_backup="$standalone_root/auto-update-version.before-$release_name"

if [[ -e "$visible_codex" && ! -L "$visible_codex" ]]; then
  die "refusing to replace non-symlink command: $visible_codex"
fi
if [[ -f "$standalone_root/auto-update-version" && -e "$auto_update_backup" ]]; then
  die "auto-update backup already exists: $auto_update_backup"
fi

printf 'Candidate verified:\n'
printf '  package: %s\n' "$package_dir"
printf '  release: %s\n' "$release_dir"
printf '  target:  %s\n' "$package_target"
printf '  version: %s\n' "$package_version"
if ((promote == 0)); then
  printf '\nValidation only. Re-run with --promote to install it.\n'
  exit 0
fi

mkdir -p "$releases_dir" "$bin_dir"
exec 9>"$standalone_root/install.lock"
flock -x 9

cleanup() {
  if [[ -n "$staging_dir" && -d "$staging_dir" ]]; then
    rm -rf -- "$staging_dir"
  fi
}
trap cleanup EXIT HUP INT TERM

if [[ -e "$release_dir" || -L "$release_dir" ]]; then
  [[ -d "$release_dir" && ! -L "$release_dir" ]] || \
    die "release destination already exists and is not an ordinary directory"
  (
    cd "$release_dir"
    sha256sum --strict --check "$checksums_file"
  )
  installed_codex_sha="$(sha256sum "$release_dir/bin/codex" | awk '{print $1}')"
  candidate_codex_sha="$(sha256sum "$package_dir/bin/codex" | awk '{print $1}')"
  installed_host_sha="$(sha256sum "$release_dir/bin/codex-code-mode-host" | awk '{print $1}')"
  candidate_host_sha="$(sha256sum "$package_dir/bin/codex-code-mode-host" | awk '{print $1}')"
  [[ "$installed_codex_sha" == "$candidate_codex_sha" ]] || \
    die "existing release has different Codex bytes"
  [[ "$installed_host_sha" == "$candidate_host_sha" ]] || \
    die "existing release has different code-mode host bytes"
  [[ -L "$release_dir/codex" && "$(readlink "$release_dir/codex")" == "bin/codex" ]] || \
    die "existing release has an invalid entrypoint symlink"
else
  staging_dir="$(mktemp -d "$releases_dir/.${release_name}.tmp.XXXXXX")"
  cp -a "$package_dir"/. "$staging_dir"/
  (
    cd "$staging_dir"
    sha256sum --strict --check "$checksums_file"
  )
  ln -s "bin/codex" "$staging_dir/codex"
  mv "$staging_dir" "$release_dir"
  staging_dir=""
fi

previous_dir=""
if [[ -L "$current_link" ]]; then
  previous_dir="$(realpath "$current_link")" || die "current package link is dangling"
elif [[ -e "$current_link" ]]; then
  die "current package selection is not a symlink: $current_link"
fi
if [[ -n "$previous_dir" && "$previous_dir" != "$release_dir" ]]; then
  rollback_link="$standalone_root/current.rollback-before-$release_name"
  if [[ -e "$rollback_link" || -L "$rollback_link" ]]; then
    [[ "$(realpath "$rollback_link")" == "$previous_dir" ]] || \
      die "rollback link already exists with a different target: $rollback_link"
  else
    ln -s "$previous_dir" "$rollback_link"
  fi
fi

if [[ "$previous_dir" != "$release_dir" ]]; then
  temporary_current="$standalone_root/.current.$$"
  ln -s "$release_dir" "$temporary_current"
  mv -Tf "$temporary_current" "$current_link"
  selection_id="$(LC_ALL=C stat -c '%d %i %.9Z' "$current_link")"
  previous_name="${previous_dir##*/}"
  history_tmp="$standalone_root/.selection-history.$$"
  printf '%s\n%s\n%s\n' "$release_name" "$previous_name" "$selection_id" > "$history_tmp"
  mv -f "$history_tmp" "$standalone_root/selection-history"
fi

temporary_visible="$bin_dir/.codex.$$"
ln -s "$current_link/bin/codex" "$temporary_visible"
mv -Tf "$temporary_visible" "$visible_codex"

if [[ -f "$standalone_root/auto-update-version" ]]; then
  mv "$standalone_root/auto-update-version" "$auto_update_backup"
fi

"$visible_codex" --version >/dev/null

timestamp="$(date -u +%Y%m%d%H%M%S)"
unit="codex-ai-lab-promotion-$timestamp-$$"
systemd-run \
  --user \
  --unit="$unit" \
  --description="Promote AI Lab Codex package $release_name" \
  --on-active=3s \
  --setenv="CODEX_HOME=$codex_home_dir" \
  "$release_dir/bin/codex" app-server daemon update --from-cli --yes

printf '\nStandalone package selected successfully.\n'
printf 'Daemon promotion is scheduled as %s.service and may disconnect active sessions.\n' "$unit"
printf 'After reconnecting, inspect it with:\n'
printf '  journalctl --user -u %s.service --no-pager\n' "$unit"
printf '  codex app-server daemon version\n'
