#!/bin/sh
# Stand-in for bwrap in unit tests: reads the NUL-separated argument list from
# `--args 9` like the real thing, applies --setenv, and runs the command
# unconfined. Behaviour depends on the file name:
#   fake-bwrap.sh        passes nothing (the isolation probe must reject it)
#   fake-bwrap-probe.sh  also answers the isolation probe
#   fake-bwrap-old.sh    like -probe, but rejects --disable-userns
name=$(basename "$0")
[ "$1" = "--args" ] && [ "$3" = "--" ] || { echo "fake bwrap: expected --args N --" >&2; exit 2; }
shift 3
tmp=$(mktemp) || exit 2
tr '\0' '\n' <&9 >"$tmp"
mode=opts
key=
while IFS= read -r line; do
  case $mode in
    opts)
      case $line in
        --setenv) mode=setenv_k ;;
        --disable-userns)
          if [ "$name" = "fake-bwrap-old.sh" ]; then
            rm -f "$tmp"
            echo "bwrap: Unknown option --disable-userns" >&2
            exit 1
          fi ;;
      esac ;;
    setenv_k) key=$line; mode=setenv_v ;;
    setenv_v) export "$key=$line"; mode=opts ;;
  esac
done <"$tmp"
rm -f "$tmp"
if [ "$name" != "fake-bwrap.sh" ] && [ "$1" = "/bin/sh" ]; then
  case $3 in *"echo isolated"*) echo isolated; exit 0 ;; esac
fi
exec "$@"
