"""
Mock Kubernetes SRE FastMCP Server

Simulates a production Kubernetes cluster with Prometheus monitoring.
Provides 17 tools across Kubernetes, Prometheus, and Alertmanager domains.
Used by SRE orchestration integration tests.

Set VERBOSE_MODE=true to simulate a realistic production cluster with dozens
of workloads, verbose k8s metadata, high-cardinality Prometheus metrics, and
multiple firing alerts.
"""

import json
import os
from typing import Optional

import yaml
from fastmcp import FastMCP

VERBOSE_MODE = os.environ.get("VERBOSE_MODE", "").lower() in ("true", "1")

mcp = FastMCP("k8s-sre-mcp")

# ---------------------------------------------------------------------------
# Simulated cluster state
# ---------------------------------------------------------------------------

if VERBOSE_MODE:
    from verbose_data import (
        NAMESPACES,
        WORKLOADS,
        SERVICES,
        CANNED_SERVICE_MONITORS,
        METRIC_METADATA,
        EXISTING_ALERTS,
        EXISTING_RULES,
        PROMETHEUS_QUERY_DATA,
        PROMETHEUS_TARGETS_DATA,
        LABEL_VALUES_DATA,
    )
else:
    NAMESPACES = ["production", "staging", "kube-system", "monitoring"]

    WORKLOADS = {
        "production": [
            {
                "name": "nginx-ingress",
                "kind": "Deployment",
                "replicas": 3,
                "labels": {"app": "nginx-ingress", "team": "platform"},
                "containers": [
                    {
                        "name": "nginx-ingress-controller",
                        "image": "registry.k8s.io/ingress-nginx/controller:v1.9.4",
                        "ports": [
                            {"name": "http", "containerPort": 80},
                            {"name": "https", "containerPort": 443},
                            {"name": "metrics", "containerPort": 10254},
                        ],
                    }
                ],
                "annotations": {
                    "prometheus.io/scrape": "true",
                    "prometheus.io/port": "10254",
                    "prometheus.io/path": "/metrics",
                },
            },
            {
                "name": "payment-service",
                "kind": "Deployment",
                "replicas": 2,
                "labels": {"app": "payment-service", "team": "payments", "lang": "go"},
                "containers": [
                    {
                        "name": "payment-service",
                        "image": "internal/payment-service:v2.3.1",
                        "ports": [
                            {"name": "http", "containerPort": 8080},
                            {"name": "metrics", "containerPort": 9090},
                        ],
                    }
                ],
                "annotations": {
                    "prometheus.io/scrape": "true",
                    "prometheus.io/port": "9090",
                },
            },
            {
                "name": "user-api",
                "kind": "Deployment",
                "replicas": 3,
                "labels": {"app": "user-api", "team": "identity", "lang": "java"},
                "containers": [
                    {
                        "name": "user-api",
                        "image": "internal/user-api:v1.8.0",
                        "ports": [
                            {"name": "http", "containerPort": 8080},
                            {"name": "metrics", "containerPort": 8081},
                        ],
                    }
                ],
                "annotations": {
                    "prometheus.io/scrape": "true",
                    "prometheus.io/port": "8081",
                    "prometheus.io/path": "/actuator/prometheus",
                },
            },
            {
                "name": "redis",
                "kind": "StatefulSet",
                "replicas": 3,
                "labels": {"app": "redis", "team": "platform"},
                "containers": [
                    {
                        "name": "redis",
                        "image": "redis:7.2-alpine",
                        "ports": [{"name": "redis", "containerPort": 6379}],
                    }
                ],
                "annotations": {},
            },
            {
                "name": "cronjob-worker",
                "kind": "Deployment",
                "replicas": 1,
                "labels": {"app": "cronjob-worker", "team": "batch"},
                "containers": [
                    {
                        "name": "worker",
                        "image": "internal/cronjob-worker:v1.0.2",
                        "ports": [],
                    }
                ],
                "annotations": {},
            },
        ],
    }

    SERVICES = {
        "production": [
            {
                "name": "nginx-ingress",
                "type": "LoadBalancer",
                "ports": [
                    {"name": "http", "port": 80, "targetPort": 80},
                    {"name": "https", "port": 443, "targetPort": 443},
                    {"name": "metrics", "port": 10254, "targetPort": 10254},
                ],
                "selector": {"app": "nginx-ingress"},
            },
            {
                "name": "payment-service",
                "type": "ClusterIP",
                "ports": [
                    {"name": "http", "port": 8080, "targetPort": 8080},
                    {"name": "metrics", "port": 9090, "targetPort": 9090},
                ],
                "selector": {"app": "payment-service"},
            },
            {
                "name": "user-api",
                "type": "ClusterIP",
                "ports": [
                    {"name": "http", "port": 8080, "targetPort": 8080},
                    {"name": "metrics", "port": 8081, "targetPort": 8081},
                ],
                "selector": {"app": "user-api"},
            },
            {
                "name": "redis",
                "type": "ClusterIP",
                "ports": [{"name": "redis", "port": 6379, "targetPort": 6379}],
                "selector": {"app": "redis"},
            },
        ],
    }

    # Pre-existing ServiceMonitor (nginx-ingress already has one)
    CANNED_SERVICE_MONITORS = {
        "production": [
            {
                "name": "nginx-ingress-monitor",
                "namespace": "production",
                "labels": {"team": "platform"},
                "selector": {"matchLabels": {"app": "nginx-ingress"}},
                "endpoints": [{"port": "metrics", "interval": "30s", "path": "/metrics"}],
            }
        ],
    }

    # Prometheus metric metadata
    METRIC_METADATA = {
        "nginx_ingress_controller_requests_total": {
            "type": "counter",
            "help": "Total number of requests processed",
            "unit": "",
        },
        "nginx_ingress_controller_request_duration_seconds": {
            "type": "histogram",
            "help": "Request duration in seconds",
            "unit": "seconds",
        },
        "http_requests_total": {
            "type": "counter",
            "help": "Total HTTP requests",
            "unit": "",
        },
        "http_request_duration_seconds": {
            "type": "histogram",
            "help": "HTTP request duration",
            "unit": "seconds",
        },
        "process_cpu_seconds_total": {
            "type": "counter",
            "help": "Total user and system CPU time spent in seconds",
            "unit": "seconds",
        },
        "process_resident_memory_bytes": {
            "type": "gauge",
            "help": "Resident memory size in bytes",
            "unit": "bytes",
        },
        "up": {
            "type": "gauge",
            "help": "Whether the target is up",
            "unit": "",
        },
        "jvm_memory_used_bytes": {
            "type": "gauge",
            "help": "JVM memory used",
            "unit": "bytes",
        },
        "go_goroutines": {
            "type": "gauge",
            "help": "Number of goroutines",
            "unit": "",
        },
    }

    # Existing alerts
    EXISTING_ALERTS = [
        {
            "name": "HighErrorRate-nginx",
            "state": "firing",
            "severity": "warning",
            "expr": 'rate(nginx_ingress_controller_requests_total{status=~"5.."}[5m]) > 0.05',
            "annotations": {
                "summary": "High 5xx error rate on nginx-ingress",
                "description": "Error rate exceeds 5% over 5 minutes",
            },
        },
        {
            "name": "PodRestartLoop",
            "state": "pending",
            "severity": "critical",
            "expr": "increase(kube_pod_container_status_restarts_total[1h]) > 5",
            "annotations": {
                "summary": "Pod restart loop detected",
                "description": "Pod has restarted more than 5 times in the last hour",
            },
        },
    ]

    EXISTING_RULES = [
        {
            "name": "nginx-error-rate",
            "expr": 'rate(nginx_ingress_controller_requests_total{status=~"5.."}[5m]) > 0.05',
            "for": "5m",
            "severity": "warning",
            "annotations": {"summary": "High 5xx error rate on nginx-ingress"},
        },
    ]

    # Minimal Prometheus data dicts (used by refactored query functions)
    PROMETHEUS_QUERY_DATA = {
        "up": [
            {"metric": {"__name__": "up", "job": w_name, "namespace": "production"}, "value": [1709000000, "1"]}
            for w_name in ["nginx-ingress", "payment-service", "user-api"]
        ],
        "nginx_ingress_controller_requests_total": [
            {
                "metric": {"__name__": "nginx_ingress_controller_requests_total", "status": "200", "namespace": "production"},
                "value": [1709000000, "125432"],
            },
            {
                "metric": {"__name__": "nginx_ingress_controller_requests_total", "status": "500", "namespace": "production"},
                "value": [1709000000, "342"],
            },
        ],
        "http_requests_total": [
            {"metric": {"__name__": "http_requests_total", "job": svc, "namespace": "production"}, "value": [1709000000, "54321"]}
            for svc in ["payment-service", "user-api"]
        ],
        "http_request_duration_seconds": [
            {"metric": {"__name__": "http_request_duration_seconds", "job": svc, "quantile": "0.99"}, "value": [1709000000, "0.245"]}
            for svc in ["payment-service", "user-api"]
        ],
        "process_resident_memory_bytes": [
            {"metric": {"__name__": "process_resident_memory_bytes", "job": "payment-service"}, "value": [1709000000, "134217728"]},
        ],
        "go_goroutines": [
            {"metric": {"__name__": "go_goroutines", "job": "payment-service"}, "value": [1709000000, "42"]},
        ],
        "jvm_memory_used_bytes": [
            {"metric": {"__name__": "jvm_memory_used_bytes", "job": "user-api", "area": "heap"}, "value": [1709000000, "268435456"]},
        ],
    }

    PROMETHEUS_TARGETS_DATA = [
        ("nginx-ingress", "production", 10254, "/metrics"),
        ("payment-service", "production", 9090, "/metrics"),
        ("user-api", "production", 8081, "/actuator/prometheus"),
    ]

    LABEL_VALUES_DATA = {
        "namespace": NAMESPACES,
        "job": ["nginx-ingress", "payment-service", "user-api"],
        "instance": [
            "nginx-ingress.production.svc:10254",
            "payment-service.production.svc:9090",
            "user-api.production.svc:8081",
        ],
        "status": ["200", "201", "204", "301", "400", "404", "500", "502", "503"],
    }

