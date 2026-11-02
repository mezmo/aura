#!/usr/bin/env bash
# Check that every metric name a Grafana dashboard queries is an instrument
# `aura::metrics` declares, under the Prometheus name it scrapes as.
#
#   deployment/grafana/check-metric-names.sh [dashboard.json] [metrics.rs]
#
# The catalogue is read from the instrument declarations in
# crates/aura/src/metrics.rs and translated the way opentelemetry-prometheus
# does: dots to underscores, `_seconds` for unit `s`, `_total` on counters.
# Exit 1 lists the names the dashboard queries that no instrument declares.
set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
dashboard=${1:-$here/aura-overview.json}
source=${2:-$here/../../crates/aura/src/metrics.rs}

declared=$(awk '
  match($0, /\.(f64|u64|i64)_(histogram|counter|up_down_counter|gauge)\("[a-z0-9_.]+"\)/) {
    decl = substr($0, RSTART, RLENGTH)
    kind = decl; sub(/^\.(f64|u64|i64)_/, "", kind); sub(/\(.*/, "", kind)
    name = decl; sub(/^[^"]*"/, "", name); sub(/".*/, "", name)
    gsub(/\./, "_", name)
    pending = name; pending_kind = kind; next
  }
  pending != "" && /\.with_unit\("s"\)/ { pending = pending "_seconds" }
  pending != "" && /\.build\(\)/ {
    if (pending_kind == "counter") pending = pending "_total"
    print pending; pending = ""
  }
' "$source" | sort -u)

# A metric name in PromQL is the identifier a label selector follows.
queried=$(jq -r '.. | objects | select(has("expr")) | .expr' "$dashboard" \
  | grep -oE '\b(aura|gen_ai|mcp|http)_[a-z0-9_]+\{' \
  | sed -E 's/\{$//; s/_(bucket|sum|count)$//' \
  | sort -u)

missing=$(comm -23 <(printf '%s\n' "$queried") <(printf '%s\n' "$declared"))
if [[ -n $missing ]]; then
  echo "metric names queried by $dashboard that $source does not declare:" >&2
  printf '  %s\n' $missing >&2
  exit 1
fi
echo "ok: $(printf '%s\n' "$queried" | wc -l) metric names in $dashboard, all declared in $source ($(printf '%s\n' "$declared" | wc -l) instruments)"
