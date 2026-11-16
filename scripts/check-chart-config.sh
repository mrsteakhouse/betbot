#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
bin="${BETBOT_BIN:-target/debug/betbot}"
tmp="$(mktemp -d)"; trap 'rm -rf "$tmp"' EXIT
for values in charts/betbot/ci/*-values.yaml; do
  helm template betbot charts/betbot -f "$values" --show-only templates/configmap.yaml \
    | yq -r '.data["config.yaml"]' > "$tmp/config.yaml"
  "$bin" --config "$tmp/config.yaml" --check-config
  echo "ok: $values"
done
