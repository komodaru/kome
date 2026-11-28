#!/usr/bin/env bash
# Valgrind を入れる (操作ごとの命令の数を Cachegrind で数えるため。docs/bench.md)。
# 数は Valgrind の版に左右されないので、版は固定しない。
set -euo pipefail
if command -v valgrind >/dev/null; then
    valgrind --version
    exit 0
fi
sudo apt-get update -q
sudo apt-get install -y -q --no-install-recommends valgrind
valgrind --version
