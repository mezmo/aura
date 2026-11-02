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

# Attribute keys the module declares, as Prometheus label names, plus the
# labels Prometheus itself adds. These are the identifiers in an expression
# that look like metric names but are not.
labels=$(awk '
  match($0, /const [A-Z_]+: &str = "[a-z0-9_.]+"/) {
    name = substr($0, RSTART, RLENGTH); sub(/^[^"]*"/, "", name); sub(/".*/, "", name)
    gsub(/\./, "_", name); print name
  }
' "$source"; printf 'le\ninstance\njob\n')

# Every metric-shaped identifier in every expression, minus the label names.
# A histogram or summary suffix is tried as part of the name first, then
# folded back onto the declared name.
missing=$(jq -r '.. | objects | select(has("expr")) | .expr' "$dashboard" \
  | grep -oE '\b(aura|gen_ai|mcp|http)_[a-z0-9_]+' \
  | sort -u \
  | comm -23 - <(printf '%s\n' "$labels" | sort -u) \
  | while read -r name; do
      stem=$(sed -E 's/_(bucket|sum|count)$//' <<<"$name")
      grep -qxF "$name" <<<"$declared" || grep -qxF "$stem" <<<"$declared" || echo "$name"
    done)
if [[ -n $missing ]]; then
  echo "metric names queried by $dashboard that $source does not declare:" >&2
  printf '  %s\n' $missing >&2
  exit 1
fi
queried=$(jq -r '.. | objects | select(has("expr")) | .expr' "$dashboard" \
  | grep -oE '\b(aura|gen_ai|mcp|http)_[a-z0-9_]+' | sed -E 's/_(bucket|sum|count)$//' | sort -u \
  | comm -23 - <(printf '%s\n' "$labels" | sort -u) | wc -l)
echo "ok: $queried metric names in $dashboard, all declared in $source ($(printf '%s\n' "$declared" | wc -l) instruments)"
