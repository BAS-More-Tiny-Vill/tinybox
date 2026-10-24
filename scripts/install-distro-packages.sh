#!/usr/bin/env bash
set -euo pipefail

if command -v dnf >/dev/null 2>&1; then
  dnf install -y gcc gcc-c++ cmake curl git gzip jq make openssl-devel perl tar util-linux
elif command -v pacman >/dev/null 2>&1; then
  # The distro builder runs in a nested container where Landlock cannot be
  # applied. Keep pacman's syscall sandbox enabled and disable only filesystem
  # sandboxing for package downloads.
  pacman -Syu --disable-sandbox-filesystem --noconfirm base-devel cmake curl git gzip jq openssl perl util-linux
else
  echo "unsupported distro package manager" >&2
  exit 2
fi
