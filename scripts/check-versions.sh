#!/usr/bin/env bash
# Checks that all version strings in the repo agree:
# - rust-toolchain.toml channel == Dockerfile builder image tag
# - Cargo.toml == Cargo.lock == Chart.yaml appVersion == Chart.yaml version
# - optionally, all of the above equal the version given as $1
set -euo pipefail
cd "$(dirname "$0")/.."

tc="$(sed -n 's/^channel *= *"\(.*\)"/\1/p' rust-toolchain.toml)"
df="$(sed -n 's/^FROM rust:\([0-9][0-9.]*\)-.* AS builder$/\1/p' Dockerfile)"
cargo="$(sed -n 's/^version *= *"\([^"]*\)".*/\1/p' Cargo.toml | head -n 1)"
lock="$(awk '
  /^name = "betbot"$/ { found = 1; next }
  found && /^version = / { gsub(/^version = "|"$/, ""); print; exit }
' Cargo.lock)"
app="$(sed -n 's/^appVersion: *"\([^"]*\)".*/\1/p' charts/betbot/Chart.yaml)"
chart="$(sed -n 's/^version: *\([0-9][0-9.A-Za-z+-]*\).*/\1/p' charts/betbot/Chart.yaml)"

fail=0
check() { # <what> <a> <b>
  if [[ "$2" != "$3" ]]; then
    echo "FAIL: $1 ($2 != $3)"
    fail=1
  fi
}

for pair in "rust-toolchain.toml channel:$tc" "Dockerfile rust builder tag:$df" \
  "Cargo.toml version:$cargo" "Cargo.lock betbot version:$lock" \
  "Chart.yaml appVersion:$app" "Chart.yaml version:$chart"; do
  if [[ -z "${pair##*:}" ]]; then
    echo "FAIL: ${pair%:*} not found"
    fail=1
  fi
done

check "rust-toolchain.toml channel vs Dockerfile rust builder tag" "$tc" "$df"
check "Cargo.toml vs Cargo.lock" "$cargo" "$lock"
check "Cargo.toml vs Chart.yaml appVersion" "$cargo" "$app"
check "Cargo.toml vs Chart.yaml version" "$cargo" "$chart"

if [[ $# -ge 1 ]]; then
  check "Cargo.toml vs expected" "$cargo" "$1"
  check "Cargo.lock vs expected" "$lock" "$1"
  check "Chart.yaml appVersion vs expected" "$app" "$1"
  check "Chart.yaml version vs expected" "$chart" "$1"
fi

if [[ "$fail" -eq 0 ]]; then
  echo ok
fi
exit "$fail"
