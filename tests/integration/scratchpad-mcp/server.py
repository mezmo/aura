"""
Scratchpad Test FastMCP Server

Returns deterministic, controlled-size outputs for testing scratchpad
context window management: interception thresholds, exploration tools,
context budget tracking, and SSE event emission.

6 tools with configurable output sizes to exercise all 8 scratchpad
exploration tools (head, slice, grep, schema, item_schema, get_in,
iterate_over, read).
"""

import json
import math

from fastmcp import FastMCP

mcp = FastMCP("scratchpad-test-mcp")


# ---------------------------------------------------------------------------
# Deterministic data generators
# ---------------------------------------------------------------------------


def generate_result_item(index: int) -> dict:
    """Generate a single deterministic result item."""
    return {
        "id": f"item-{index:03d}",
        "name": f"Dataset Entry {index}",
        "status": "active" if index % 3 != 0 else "inactive",
        "metadata": {
            "score": round(0.1 * (index % 10) + 0.05, 2),
            "tags": [f"tag-{chr(97 + (index % 5))}", f"category-{(index % 3) + 1}"],
        },
        "details": {
            "region": ["us-east-1", "eu-west-1", "ap-south-1"][index % 3],
            "version": f"v{1 + (index % 4)}.{index % 10}.0",
        },
    }


def generate_log_entry(index: int) -> str:
    """Generate a single deterministic log entry."""
    levels = ["INFO", "INFO", "WARN", "INFO", "ERROR"]
    services = ["auth-service", "data-pipeline", "api-gateway", "scheduler"]
    messages = [
        "Request processed successfully",
        "Connection pool refreshed",
        "High latency detected on upstream",
        "Cache miss ratio above threshold",
        "Failed to connect to downstream service",
        "Retry attempt 2 of 3",
        "Health check passed",
        "Rate limit approaching threshold",
        "Timeout waiting for response",
        "Circuit breaker tripped",
    ]

    level = levels[index % len(levels)]
    service = services[index % len(services)]
    message = messages[index % len(messages)]
    ts_seconds = 1709000000 + index * 15

    return (
        f"2024-02-27T{10 + (ts_seconds // 3600) % 24:02d}:"
        f"{(ts_seconds // 60) % 60:02d}:{ts_seconds % 60:02d}Z "
        f"[{level}] {service}: {message}"
    )


# ---------------------------------------------------------------------------
# Tools
# ---------------------------------------------------------------------------


@mcp.tool()
def sp_get_large_json(size: int = 2000) -> str:
    """Retrieve a large JSON dataset of result items. Size parameter controls
    approximate output size in bytes (default 2000). Returns nested JSON with
    results array, total count, and query metadata."""
    # Each item is roughly 200-250 bytes of JSON
    items_needed = max(1, size // 220)
    results = [generate_result_item(i) for i in range(items_needed)]
    output = {
        "results": results,
        "total": len(results),
        "query": {
            "source": "scratchpad-test",
            "timestamp": "2024-02-27T10:30:00Z",
            "filters_applied": ["status=all", "region=all"],
        },
    }
    return json.dumps(output)


@mcp.tool()
def sp_get_large_text(size: int = 1500) -> str:
    """Retrieve multi-line log entries. Size parameter controls approximate
    output size in bytes (default 1500). Returns timestamped log lines with
    levels (INFO/WARN/ERROR) and service names."""
    # Each log line is roughly 90-110 bytes
    lines_needed = max(1, size // 100)
    lines = [generate_log_entry(i) for i in range(lines_needed)]
    return "\n".join(lines)


@mcp.tool()
def sp_get_small_json(size: int = 0) -> str:
    """Retrieve a small JSON status object. Always returns ~30 bytes,
    designed to pass through below scratchpad interception thresholds.
    The size parameter is accepted for API consistency but ignored."""
    return json.dumps({"status": "ok", "count": 3})


@mcp.tool()
def sp_get_small_text(size: int = 0) -> str:
    """Retrieve a short text status message. Always returns ~25 bytes,
    designed to pass through below scratchpad interception thresholds.
    The size parameter is accepted for API consistency but ignored."""
    return "All systems operational."


@mcp.tool()
def sp_get_boundary_json(target_bytes: int = 500) -> str:
    """Retrieve JSON padded to an exact target size in bytes (default 500).
    Used for testing threshold boundary behavior. The output will be exactly
    target_bytes long."""
    base = {"boundary_test": True, "target": target_bytes, "data": ""}
    base_json = json.dumps(base)
    # Calculate padding needed (subtract existing length, account for padding field content)
    padding_needed = target_bytes - len(base_json)
    if padding_needed > 0:
        base["data"] = "x" * padding_needed
        # Adjust: re-serialize and fine-tune
        result = json.dumps(base)
        diff = target_bytes - len(result)
        if diff > 0:
            base["data"] = base["data"] + "x" * diff
        elif diff < 0:
            base["data"] = base["data"][:len(base["data"]) + diff]
        result = json.dumps(base)
        # Final trim or pad if off by one
        if len(result) < target_bytes:
            base["data"] = base["data"] + "x" * (target_bytes - len(result))
            result = json.dumps(base)
        elif len(result) > target_bytes:
            base["data"] = base["data"][:len(base["data"]) - (len(result) - target_bytes)]
            result = json.dumps(base)
        return result
    return base_json


@mcp.tool()
def sp_get_nested_json(size: int = 0) -> str:
    """Retrieve deeply nested JSON with heterogeneous array items. Some items
    have 'error' fields, some have 'metrics', some have both. Returns ~3000
    bytes for testing item_schema with mixed types and deep get_in paths.
    The size parameter is accepted for API consistency but ignored."""
    items = []
    for i in range(12):
        item = {
            "id": f"node-{i:03d}",
            "hostname": f"worker-{i:02d}.cluster.internal",
            "status": "healthy" if i % 4 != 0 else "degraded",
        }
        # Heterogeneous fields
        if i % 3 == 0:
            item["error"] = {
                "code": 500 + (i % 5),
                "message": f"Connection timeout after {30 + i}s",
                "retries": i % 4,
            }
        if i % 2 == 0:
            item["metrics"] = {
                "cpu_percent": round(20.0 + (i * 7.3) % 60, 1),
                "memory_mb": 512 + (i * 128) % 2048,
                "disk_io_ops": 100 + i * 47,
                "network": {
                    "rx_bytes": 1024000 + i * 50000,
                    "tx_bytes": 512000 + i * 25000,
                    "connections": 10 + i * 3,
                },
            }
        if i % 5 == 0:
            item["labels"] = {
                "zone": ["us-east-1a", "us-east-1b", "eu-west-1a"][i % 3],
                "tier": "critical" if i < 4 else "standard",
                "pool": f"pool-{chr(65 + i % 3)}",
            }
        items.append(item)

    output = {
        "cluster": {
            "name": "prod-us-east",
            "version": "1.28.4",
            "node_count": len(items),
        },
        "nodes": items,
        "summary": {
            "healthy": sum(1 for it in items if it["status"] == "healthy"),
            "degraded": sum(1 for it in items if it["status"] == "degraded"),
            "with_errors": sum(1 for it in items if "error" in it),
            "with_metrics": sum(1 for it in items if "metrics" in it),
        },
    }
    return json.dumps(output)


# ---------------------------------------------------------------------------
# Entry point
# ---------------------------------------------------------------------------

if __name__ == "__main__":
    print("scratchpad-test-mcp starting")
    mcp.run(transport="streamable-http", host="0.0.0.0", port=8083)