# Dynamic state: applied manifests
applied_service_monitors: dict[str, list[dict]] = {}
applied_prometheus_rules: list[dict] = []
applied_alert_rules: list[dict] = []


# ---------------------------------------------------------------------------
# Kubernetes tools (7)
# ---------------------------------------------------------------------------


@mcp.tool()
def k8s_list_namespaces() -> str:
    """List all Kubernetes namespaces in the cluster."""
    return json.dumps({"namespaces": NAMESPACES})


@mcp.tool()
def k8s_list_workloads(namespace: str) -> str:
    """List workloads (Deployments, StatefulSets) in a namespace with summary info."""
    workloads = WORKLOADS.get(namespace, [])
    summary = []
    for w in workloads:
        metrics_ports = []
        for c in w.get("containers", []):
            for p in c.get("ports", []):
                if p.get("name") == "metrics":
                    metrics_ports.append(p["containerPort"])
        summary.append(
            {
                "name": w["name"],
                "kind": w["kind"],
                "replicas": w["replicas"],
                "labels": w["labels"],
                "has_metrics": len(metrics_ports) > 0,
                "metrics_ports": metrics_ports,
            }
        )
    return json.dumps({"namespace": namespace, "workloads": summary})


@mcp.tool()
def k8s_get_workload(namespace: str, name: str) -> str:
    """Get detailed spec for a specific workload including containers, ports, and annotations."""
    workloads = WORKLOADS.get(namespace, [])
    for w in workloads:
        if w["name"] == name:
            return json.dumps({"namespace": namespace, "workload": w})
    return json.dumps({"error": f"Workload '{name}' not found in namespace '{namespace}'"})


