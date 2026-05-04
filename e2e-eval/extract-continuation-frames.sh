#!/usr/bin/env bash
# Extract continuation frames from prompt journals in an SRE hard results dir
# Usage: ./e2e-eval/extract-continuation-frames.sh <results-dir>
set -euo pipefail

RESULTS_DIR="${1:?Usage: $0 <results-dir>}"
REPORT="$RESULTS_DIR/continuation-frames.md"

echo "# Continuation Frames Report" > "$REPORT"
echo "Generated: $(date -u +%Y-%m-%dT%H:%M:%SZ)" >> "$REPORT"
echo "" >> "$REPORT"

for model_dir in "$RESULTS_DIR"/iter-1/*; do
  [ -d "$model_dir" ] || continue
  model=$(basename "$model_dir")
  
  for sse_file in "$model_dir"/*.sse; do
    [ -f "$sse_file" ] || continue
    label=$(basename "$sse_file" .sse)
    
    # Find the session dir from the SSE metadata
    session_id=$(grep -o '"chat_session_id":"[^"]*"' "$sse_file" 2>/dev/null | head -1 | cut -d'"' -f4)
    [ -z "$session_id" ] && continue
    
    # Find the latest run's prompt journal
    journal="/tmp/aura-sre-hard-e2e/$session_id/latest/prompt-journal.md"
    [ -f "$journal" ] || continue
    
    # Extract continuation phases (post-execute decision points)
    continuation=$(awk '/PHASE: Planning.*Attempt/{found=1} found && /── USER MESSAGE/{p=1; next} p && /^════/{p=0; found=0; print "---"} p{print}' "$journal")
    
    if [ -n "$continuation" ]; then
      echo "## $model / $label" >> "$REPORT"
      echo '```' >> "$REPORT"
      echo "$continuation" >> "$REPORT"
      echo '```' >> "$REPORT"
      echo "" >> "$REPORT"
    fi
  done
done

echo "Wrote: $REPORT"
