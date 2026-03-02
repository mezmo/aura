#!/bin/bash
# Benchmark MCP connection time vs tool discovery time
#
# Usage: ./scripts/benchmark_mcp_timing.sh [MCP_URL]
#
# Default: http://localhost:9999/mcp

set -e

MCP_URL="${1:-http://localhost:9999/mcp}"

echo "=== MCP Timing Benchmark ==="
echo ""
echo "Target: $MCP_URL"
echo ""

# Common headers for MCP streamable HTTP
HEADERS="-H 'Content-Type: application/json' -H 'Accept: application/json, text/event-stream'"

# Test 1: Raw HTTP timing (baseline)
echo "Test 1: Raw HTTP POST (baseline)"
HTTP_TIME=$(curl -w "%{time_total}" -s -o /dev/null -X POST "$MCP_URL" \
    -H "Content-Type: application/json" \
    -H "Accept: application/json, text/event-stream" \
    -d '{"jsonrpc":"2.0","method":"ping","id":1}' 2>&1)
echo "  HTTP roundtrip: ${HTTP_TIME}s"

# Test 2: MCP Initialize handshake
echo ""
echo "Test 2: MCP Initialize Handshake"
INIT_START=$(python3 -c 'import time; print(time.time())')
INIT_RESP=$(curl -s -X POST "$MCP_URL" \
    -H "Content-Type: application/json" \
    -H "Accept: application/json, text/event-stream" \
    -d '{
        "jsonrpc": "2.0",
        "method": "initialize",
        "id": 1,
        "params": {
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": {
                "name": "timing-benchmark",
                "version": "0.1.0"
            }
        }
    }' 2>&1)
INIT_END=$(python3 -c 'import time; print(time.time())')
INIT_TIME=$(python3 -c "print(f'{$INIT_END - $INIT_START:.3f}')")
echo "  Initialize time: ${INIT_TIME}s"
# Check for error
if echo "$INIT_RESP" | grep -q '"error"'; then
    echo "  Error: $INIT_RESP"
else
    # Extract server name
    SERVER_NAME=$(echo "$INIT_RESP" | python3 -c "import sys,json; data=json.load(sys.stdin); print(data.get('result',{}).get('serverInfo',{}).get('name','?'))" 2>/dev/null || echo "?")
    echo "  Server: $SERVER_NAME"
fi

# Test 3: MCP tools/list (tool discovery)
echo ""
echo "Test 3: MCP tools/list (tool discovery)"
LIST_START=$(python3 -c 'import time; print(time.time())')
LIST_RESP=$(curl -s -X POST "$MCP_URL" \
    -H "Content-Type: application/json" \
    -H "Accept: application/json, text/event-stream" \
    -d '{
        "jsonrpc": "2.0",
        "method": "tools/list",
        "id": 2,
        "params": {}
    }' 2>&1)
LIST_END=$(python3 -c 'import time; print(time.time())')
LIST_TIME=$(python3 -c "print(f'{$LIST_END - $LIST_START:.3f}')")
echo "  tools/list time: ${LIST_TIME}s"

# Count tools
TOOL_COUNT=$(echo "$LIST_RESP" | python3 -c "import sys,json; data=json.load(sys.stdin); print(len(data.get('result',{}).get('tools',[])))" 2>/dev/null || echo "?")
echo "  Tools discovered: $TOOL_COUNT"

# Test 4: Combined timing with curl's built-in metrics
echo ""
echo "Test 4: Detailed timing breakdown (initialize)"
echo "  (dns_lookup, connect, tls, transfer)"
curl -w "  DNS: %{time_namelookup}s, Connect: %{time_connect}s, TLS: %{time_appconnect}s, Total: %{time_total}s\n" \
    -s -o /dev/null -X POST "$MCP_URL" \
    -H "Content-Type: application/json" \
    -H "Accept: application/json, text/event-stream" \
    -d '{"jsonrpc":"2.0","method":"initialize","id":1,"params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"bench","version":"0.1.0"}}}'

# Test 5: Multiple sequential requests to show connection reuse
echo ""
echo "Test 5: Sequential requests (connection reuse)"
for i in 1 2 3; do
    REQ_TIME=$(curl -w "%{time_total}" -s -o /dev/null -X POST "$MCP_URL" \
        -H "Content-Type: application/json" \
        -H "Accept: application/json, text/event-stream" \
        -d '{"jsonrpc":"2.0","method":"tools/list","id":'$i',"params":{}}' 2>&1)
    echo "  Request $i: ${REQ_TIME}s"
done

# Summary
echo ""
echo "=== Summary ==="
echo "HTTP baseline:  ${HTTP_TIME}s"
echo "Initialize:     ${INIT_TIME}s"
echo "tools/list:     ${LIST_TIME}s"
TOTAL=$(python3 -c "print(f'{float($INIT_TIME) + float($LIST_TIME):.3f}')")
echo "Total:          ${TOTAL}s"
echo ""
echo "=== Analysis ==="
echo "Connection overhead: ~${HTTP_TIME}s (TCP + TLS)"
echo "MCP handshake cost:  ~${INIT_TIME}s (initialize RPC)"
echo "Tool discovery cost: ~${LIST_TIME}s (tools/list RPC)"
echo ""
echo "What caching DOES save:"
echo "  - tools/list RPC: ~${LIST_TIME}s per server"
echo ""
echo "What caching does NOT save:"
echo "  - TCP connection: ~${HTTP_TIME}s"
echo "  - MCP initialize: ~${INIT_TIME}s"
echo ""
echo "For remote servers (network latency):"
echo "  - Add ~50-200ms RTT per RPC call"
echo "  - Initialize + tools/list = 2 RTTs"
echo "  - With cache: Initialize only = 1 RTT"