@mcp.tool()
def k8s_list_services(namespace: str) -> str:
    """List Kubernetes services in a namespace with ports and selectors."""
    services = SERVICES.get(namespace, [])
    return json.dumps({"namespace": namespace, "services": services})


@mcp.tool()
def k8s_get_service(namespace: str, name: str) -> str:
    """Get detailed information about a specific Kubernetes service."""
    services = SERVICES.get(namespace, [])
    for s in services:
        if s["name"] == name:
            return json.dumps({"namespace": namespace, "service": s})
    return json.dumps({"error": f"Service '{name}' not found in namespace '{namespace}'"})


@mcp.tool()
def k8s_list_service_monitors(namespace: str) -> str:
    """List ServiceMonitor resources in a namespace (canned + dynamically applied)."""
    monitors = list(CANNED_SERVICE_MONITORS.get(namespace, []))
    monitors.extend(applied_service_monitors.get(namespace, []))
    return json.dumps({"namespace": namespace, "service_monitors": monitors})


@mcp.tool()
def k8s_apply_manifest(yaml_content: str) -> str:
    """Apply a Kubernetes manifest (ServiceMonitor or PrometheusRule). Returns success or validation error."""
    try:
        doc = yaml.safe_load(yaml_content)
    except yaml.YAMLError as e:
        return json.dumps({"error": f"Invalid YAML: {e}"})

    if not isinstance(doc, dict):
        return json.dumps({"error": "YAML must be a mapping"})

    kind = doc.get("kind", "")
    metadata = doc.get("metadata", {})
    name = metadata.get("name", "unknown")
    namespace = metadata.get("namespace", "default")

    if kind == "ServiceMonitor":
        spec = doc.get("spec", {})
        monitor = {
            "name": name,
            "namespace": namespace,
            "labels": metadata.get("labels", {}),
            "selector": spec.get("selector", {}),
            "endpoints": spec.get("endpoints", []),
        }
        applied_service_monitors.setdefault(namespace, []).append(monitor)
        return json.dumps(
            {
                "status": "created",
                "kind": "ServiceMonitor",
                "name": name,
                "namespace": namespace,
            }
        )
    elif kind == "PrometheusRule":
        spec = doc.get("spec", {})
        rule = {
            "name": name,
            "namespace": namespace,
            "labels": metadata.get("labels", {}),
            "groups": spec.get("groups", []),
        }
        applied_prometheus_rules.append(rule)
        return json.dumps(
            {
                "status": "created",
                "kind": "PrometheusRule",
                "name": name,
                "namespace": namespace,
            }
        )
    else:
        return json.dumps(
            {
                "error": f"Unsupported kind '{kind}'. Only ServiceMonitor and PrometheusRule are supported."
            }
        )


