"""
Scratchpad Test FastMCP Server

Returns deterministic, controlled-size outputs for testing scratchpad
context window management: interception thresholds, exploration tools,
context budget tracking, and SSE event emission.

9 tools with configurable output sizes to exercise all 8 scratchpad
exploration tools (head, slice, grep, schema, item_schema, get_in,
iterate_over, read).

Integration tools (sp_get_*): Used by Docker-based integration tests.
E2E tools (sp_inventory_report, sp_log_analysis, sp_cluster_status):
Used by run-scratchpad-comparison.sh with verifiable "needle" values
at deterministic positions for answer-correctness assertions.
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
# E2E tools — deterministic needles for answer-correctness assertions
# ---------------------------------------------------------------------------


CATEGORIES = ["electronics", "clothing", "food", "tools", "furniture"]
WAREHOUSES = ["us-east-1", "us-west-2", "eu-west-1", "ap-south-1", "ap-northeast-1"]
ITEM_NAMES = [
    "Wireless Charger", "USB-C Hub", "Bluetooth Speaker", "LED Desk Lamp",
    "Mechanical Keyboard", "Noise-Canceling Headphones", "Portable SSD",
    "Smart Watch", "Webcam HD", "Ergonomic Mouse",
]


def generate_inventory_item(index: int) -> dict:
    """Generate a deterministic inventory item.

    Known needles:
      index 7  → SKU-007, price=42.99, category=electronics, in_stock=False
      index 22 → SKU-022, price=15.50, warehouse=us-west-2
    """
    # Deterministic price: base + fractional from index
    price = round(5.0 + (index * 7.31) % 95, 2)
    # Override specific needles
    if index == 7:
        price = 42.99
    elif index == 22:
        price = 15.50

    return {
        "sku": f"SKU-{index:03d}",
        "name": f"{ITEM_NAMES[index % len(ITEM_NAMES)]} v{1 + index // 10}",
        "category": CATEGORIES[index % len(CATEGORIES)],
        "price": price,
        "in_stock": index % 3 != 0,  # every 3rd item is out of stock
        "quantity": (index * 17) % 200,
        "warehouse": WAREHOUSES[index % len(WAREHOUSES)],
    }


@mcp.tool()
def sp_inventory_report(size: int = 10000) -> str:
    """Retrieve an inventory report with product items and summary statistics.
    Size parameter controls approximate output size in bytes (default 10000).
    Returns JSON with items array, summary with out_of_stock_count, total_value,
    and category_breakdown."""
    # Each item is roughly 200-230 bytes of JSON
    items_needed = max(1, size // 215)
    items = [generate_inventory_item(i) for i in range(items_needed)]

    out_of_stock = sum(1 for it in items if not it["in_stock"])
    total_value = round(sum(it["price"] * it["quantity"] for it in items), 2)

    category_counts = {}
    for it in items:
        category_counts[it["category"]] = category_counts.get(it["category"], 0) + 1

    output = {
        "items": items,
        "summary": {
            "total_items": len(items),
            "out_of_stock_count": out_of_stock,
            "total_value": total_value,
            "category_breakdown": category_counts,
        },
        "report_metadata": {
            "generated": "2024-03-15T09:00:00Z",
            "warehouse_region": "all",
        },
    }
    return json.dumps(output)


LOG_SERVICES = ["auth-service", "data-pipeline", "api-gateway", "scheduler",
                "metrics-collector", "cache-proxy"]
LOG_LEVELS = ["INFO", "INFO", "INFO", "WARN", "INFO", "INFO", "INFO",
              "DEBUG", "INFO", "ERROR"]
LOG_MESSAGES = [
    "Request processed successfully",
    "Connection pool refreshed",
    "High latency detected on upstream",
    "Cache miss ratio above threshold",
    "Health check passed",
    "Retry attempt completed",
    "Rate limit check passed",
    "Batch processing complete",
    "Session token validated",
    "Scheduled job triggered",
]

# Error entries injected at specific positions
ERROR_NEEDLES = {
    35: ("auth-service", "Authentication failure", "AUTH-4091"),
    78: ("data-pipeline", "Timeout exceeded", "PIPE-5003"),
    112: ("api-gateway", "Rate limit exceeded", "GW-2047"),
    156: ("cache-proxy", "Connection refused", "CACHE-6112"),
}


@mcp.tool()
def sp_log_analysis(size: int = 8000) -> str:
    """Retrieve application log entries for analysis. Size parameter controls
    approximate output size in bytes (default 8000). Returns multi-line text
    with timestamped entries. Contains specific ERROR entries with unique error
    codes at deterministic positions for search testing."""
    lines_needed = max(1, size // 95)
    lines = []
    for i in range(lines_needed):
        ts_seconds = 1710500000 + i * 12
        hour = (10 + (ts_seconds // 3600) % 24) % 24
        minute = (ts_seconds // 60) % 60
        second = ts_seconds % 60
        ts = f"2024-03-15T{hour:02d}:{minute:02d}:{second:02d}Z"

        if i in ERROR_NEEDLES:
            service, message, code = ERROR_NEEDLES[i]
            lines.append(f"{ts} [ERROR] {service}: {message} (code: {code})")
        else:
            level = LOG_LEVELS[i % len(LOG_LEVELS)]
            service = LOG_SERVICES[i % len(LOG_SERVICES)]
            message = LOG_MESSAGES[i % len(LOG_MESSAGES)]
            lines.append(f"{ts} [{level}] {service}: {message}")

    return "\n".join(lines)


NODE_NAMES = ["control-plane-01", "control-plane-02", "control-plane-03",
              "worker-01", "worker-02", "worker-03", "worker-04", "worker-05",
              "worker-06", "worker-07", "worker-08", "worker-09"]


def generate_pod(node_index: int, pod_index: int) -> dict:
    """Generate a deterministic pod."""
    namespaces = ["default", "kube-system", "monitoring", "production"]
    names = ["nginx", "redis", "postgres", "prometheus", "grafana",
             "api-server", "worker", "scheduler"]
    return {
        "name": f"{names[(node_index + pod_index) % len(names)]}-"
                f"{(node_index * 10 + pod_index):04x}",
        "namespace": namespaces[(node_index + pod_index) % len(namespaces)],
        "status": "Running" if (node_index + pod_index) % 7 != 0 else "CrashLoopBackOff",
        "restarts": (node_index + pod_index) % 5,
        "cpu_millicores": 50 + ((node_index * 3 + pod_index * 7) % 450),
        "memory_mb": 64 + ((node_index * 11 + pod_index * 13) % 512),
    }


@mcp.tool()
def sp_cluster_status(size: int = 6000) -> str:
    """Retrieve Kubernetes cluster status with node and pod details. Size
    parameter controls approximate output size in bytes (default 6000).
    Returns nested JSON with nodes array, each containing pods.

    Known needle: worker-03 always has status=NotReady, cpu_percent=98.7."""
    # ~500 bytes per node with 3 pods
    nodes_needed = max(3, min(len(NODE_NAMES), size // 500))
    nodes = []
    for i in range(nodes_needed):
        name = NODE_NAMES[i] if i < len(NODE_NAMES) else f"worker-{i:02d}"

        # Needle: worker-03 (index 5) is NotReady with high CPU
        if name == "worker-03":
            status = "NotReady"
            cpu_pct = 98.7
        else:
            status = "Ready"
            cpu_pct = round(15.0 + (i * 11.3) % 70, 1)

        pods_per_node = max(1, min(5, size // (nodes_needed * 200)))
        pods = [generate_pod(i, j) for j in range(pods_per_node)]

        nodes.append({
            "name": name,
            "status": status,
            "cpu_percent": cpu_pct,
            "memory_mb": 2048 + (i * 512) % 8192,
            "pods": pods,
            "conditions": {
                "ready": status == "Ready",
                "disk_pressure": False,
                "memory_pressure": name == "worker-03",
                "pid_pressure": False,
            },
        })

    output = {
        "cluster": {
            "name": "prod-us-east",
            "version": "1.29.2",
            "total_nodes": len(nodes),
            "ready_nodes": sum(1 for n in nodes if n["status"] == "Ready"),
        },
        "nodes": nodes,
    }
    return json.dumps(output)


# ---------------------------------------------------------------------------
# Entry point
# ---------------------------------------------------------------------------

if __name__ == "__main__":
    print("scratchpad-test-mcp starting")
    mcp.run(transport="streamable-http", host="0.0.0.0", port=8083)
