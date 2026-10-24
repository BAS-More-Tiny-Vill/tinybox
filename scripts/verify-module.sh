#!/usr/bin/env bash
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)"
archive="" verify_uid="" verify_gid="" verifier_bin=""
while (($#)); do
  case "$1" in
    --archive) archive="${2:?missing archive path}"; shift 2 ;;
    --uid) verify_uid="${2:?missing verifier uid}"; shift 2 ;;
    --gid) verify_gid="${2:?missing verifier gid}"; shift 2 ;;
    --verifier-bin) verifier_bin="${2:?missing verifier binary}"; shift 2 ;;
    *)
      if [[ -z "$archive" ]]; then archive="$1"; shift
      else echo "unknown argument: $1" >&2; exit 2
      fi
      ;;
  esac
done
[[ -n "$archive" ]] || {
  echo "usage: verify-module.sh --archive <module-archive> [--uid UID --gid GID] [--verifier-bin PATH]" >&2
  exit 2
}
[[ -z "$verify_uid" && -z "$verify_gid" || "$verify_uid" =~ ^[0-9]+$ && "$verify_gid" =~ ^[0-9]+$ ]] || {
  echo "verifier uid and gid must be supplied together as non-negative integers" >&2
  exit 2
}
if [[ -n "$verifier_bin" && "$verifier_bin" != /* ]]; then
  verifier_bin="$PWD/$verifier_bin"
fi
archive="$(cd "$(dirname "$archive")" && pwd -P)/$(basename "$archive")"
cd "$root"
target_root="${CARGO_TARGET_DIR:-$root/target}"
[[ "$target_root" == /* ]] || target_root="$root/$target_root"
mkdir -p "$target_root"
target_root="$(cd "$target_root" && pwd -P)"
[[ "$target_root" == "$root"/* ]] || { echo "CARGO_TARGET_DIR must stay inside the repository" >&2; exit 2; }
work="$target_root/tinybox-module-verify-$$"
rm -rf "$work"
mkdir -p "$work"
chmod 700 "$work"
trap 'rm -rf "$work"' EXIT
case "$archive" in
  *.tar.gz)
    members="$(tar -tzf "$archive")"
    while IFS= read -r member; do
      case "$member" in
        /*|../*|*/../*|*/..) echo "archive member escapes the extraction directory: $member" >&2; exit 2 ;;
      esac
    done <<<"$members"
    if tar -tvzf "$archive" | awk 'substr($0, 1, 1) !~ /^[-d]$/ { exit 1 }'; then
      tar -xzf "$archive" -C "$work"
    else
      echo "archive contains a link or special file" >&2
      exit 2
    fi
    ;;
  *) echo "unsupported module archive: $archive" >&2; exit 2 ;;
esac
case "$(uname -s)" in
  Darwin) library="$work/libtinybox.dylib" ;;
  Linux) library="$work/libtinybox.so" ;;
  *) echo "verify-module.sh requires a Unix runner" >&2; exit 1 ;;
esac
test -f "$work/modules.toml"
if [[ -z "$verifier_bin" ]]; then
  cargo build --locked --release --manifest-path Cargo.toml --package tinybox-module --example verify_module
  verifier_bin="$target_root/release/examples/verify_module"
fi
[[ -x "$verifier_bin" ]] || { echo "module verifier is not executable: $verifier_bin" >&2; exit 2; }
if [[ -n "$verify_uid" ]]; then
  command -v setpriv >/dev/null 2>&1 || { echo "setpriv is required for uid-aware module verification" >&2; exit 2; }
  chown "$verify_uid:$verify_gid" "$work"
  setpriv --reuid "$verify_uid" --regid "$verify_gid" --clear-groups -- "$verifier_bin" "$library"
else
  "$verifier_bin" "$library"
fi