# ---------------------------------------------------------------------------
# Prometheus tools (4)
# ---------------------------------------------------------------------------


@mcp.tool()
def prometheus_query(query: str) -> str:
    """Execute a PromQL instant query and return results. Supports common metrics patterns."""
    # Match query substring against PROMETHEUS_QUERY_DATA keys
    results = []
    for pattern, data in PROMETHEUS_QUERY_DATA.items():
        if pattern in query:
            results = list(data)
            break
    if not results:
        results = [{"metric": {"__name__": "unknown_metric"}, "value": [1709000000, "0"]}]

    return json.dumps(
        {
            "status": "success",
            "data": {"resultType": "vector", "result": results},
        }
    )


@mcp.tool()
def prometheus_targets() -> str:
    """List all active Prometheus scrape targets with health status."""
    targets = []
    for name, namespace, port, path in PROMETHEUS_TARGETS_DATA:
        targets.append(
            {
                "labels": {
                    "job": name,
                    "namespace": namespace,
                    "instance": f"{name}.{namespace}.svc:{port}",
                },
                "scrapeUrl": f"http://{name}.{namespace}.svc:{port}{path}",
                "health": "up",
                "lastScrape": "2024-02-27T10:30:00Z",
                "lastScrapeDuration": "0.012s",
            }
        )
    return json.dumps({"status": "success", "data": {"activeTargets": targets}})


@mcp.tool()
def prometheus_metric_metadata(metric_name: str) -> str:
    """Get type, help text, and unit for a Prometheus metric."""
    meta = METRIC_METADATA.get(metric_name)
    if meta:
        return json.dumps({"metric": metric_name, "metadata": meta})
    return json.dumps({"metric": metric_name, "metadata": None, "note": "Metric not found in metadata"})


@mcp.tool()
def prometheus_label_values(label_name: str, metric_name: Optional[str] = None) -> str:
    """Get distinct values for a label, optionally filtered by metric name."""
    values = LABEL_VALUES_DATA.get(label_name, [])
    return json.dumps({"label": label_name, "values": values})


# ---------------------------------------------------------------------------
# Alertmanager tools (4)
# ---------------------------------------------------------------------------


@mcp.tool()
def alertmanager_get_status() -> str:
    """Get Alertmanager cluster status and version info."""
    return json.dumps(
        {
            "cluster": {"status": "ready", "peers": 3},
            "versionInfo": {"version": "0.27.0", "branch": "HEAD"},
            "uptime": "72h15m",
            "config": {"route": {"receiver": "slack-notifications", "group_wait": "30s"}},
        }
    )


@mcp.tool()
def alertmanager_get_alerts() -> str:
    """Get currently firing and pending alerts."""
    return json.dumps({"status": "success", "alerts": EXISTING_ALERTS})


@mcp.tool()
def alertmanager_get_rules() -> str:
    """Get all alerting rules (canned + dynamically created)."""
    all_rules = list(EXISTING_RULES) + applied_alert_rules
    return json.dumps({"status": "success", "rules": all_rules})


@mcp.tool()
def alertmanager_create_rule(
    name: str, expr: str, for_duration: str, severity: str, annotations: str
) -> str:
    """Create a new Prometheus alerting rule. Annotations should be a JSON string with summary and description."""
    try:
        ann = json.loads(annotations) if isinstance(annotations, str) else annotations
    except (json.JSONDecodeError, TypeError):
        ann = {"summary": annotations}

    rule = {
        "name": name,
        "expr": expr,
        "for": for_duration,
        "severity": severity,
        "annotations": ann,
    }
    applied_alert_rules.append(rule)
    return json.dumps({"status": "created", "rule": rule})


# ---------------------------------------------------------------------------
# Entry point
# ---------------------------------------------------------------------------

if __name__ == "__main__":
    mode = "VERBOSE" if VERBOSE_MODE else "minimal"
    print(f"k8s-sre-mcp starting in {mode} mode")
    mcp.run(transport="streamable-http", host="0.0.0.0", port=8082)
