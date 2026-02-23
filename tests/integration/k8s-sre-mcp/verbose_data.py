"""
Verbose data for k8s-sre-mcp mock server.

When VERBOSE_MODE is enabled, these structures replace the minimal defaults in
server.py. The goal is to simulate a realistic production Kubernetes cluster
with dozens of workloads, verbose YAML-style metadata, high-cardinality
Prometheus metrics, and multiple firing alerts — stress-testing orchestration
context management without changing the default test path.
"""

# ---------------------------------------------------------------------------
# Namespaces (7 total)
# ---------------------------------------------------------------------------

NAMESPACES = [
    "production",
    "staging",
    "kube-system",
    "monitoring",
    "cert-manager",
    "istio-system",
    "logging",
]

# ---------------------------------------------------------------------------
# Helper: common k8s metadata fields
# ---------------------------------------------------------------------------

_MANAGED_FIELDS = [
    {
        "manager": "kube-controller-manager",
        "operation": "Update",
        "apiVersion": "apps/v1",
        "time": "2024-01-15T08:30:00Z",
        "fieldsType": "FieldsV1",
        "fieldsV1": {"f:metadata": {"f:annotations": {}}, "f:spec": {"f:replicas": {}}},
    },
    {
        "manager": "argocd-application-controller",
        "operation": "Apply",
        "apiVersion": "apps/v1",
        "time": "2024-02-20T14:22:00Z",
        "fieldsType": "FieldsV1",
        "fieldsV1": {"f:spec": {"f:template": {"f:spec": {"f:containers": {}}}}},
    },
]


def _base_metadata(name, namespace, labels, generation=5):
    return {
        "creationTimestamp": "2024-01-10T12:00:00Z",
        "generation": generation,
        "resourceVersion": "928374",
        "uid": f"a1b2c3d4-{name[:8]}-{namespace[:4]}-0000-000000000001",
        "managedFields": _MANAGED_FIELDS,
    }


def _healthy_status(replicas, generation=5):
    return {
        "availableReplicas": replicas,
        "readyReplicas": replicas,
        "replicas": replicas,
        "updatedReplicas": replicas,
        "observedGeneration": generation,
        "conditions": [
            {
                "type": "Available",
                "status": "True",
                "lastTransitionTime": "2024-01-10T12:05:00Z",
                "lastUpdateTime": "2024-01-10T12:05:00Z",
                "reason": "MinimumReplicasAvailable",
                "message": "Deployment has minimum availability.",
            },
            {
                "type": "Progressing",
                "status": "True",
                "lastTransitionTime": "2024-02-20T14:25:00Z",
                "lastUpdateTime": "2024-02-20T14:25:00Z",
                "reason": "NewReplicaSetAvailable",
                "message": f"ReplicaSet has successfully progressed.",
            },
        ],
    }


def _default_spec():
    return {
        "strategy": {"type": "RollingUpdate", "rollingUpdate": {"maxSurge": "25%", "maxUnavailable": "25%"}},
        "revisionHistoryLimit": 10,
        "progressDeadlineSeconds": 600,
    }


_ISTIO_INIT_CONTAINER = {
    "name": "istio-init",
    "image": "docker.io/istio/proxyv2:1.20.2",
    "args": ["istio-iptables", "-p", "15001", "-z", "15006", "-u", "1337", "-m", "REDIRECT"],
    "resources": {"limits": {"cpu": "2000m", "memory": "1Gi"}, "requests": {"cpu": "10m", "memory": "40Mi"}},
    "securityContext": {
        "allowPrivilegeEscalation": False,
        "capabilities": {"add": ["NET_ADMIN", "NET_RAW"], "drop": ["ALL"]},
        "readOnlyRootFilesystem": False,
        "runAsGroup": 0,
        "runAsNonRoot": False,
        "runAsUser": 0,
    },
}

_ISTIO_SIDECAR = {
    "name": "istio-proxy",
    "image": "docker.io/istio/proxyv2:1.20.2",
    "ports": [
        {"containerPort": 15090, "name": "http-envoy-prom", "protocol": "TCP"},
        {"containerPort": 15021, "name": "health", "protocol": "TCP"},
    ],
    "resources": {"limits": {"cpu": "2000m", "memory": "1Gi"}, "requests": {"cpu": "100m", "memory": "128Mi"}},
    "securityContext": {"allowPrivilegeEscalation": False, "runAsUser": 1337, "runAsGroup": 1337},
    "volumeMounts": [
        {"name": "istio-envoy", "mountPath": "/etc/istio/proxy"},
        {"name": "istio-certs", "mountPath": "/var/run/secrets/istio", "readOnly": True},
    ],
}

_LOG_FORWARDER_SIDECAR = {
    "name": "log-forwarder",
    "image": "internal/log-forwarder:v1.2.0",
    "resources": {"limits": {"cpu": "200m", "memory": "128Mi"}, "requests": {"cpu": "50m", "memory": "64Mi"}},
    "volumeMounts": [{"name": "app-logs", "mountPath": "/var/log/app", "readOnly": True}],
    "env": [
        {"name": "LOG_LEVEL", "value": "info"},
        {"name": "MEZMO_INGESTION_KEY", "valueFrom": {"secretKeyRef": {"name": "mezmo-creds", "key": "ingestion-key"}}},
    ],
}


def _common_annotations(app_name):
    return {
        "prometheus.io/scrape": "true",
        "argocd.argoproj.io/managed-by": "argocd",
        "argocd.argoproj.io/sync-wave": "5",
        "helm.sh/chart": f"{app_name}-1.0.0",
        "meta.helm.sh/release-name": app_name,
        "meta.helm.sh/release-namespace": "production",
        "deployment.kubernetes.io/revision": "3",
        "kubectl.kubernetes.io/last-applied-configuration": '{"truncated": "...large JSON..."}',
    }


def _standard_probes(port, path="/healthz"):
    return {
        "livenessProbe": {"httpGet": {"path": f"{path}/live", "port": port}, "initialDelaySeconds": 15, "periodSeconds": 20, "failureThreshold": 3},
        "readinessProbe": {"httpGet": {"path": f"{path}/ready", "port": port}, "initialDelaySeconds": 5, "periodSeconds": 10, "failureThreshold": 3},
        "startupProbe": {"httpGet": {"path": f"{path}/startup", "port": port}, "initialDelaySeconds": 10, "periodSeconds": 5, "failureThreshold": 30},
    }


def _standard_resources(cpu_req="100m", mem_req="128Mi", cpu_lim="500m", mem_lim="512Mi"):
    return {"requests": {"cpu": cpu_req, "memory": mem_req}, "limits": {"cpu": cpu_lim, "memory": mem_lim}}


def _standard_volumes(app_name):
    return [
        {"name": "app-config", "configMap": {"name": f"{app_name}-config"}},
        {"name": "app-secrets", "secret": {"secretName": f"{app_name}-secrets"}},
        {"name": "app-logs", "emptyDir": {}},
        {"name": "tmp", "emptyDir": {"medium": "Memory", "sizeLimit": "64Mi"}},
        {"name": "istio-envoy", "emptyDir": {"medium": "Memory"}},
        {"name": "istio-certs", "secret": {"secretName": "istio.default"}},
    ]


def _standard_env(app_name):
    return [
        {"name": "APP_NAME", "value": app_name},
        {"name": "LOG_LEVEL", "value": "info"},
        {"name": "NODE_NAME", "valueFrom": {"fieldRef": {"fieldPath": "spec.nodeName"}}},
        {"name": "POD_NAME", "valueFrom": {"fieldRef": {"fieldPath": "metadata.name"}}},
        {"name": "POD_NAMESPACE", "valueFrom": {"fieldRef": {"fieldPath": "metadata.namespace"}}},
        {"name": "DB_HOST", "valueFrom": {"configMapKeyRef": {"name": f"{app_name}-config", "key": "db-host"}}},
        {"name": "DB_PASSWORD", "valueFrom": {"secretKeyRef": {"name": f"{app_name}-secrets", "key": "db-password"}}},
    ]


# ---------------------------------------------------------------------------
# Workloads
# ---------------------------------------------------------------------------

WORKLOADS = {
    "production": [
        {
            "name": "nginx-ingress",
            "kind": "Deployment",
            "replicas": 3,
            "labels": {"app": "nginx-ingress", "team": "platform", "app.kubernetes.io/managed-by": "Helm"},
            "containers": [
                {
                    "name": "nginx-ingress-controller",
                    "image": "registry.k8s.io/ingress-nginx/controller:v1.9.4",
                    "ports": [
                        {"name": "http", "containerPort": 80, "protocol": "TCP"},
                        {"name": "https", "containerPort": 443, "protocol": "TCP"},
                        {"name": "metrics", "containerPort": 10254, "protocol": "TCP"},
                    ],
                    "resources": _standard_resources("200m", "256Mi", "1000m", "1Gi"),
                    "env": _standard_env("nginx-ingress"),
                    "volumeMounts": [
                        {"name": "app-config", "mountPath": "/etc/nginx/conf.d"},
                        {"name": "tmp", "mountPath": "/tmp"},
                    ],
                    "securityContext": {"runAsUser": 101, "runAsNonRoot": True, "allowPrivilegeEscalation": False},
                    **_standard_probes(10254, "/healthz"),
                }
            ],
            "initContainers": [_ISTIO_INIT_CONTAINER],
            "sidecarContainers": [_ISTIO_SIDECAR, _LOG_FORWARDER_SIDECAR],
            "volumes": _standard_volumes("nginx-ingress"),
            "annotations": {
                **_common_annotations("nginx-ingress"),
                "prometheus.io/port": "10254",
                "prometheus.io/path": "/metrics",
            },
            "metadata": _base_metadata("nginx-ingress", "production", {"app": "nginx-ingress"}, generation=8),
            "status": _healthy_status(3, 8),
            "spec": _default_spec(),
        },
        {
            "name": "payment-service",
            "kind": "Deployment",
            "replicas": 2,
            "labels": {"app": "payment-service", "team": "payments", "lang": "go", "app.kubernetes.io/managed-by": "Helm"},
            "containers": [
                {
                    "name": "payment-service",
                    "image": "internal/payment-service:v2.3.1",
                    "ports": [
                        {"name": "http", "containerPort": 8080, "protocol": "TCP"},
                        {"name": "grpc", "containerPort": 9000, "protocol": "TCP"},
                        {"name": "metrics", "containerPort": 9090, "protocol": "TCP"},
                    ],
                    "resources": _standard_resources("200m", "256Mi", "1000m", "1Gi"),
                    "env": _standard_env("payment-service") + [
                        {"name": "STRIPE_API_KEY", "valueFrom": {"secretKeyRef": {"name": "payment-secrets", "key": "stripe-api-key"}}},
                        {"name": "REDIS_URL", "value": "redis://redis.production.svc:6379"},
                        {"name": "GRPC_PORT", "value": "9000"},
                    ],
                    "volumeMounts": [
                        {"name": "app-config", "mountPath": "/etc/payment"},
                        {"name": "app-secrets", "mountPath": "/etc/secrets", "readOnly": True},
                        {"name": "tmp", "mountPath": "/tmp"},
                    ],
                    "securityContext": {"runAsUser": 1000, "runAsNonRoot": True, "allowPrivilegeEscalation": False, "readOnlyRootFilesystem": True},
                    **_standard_probes(8080),
                }
            ],
            "initContainers": [_ISTIO_INIT_CONTAINER],
            "sidecarContainers": [_ISTIO_SIDECAR, _LOG_FORWARDER_SIDECAR],
            "volumes": _standard_volumes("payment-service"),
            "annotations": {
                **_common_annotations("payment-service"),
                "prometheus.io/port": "9090",
                "datadog/service": "payment-service",
            },
            "metadata": _base_metadata("payment-service", "production", {"app": "payment-service"}, generation=12),
            "status": _healthy_status(2, 12),
            "spec": _default_spec(),
        },
        {
            "name": "user-api",
            "kind": "Deployment",
            "replicas": 3,
            "labels": {"app": "user-api", "team": "identity", "lang": "java", "app.kubernetes.io/managed-by": "Helm"},
            "containers": [
                {
                    "name": "user-api",
                    "image": "internal/user-api:v1.8.0",
                    "ports": [
                        {"name": "http", "containerPort": 8080, "protocol": "TCP"},
                        {"name": "metrics", "containerPort": 8081, "protocol": "TCP"},
                    ],
                    "resources": _standard_resources("500m", "512Mi", "2000m", "2Gi"),
                    "env": _standard_env("user-api") + [
                        {"name": "JAVA_OPTS", "value": "-Xms512m -Xmx1536m -XX:+UseG1GC"},
                        {"name": "SPRING_PROFILES_ACTIVE", "value": "production"},
                    ],
                    "volumeMounts": [
                        {"name": "app-config", "mountPath": "/etc/user-api"},
                        {"name": "app-logs", "mountPath": "/var/log/app"},
                        {"name": "tmp", "mountPath": "/tmp"},
                    ],
                    "securityContext": {"runAsUser": 1000, "runAsNonRoot": True, "allowPrivilegeEscalation": False},
                    **_standard_probes(8081, "/actuator/health"),
                }
            ],
            "initContainers": [
                _ISTIO_INIT_CONTAINER,
                {
                    "name": "config-loader",
                    "image": "internal/config-loader:v1.0.0",
                    "command": ["sh", "-c", "cp /config/* /etc/user-api/"],
                    "volumeMounts": [{"name": "app-config", "mountPath": "/etc/user-api"}],
                    "resources": {"limits": {"cpu": "100m", "memory": "64Mi"}, "requests": {"cpu": "10m", "memory": "32Mi"}},
                },
            ],
            "sidecarContainers": [_ISTIO_SIDECAR, _LOG_FORWARDER_SIDECAR],
            "volumes": _standard_volumes("user-api"),
            "annotations": {
                **_common_annotations("user-api"),
                "prometheus.io/port": "8081",
                "prometheus.io/path": "/actuator/prometheus",
            },
            "metadata": _base_metadata("user-api", "production", {"app": "user-api"}, generation=15),
            "status": _healthy_status(3, 15),
            "spec": _default_spec(),
        },
        {
            "name": "redis",
            "kind": "StatefulSet",
            "replicas": 3,
            "labels": {"app": "redis", "team": "platform", "app.kubernetes.io/managed-by": "Helm"},
            "containers": [
                {
                    "name": "redis",
                    "image": "redis:7.2-alpine",
                    "ports": [{"name": "redis", "containerPort": 6379, "protocol": "TCP"}],
                    "resources": _standard_resources("100m", "256Mi", "500m", "1Gi"),
                    "command": ["redis-server", "/etc/redis/redis.conf"],
                    "volumeMounts": [
                        {"name": "redis-data", "mountPath": "/data"},
                        {"name": "redis-config", "mountPath": "/etc/redis"},
                    ],
                    "livenessProbe": {"exec": {"command": ["redis-cli", "ping"]}, "initialDelaySeconds": 15, "periodSeconds": 20},
                    "readinessProbe": {"exec": {"command": ["redis-cli", "ping"]}, "initialDelaySeconds": 5, "periodSeconds": 10},
                }
            ],
            "volumes": [
                {"name": "redis-config", "configMap": {"name": "redis-config"}},
                {"name": "redis-data", "persistentVolumeClaim": {"claimName": "redis-data"}},
            ],
            "annotations": {
                "helm.sh/chart": "redis-7.2.0",
                "meta.helm.sh/release-name": "redis",
            },
            "metadata": _base_metadata("redis", "production", {"app": "redis"}, generation=3),
            "status": _healthy_status(3, 3),
            "spec": {"serviceName": "redis-headless", "podManagementPolicy": "OrderedReady", "updateStrategy": {"type": "RollingUpdate"}},
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
                    "resources": _standard_resources("50m", "128Mi", "500m", "512Mi"),
                    "env": _standard_env("cronjob-worker"),
                    "volumeMounts": [{"name": "app-config", "mountPath": "/etc/worker"}],
                }
            ],
            "volumes": [{"name": "app-config", "configMap": {"name": "cronjob-worker-config"}}],
            "annotations": _common_annotations("cronjob-worker"),
            "metadata": _base_metadata("cronjob-worker", "production", {"app": "cronjob-worker"}, generation=2),
            "status": _healthy_status(1, 2),
            "spec": _default_spec(),
        },
        {
            "name": "order-service",
            "kind": "Deployment",
            "replicas": 3,
            "labels": {"app": "order-service", "team": "commerce", "lang": "go"},
            "containers": [
                {
                    "name": "order-service",
                    "image": "internal/order-service:v3.1.0",
                    "ports": [
                        {"name": "http", "containerPort": 8080, "protocol": "TCP"},
                        {"name": "metrics", "containerPort": 9090, "protocol": "TCP"},
                    ],
                    "resources": _standard_resources("200m", "256Mi", "1000m", "1Gi"),
                    "env": _standard_env("order-service"),
                    **_standard_probes(8080),
                }
            ],
            "initContainers": [_ISTIO_INIT_CONTAINER],
            "sidecarContainers": [_ISTIO_SIDECAR, _LOG_FORWARDER_SIDECAR],
            "volumes": _standard_volumes("order-service"),
            "annotations": {**_common_annotations("order-service"), "prometheus.io/port": "9090"},
            "metadata": _base_metadata("order-service", "production", {"app": "order-service"}, generation=7),
            "status": _healthy_status(3, 7),
            "spec": _default_spec(),
        },
        {
            "name": "inventory-api",
            "kind": "Deployment",
            "replicas": 2,
            "labels": {"app": "inventory-api", "team": "commerce", "lang": "python"},
            "containers": [
                {
                    "name": "inventory-api",
                    "image": "internal/inventory-api:v2.0.5",
                    "ports": [
                        {"name": "http", "containerPort": 8000, "protocol": "TCP"},
                        {"name": "metrics", "containerPort": 9090, "protocol": "TCP"},
                    ],
                    "resources": _standard_resources("150m", "256Mi", "750m", "768Mi"),
                    "env": _standard_env("inventory-api"),
                    **_standard_probes(8000),
                }
            ],
            "initContainers": [_ISTIO_INIT_CONTAINER],
            "sidecarContainers": [_ISTIO_SIDECAR],
            "volumes": _standard_volumes("inventory-api"),
            "annotations": {**_common_annotations("inventory-api"), "prometheus.io/port": "9090"},
            "metadata": _base_metadata("inventory-api", "production", {"app": "inventory-api"}, generation=4),
            "status": _healthy_status(2, 4),
            "spec": _default_spec(),
        },
        {
            "name": "notification-service",
            "kind": "Deployment",
            "replicas": 2,
            "labels": {"app": "notification-service", "team": "platform", "lang": "node"},
            "containers": [
                {
                    "name": "notification-service",
                    "image": "internal/notification-service:v1.5.2",
                    "ports": [
                        {"name": "http", "containerPort": 3000, "protocol": "TCP"},
                        {"name": "metrics", "containerPort": 9090, "protocol": "TCP"},
                    ],
                    "resources": _standard_resources("100m", "128Mi", "500m", "512Mi"),
                    "env": _standard_env("notification-service"),
                    **_standard_probes(3000),
                }
            ],
            "initContainers": [_ISTIO_INIT_CONTAINER],
            "sidecarContainers": [_ISTIO_SIDECAR],
            "volumes": _standard_volumes("notification-service"),
            "annotations": {**_common_annotations("notification-service"), "prometheus.io/port": "9090"},
            "metadata": _base_metadata("notification-service", "production", {"app": "notification-service"}, generation=6),
            "status": _healthy_status(2, 6),
            "spec": _default_spec(),
        },
        {
            "name": "search-service",
            "kind": "Deployment",
            "replicas": 2,
            "labels": {"app": "search-service", "team": "search", "lang": "java"},
            "containers": [
                {
                    "name": "search-service",
                    "image": "internal/search-service:v2.2.0",
                    "ports": [
                        {"name": "http", "containerPort": 8080, "protocol": "TCP"},
                        {"name": "metrics", "containerPort": 8081, "protocol": "TCP"},
                    ],
                    "resources": _standard_resources("500m", "1Gi", "2000m", "4Gi"),
                    "env": _standard_env("search-service") + [
                        {"name": "JAVA_OPTS", "value": "-Xms1g -Xmx3g -XX:+UseG1GC"},
                        {"name": "ELASTICSEARCH_URL", "value": "http://elasticsearch.production.svc:9200"},
                    ],
                    **_standard_probes(8081, "/actuator/health"),
                }
            ],
            "initContainers": [_ISTIO_INIT_CONTAINER],
            "sidecarContainers": [_ISTIO_SIDECAR, _LOG_FORWARDER_SIDECAR],
            "volumes": _standard_volumes("search-service"),
            "annotations": {**_common_annotations("search-service"), "prometheus.io/port": "8081", "prometheus.io/path": "/actuator/prometheus"},
            "metadata": _base_metadata("search-service", "production", {"app": "search-service"}, generation=9),
            "status": _healthy_status(2, 9),
            "spec": _default_spec(),
        },
        {
            "name": "auth-proxy",
            "kind": "Deployment",
            "replicas": 2,
            "labels": {"app": "auth-proxy", "team": "identity", "lang": "go"},
            "containers": [
                {
                    "name": "auth-proxy",
                    "image": "internal/auth-proxy:v1.3.0",
                    "ports": [
                        {"name": "http", "containerPort": 8080, "protocol": "TCP"},
                        {"name": "metrics", "containerPort": 9090, "protocol": "TCP"},
                    ],
                    "resources": _standard_resources("100m", "128Mi", "500m", "256Mi"),
                    "env": _standard_env("auth-proxy") + [
                        {"name": "OAUTH_PROVIDER", "value": "okta"},
                        {"name": "OAUTH_CLIENT_ID", "valueFrom": {"secretKeyRef": {"name": "auth-proxy-secrets", "key": "oauth-client-id"}}},
                    ],
                    **_standard_probes(8080),
                }
            ],
            "initContainers": [_ISTIO_INIT_CONTAINER],
            "sidecarContainers": [_ISTIO_SIDECAR],
            "volumes": _standard_volumes("auth-proxy"),
            "annotations": {**_common_annotations("auth-proxy"), "prometheus.io/port": "9090"},
            "metadata": _base_metadata("auth-proxy", "production", {"app": "auth-proxy"}, generation=4),
            "status": _healthy_status(2, 4),
            "spec": _default_spec(),
        },
        {
            "name": "celery-worker",
            "kind": "Deployment",
            "replicas": 4,
            "labels": {"app": "celery-worker", "team": "batch", "lang": "python"},
            "containers": [
                {
                    "name": "celery-worker",
                    "image": "internal/celery-worker:v2.1.0",
                    "ports": [{"name": "metrics", "containerPort": 9090, "protocol": "TCP"}],
                    "resources": _standard_resources("200m", "512Mi", "1000m", "2Gi"),
                    "env": _standard_env("celery-worker") + [
                        {"name": "CELERY_BROKER_URL", "value": "amqp://rabbitmq.production.svc:5672"},
                        {"name": "CELERY_RESULT_BACKEND", "value": "redis://redis.production.svc:6379/1"},
                        {"name": "CELERY_CONCURRENCY", "value": "4"},
                    ],
                    "livenessProbe": {"exec": {"command": ["celery", "inspect", "ping"]}, "initialDelaySeconds": 30, "periodSeconds": 60},
                }
            ],
            "volumes": _standard_volumes("celery-worker"),
            "annotations": {**_common_annotations("celery-worker"), "prometheus.io/port": "9090"},
            "metadata": _base_metadata("celery-worker", "production", {"app": "celery-worker"}, generation=3),
            "status": _healthy_status(4, 3),
            "spec": _default_spec(),
        },
        {
            "name": "rabbitmq",
            "kind": "StatefulSet",
            "replicas": 3,
            "labels": {"app": "rabbitmq", "team": "platform", "app.kubernetes.io/managed-by": "Helm"},
            "containers": [
                {
                    "name": "rabbitmq",
                    "image": "rabbitmq:3.12-management-alpine",
                    "ports": [
                        {"name": "amqp", "containerPort": 5672, "protocol": "TCP"},
                        {"name": "management", "containerPort": 15672, "protocol": "TCP"},
                        {"name": "metrics", "containerPort": 15692, "protocol": "TCP"},
                    ],
                    "resources": _standard_resources("200m", "512Mi", "1000m", "2Gi"),
                    "env": [
                        {"name": "RABBITMQ_DEFAULT_USER", "valueFrom": {"secretKeyRef": {"name": "rabbitmq-secrets", "key": "username"}}},
                        {"name": "RABBITMQ_DEFAULT_PASS", "valueFrom": {"secretKeyRef": {"name": "rabbitmq-secrets", "key": "password"}}},
                    ],
                    "volumeMounts": [{"name": "rabbitmq-data", "mountPath": "/var/lib/rabbitmq"}],
                    "livenessProbe": {"exec": {"command": ["rabbitmq-diagnostics", "check_running"]}, "initialDelaySeconds": 60, "periodSeconds": 30},
                    "readinessProbe": {"exec": {"command": ["rabbitmq-diagnostics", "check_running"]}, "initialDelaySeconds": 20, "periodSeconds": 10},
                }
            ],
            "volumes": [{"name": "rabbitmq-data", "persistentVolumeClaim": {"claimName": "rabbitmq-data"}}],
            "annotations": {"helm.sh/chart": "rabbitmq-12.0.0", "prometheus.io/scrape": "true", "prometheus.io/port": "15692"},
            "metadata": _base_metadata("rabbitmq", "production", {"app": "rabbitmq"}, generation=2),
            "status": _healthy_status(3, 2),
            "spec": {"serviceName": "rabbitmq-headless", "podManagementPolicy": "OrderedReady", "updateStrategy": {"type": "RollingUpdate"}},
        },
        {
            "name": "postgres",
            "kind": "StatefulSet",
            "replicas": 2,
            "labels": {"app": "postgres", "team": "platform", "app.kubernetes.io/managed-by": "Helm"},
            "containers": [
                {
                    "name": "postgres",
                    "image": "postgres:16.1-alpine",
                    "ports": [{"name": "postgres", "containerPort": 5432, "protocol": "TCP"}],
                    "resources": _standard_resources("500m", "1Gi", "2000m", "4Gi"),
                    "env": [
                        {"name": "POSTGRES_DB", "value": "app"},
                        {"name": "POSTGRES_USER", "valueFrom": {"secretKeyRef": {"name": "postgres-secrets", "key": "username"}}},
                        {"name": "POSTGRES_PASSWORD", "valueFrom": {"secretKeyRef": {"name": "postgres-secrets", "key": "password"}}},
                        {"name": "PGDATA", "value": "/var/lib/postgresql/data/pgdata"},
                    ],
                    "volumeMounts": [{"name": "postgres-data", "mountPath": "/var/lib/postgresql/data"}],
                    "livenessProbe": {"exec": {"command": ["pg_isready", "-U", "postgres"]}, "initialDelaySeconds": 30, "periodSeconds": 10},
                    "readinessProbe": {"exec": {"command": ["pg_isready", "-U", "postgres"]}, "initialDelaySeconds": 5, "periodSeconds": 5},
                }
            ],
            "volumes": [{"name": "postgres-data", "persistentVolumeClaim": {"claimName": "postgres-data"}}],
            "annotations": {"helm.sh/chart": "postgresql-13.0.0"},
            "metadata": _base_metadata("postgres", "production", {"app": "postgres"}, generation=2),
            "status": _healthy_status(2, 2),
            "spec": {"serviceName": "postgres-headless", "podManagementPolicy": "OrderedReady", "updateStrategy": {"type": "RollingUpdate"}},
        },
    ],
    "staging": [
        {
            "name": "payment-service-staging",
            "kind": "Deployment",
            "replicas": 1,
            "labels": {"app": "payment-service", "team": "payments", "lang": "go", "env": "staging"},
            "containers": [
                {
                    "name": "payment-service",
                    "image": "internal/payment-service:v2.4.0-rc1",
                    "ports": [
                        {"name": "http", "containerPort": 8080, "protocol": "TCP"},
                        {"name": "metrics", "containerPort": 9090, "protocol": "TCP"},
                    ],
                    "resources": _standard_resources("100m", "128Mi", "500m", "512Mi"),
                    "env": _standard_env("payment-service"),
                    **_standard_probes(8080),
                }
            ],
            "annotations": {"prometheus.io/scrape": "true", "prometheus.io/port": "9090"},
            "metadata": _base_metadata("payment-service-staging", "staging", {"app": "payment-service"}, generation=20),
            "status": _healthy_status(1, 20),
            "spec": _default_spec(),
        },
        {
            "name": "user-api-staging",
            "kind": "Deployment",
            "replicas": 1,
            "labels": {"app": "user-api", "team": "identity", "lang": "java", "env": "staging"},
            "containers": [
                {
                    "name": "user-api",
                    "image": "internal/user-api:v1.9.0-rc2",
                    "ports": [
                        {"name": "http", "containerPort": 8080, "protocol": "TCP"},
                        {"name": "metrics", "containerPort": 8081, "protocol": "TCP"},
                    ],
                    "resources": _standard_resources("200m", "256Mi", "1000m", "1Gi"),
                    "env": _standard_env("user-api"),
                    **_standard_probes(8081, "/actuator/health"),
                }
            ],
            "annotations": {"prometheus.io/scrape": "true", "prometheus.io/port": "8081", "prometheus.io/path": "/actuator/prometheus"},
            "metadata": _base_metadata("user-api-staging", "staging", {"app": "user-api"}, generation=25),
            "status": _healthy_status(1, 25),
            "spec": _default_spec(),
        },
        {
            "name": "order-service-staging",
            "kind": "Deployment",
            "replicas": 1,
            "labels": {"app": "order-service", "team": "commerce", "lang": "go", "env": "staging"},
            "containers": [
                {
                    "name": "order-service",
                    "image": "internal/order-service:v3.2.0-rc1",
                    "ports": [
                        {"name": "http", "containerPort": 8080, "protocol": "TCP"},
                        {"name": "metrics", "containerPort": 9090, "protocol": "TCP"},
                    ],
                    "resources": _standard_resources("100m", "128Mi", "500m", "512Mi"),
                    "env": _standard_env("order-service"),
                    **_standard_probes(8080),
                }
            ],
            "annotations": {"prometheus.io/scrape": "true", "prometheus.io/port": "9090"},
            "metadata": _base_metadata("order-service-staging", "staging", {"app": "order-service"}, generation=18),
            "status": _healthy_status(1, 18),
            "spec": _default_spec(),
        },
        {
            "name": "search-service-staging",
            "kind": "Deployment",
            "replicas": 1,
            "labels": {"app": "search-service", "team": "search", "lang": "java", "env": "staging"},
            "containers": [
                {
                    "name": "search-service",
                    "image": "internal/search-service:v2.3.0-rc1",
                    "ports": [
                        {"name": "http", "containerPort": 8080, "protocol": "TCP"},
                        {"name": "metrics", "containerPort": 8081, "protocol": "TCP"},
                    ],
                    "resources": _standard_resources("200m", "512Mi", "1000m", "2Gi"),
                    "env": _standard_env("search-service"),
                    **_standard_probes(8081, "/actuator/health"),
                }
            ],
            "annotations": {"prometheus.io/scrape": "true", "prometheus.io/port": "8081"},
            "metadata": _base_metadata("search-service-staging", "staging", {"app": "search-service"}, generation=12),
            "status": _healthy_status(1, 12),
            "spec": _default_spec(),
        },
    ],
    "kube-system": [
        {
            "name": "coredns",
            "kind": "Deployment",
            "replicas": 2,
            "labels": {"k8s-app": "kube-dns", "app": "coredns"},
            "containers": [
                {
                    "name": "coredns",
                    "image": "registry.k8s.io/coredns/coredns:v1.11.1",
                    "ports": [
                        {"name": "dns", "containerPort": 53, "protocol": "UDP"},
                        {"name": "dns-tcp", "containerPort": 53, "protocol": "TCP"},
                        {"name": "metrics", "containerPort": 9153, "protocol": "TCP"},
                    ],
                    "resources": {"limits": {"memory": "170Mi"}, "requests": {"cpu": "100m", "memory": "70Mi"}},
                    "volumeMounts": [{"name": "config-volume", "mountPath": "/etc/coredns", "readOnly": True}],
                    "livenessProbe": {"httpGet": {"path": "/health", "port": 8080}, "initialDelaySeconds": 60, "periodSeconds": 10},
                    "readinessProbe": {"httpGet": {"path": "/ready", "port": 8181}, "periodSeconds": 10},
                }
            ],
            "volumes": [{"name": "config-volume", "configMap": {"name": "coredns", "items": [{"key": "Corefile", "path": "Corefile"}]}}],
            "annotations": {},
            "metadata": _base_metadata("coredns", "kube-system", {"k8s-app": "kube-dns"}, generation=1),
            "status": _healthy_status(2, 1),
            "spec": _default_spec(),
        },
        {
            "name": "kube-proxy",
            "kind": "DaemonSet",
            "replicas": 6,
            "labels": {"k8s-app": "kube-proxy"},
            "containers": [
                {
                    "name": "kube-proxy",
                    "image": "registry.k8s.io/kube-proxy:v1.28.4",
                    "ports": [{"name": "metrics", "containerPort": 10249, "protocol": "TCP"}],
                    "resources": {},
                    "securityContext": {"privileged": True},
                    "volumeMounts": [
                        {"name": "kube-proxy", "mountPath": "/var/lib/kube-proxy"},
                        {"name": "xtables-lock", "mountPath": "/run/xtables.lock"},
                    ],
                }
            ],
            "volumes": [
                {"name": "kube-proxy", "configMap": {"name": "kube-proxy"}},
                {"name": "xtables-lock", "hostPath": {"path": "/run/xtables.lock", "type": "FileOrCreate"}},
            ],
            "annotations": {},
            "metadata": _base_metadata("kube-proxy", "kube-system", {"k8s-app": "kube-proxy"}, generation=1),
            "status": _healthy_status(6, 1),
            "spec": {},
        },
        {
            "name": "metrics-server",
            "kind": "Deployment",
            "replicas": 1,
            "labels": {"k8s-app": "metrics-server"},
            "containers": [
                {
                    "name": "metrics-server",
                    "image": "registry.k8s.io/metrics-server/metrics-server:v0.7.0",
                    "ports": [{"name": "https", "containerPort": 10250, "protocol": "TCP"}],
                    "resources": {"requests": {"cpu": "100m", "memory": "200Mi"}},
                    "args": ["--cert-dir=/tmp", "--secure-port=10250", "--kubelet-preferred-address-types=InternalIP"],
                }
            ],
            "annotations": {},
            "metadata": _base_metadata("metrics-server", "kube-system", {"k8s-app": "metrics-server"}, generation=2),
            "status": _healthy_status(1, 2),
            "spec": _default_spec(),
        },
        {
            "name": "aws-node",
            "kind": "DaemonSet",
            "replicas": 6,
            "labels": {"k8s-app": "aws-node"},
            "containers": [
                {
                    "name": "aws-node",
                    "image": "602401143452.dkr.ecr.us-east-1.amazonaws.com/amazon-k8s-cni:v1.16.0",
                    "ports": [{"name": "metrics", "containerPort": 61678, "protocol": "TCP"}],
                    "resources": {"requests": {"cpu": "25m"}},
                    "env": [
                        {"name": "AWS_VPC_K8S_CNI_LOGLEVEL", "value": "DEBUG"},
                        {"name": "MY_NODE_NAME", "valueFrom": {"fieldRef": {"fieldPath": "spec.nodeName"}}},
                    ],
                    "securityContext": {"capabilities": {"add": ["NET_ADMIN", "NET_RAW"]}},
                }
            ],
            "annotations": {},
            "metadata": _base_metadata("aws-node", "kube-system", {"k8s-app": "aws-node"}, generation=1),
            "status": _healthy_status(6, 1),
            "spec": {},
        },
        {
            "name": "ebs-csi-controller",
            "kind": "Deployment",
            "replicas": 2,
            "labels": {"app": "ebs-csi-controller", "app.kubernetes.io/name": "aws-ebs-csi-driver"},
            "containers": [
                {
                    "name": "ebs-plugin",
                    "image": "602401143452.dkr.ecr.us-east-1.amazonaws.com/eks/aws-ebs-csi-driver:v1.26.1",
                    "ports": [{"name": "healthz", "containerPort": 9808, "protocol": "TCP"}],
                    "resources": {"limits": {"cpu": "100m", "memory": "128Mi"}, "requests": {"cpu": "10m", "memory": "40Mi"}},
                    "livenessProbe": {"httpGet": {"path": "/healthz", "port": "healthz"}, "initialDelaySeconds": 10, "periodSeconds": 10},
                }
            ],
            "annotations": {},
            "metadata": _base_metadata("ebs-csi-controller", "kube-system", {"app": "ebs-csi-controller"}, generation=3),
            "status": _healthy_status(2, 3),
            "spec": _default_spec(),
        },
    ],
    "monitoring": [
        {
            "name": "prometheus-server",
            "kind": "StatefulSet",
            "replicas": 1,
            "labels": {"app": "prometheus", "component": "server"},
            "containers": [
                {
                    "name": "prometheus",
                    "image": "prom/prometheus:v2.49.1",
                    "ports": [{"name": "http", "containerPort": 9090, "protocol": "TCP"}],
                    "resources": _standard_resources("500m", "2Gi", "2000m", "8Gi"),
                    "args": ["--config.file=/etc/prometheus/prometheus.yml", "--storage.tsdb.retention.time=30d", "--web.enable-lifecycle"],
                    "volumeMounts": [
                        {"name": "prometheus-data", "mountPath": "/prometheus"},
                        {"name": "prometheus-config", "mountPath": "/etc/prometheus"},
                    ],
                    "livenessProbe": {"httpGet": {"path": "/-/healthy", "port": 9090}, "initialDelaySeconds": 30, "periodSeconds": 15},
                    "readinessProbe": {"httpGet": {"path": "/-/ready", "port": 9090}, "initialDelaySeconds": 30, "periodSeconds": 5},
                }
            ],
            "volumes": [
                {"name": "prometheus-data", "persistentVolumeClaim": {"claimName": "prometheus-data"}},
                {"name": "prometheus-config", "configMap": {"name": "prometheus-config"}},
            ],
            "annotations": {},
            "metadata": _base_metadata("prometheus-server", "monitoring", {"app": "prometheus"}, generation=5),
            "status": _healthy_status(1, 5),
            "spec": {"serviceName": "prometheus-headless"},
        },
        {
            "name": "grafana",
            "kind": "Deployment",
            "replicas": 1,
            "labels": {"app": "grafana"},
            "containers": [
                {
                    "name": "grafana",
                    "image": "grafana/grafana:10.3.1",
                    "ports": [{"name": "http", "containerPort": 3000, "protocol": "TCP"}],
                    "resources": _standard_resources("100m", "128Mi", "500m", "512Mi"),
                    "env": [
                        {"name": "GF_SECURITY_ADMIN_PASSWORD", "valueFrom": {"secretKeyRef": {"name": "grafana-secrets", "key": "admin-password"}}},
                    ],
                    "volumeMounts": [
                        {"name": "grafana-data", "mountPath": "/var/lib/grafana"},
                        {"name": "grafana-dashboards", "mountPath": "/etc/grafana/provisioning/dashboards"},
                    ],
                    **_standard_probes(3000, "/api/health"),
                }
            ],
            "volumes": [
                {"name": "grafana-data", "persistentVolumeClaim": {"claimName": "grafana-data"}},
                {"name": "grafana-dashboards", "configMap": {"name": "grafana-dashboards"}},
            ],
            "annotations": {},
            "metadata": _base_metadata("grafana", "monitoring", {"app": "grafana"}, generation=3),
            "status": _healthy_status(1, 3),
            "spec": _default_spec(),
        },
        {
            "name": "alertmanager",
            "kind": "StatefulSet",
            "replicas": 3,
            "labels": {"app": "alertmanager"},
            "containers": [
                {
                    "name": "alertmanager",
                    "image": "prom/alertmanager:v0.27.0",
                    "ports": [{"name": "http", "containerPort": 9093, "protocol": "TCP"}],
                    "resources": _standard_resources("50m", "64Mi", "200m", "256Mi"),
                    "args": ["--config.file=/etc/alertmanager/alertmanager.yml", "--cluster.listen-address=0.0.0.0:9094"],
                    "volumeMounts": [{"name": "alertmanager-config", "mountPath": "/etc/alertmanager"}],
                }
            ],
            "volumes": [{"name": "alertmanager-config", "configMap": {"name": "alertmanager-config"}}],
            "annotations": {},
            "metadata": _base_metadata("alertmanager", "monitoring", {"app": "alertmanager"}, generation=2),
            "status": _healthy_status(3, 2),
            "spec": {"serviceName": "alertmanager-headless"},
        },
        {
            "name": "node-exporter",
            "kind": "DaemonSet",
            "replicas": 6,
            "labels": {"app": "node-exporter"},
            "containers": [
                {
                    "name": "node-exporter",
                    "image": "prom/node-exporter:v1.7.0",
                    "ports": [{"name": "metrics", "containerPort": 9100, "protocol": "TCP"}],
                    "resources": {"limits": {"cpu": "250m", "memory": "180Mi"}, "requests": {"cpu": "102m", "memory": "180Mi"}},
                    "args": ["--path.procfs=/host/proc", "--path.sysfs=/host/sys", "--path.rootfs=/host/root"],
                }
            ],
            "annotations": {"prometheus.io/scrape": "true", "prometheus.io/port": "9100"},
            "metadata": _base_metadata("node-exporter", "monitoring", {"app": "node-exporter"}, generation=1),
            "status": _healthy_status(6, 1),
            "spec": {},
        },
        {
            "name": "kube-state-metrics",
            "kind": "Deployment",
            "replicas": 1,
            "labels": {"app": "kube-state-metrics"},
            "containers": [
                {
                    "name": "kube-state-metrics",
                    "image": "registry.k8s.io/kube-state-metrics/kube-state-metrics:v2.10.1",
                    "ports": [
                        {"name": "http-metrics", "containerPort": 8080, "protocol": "TCP"},
                        {"name": "telemetry", "containerPort": 8081, "protocol": "TCP"},
                    ],
                    "resources": {"limits": {"cpu": "100m", "memory": "128Mi"}, "requests": {"cpu": "10m", "memory": "64Mi"}},
                    "livenessProbe": {"httpGet": {"path": "/healthz", "port": 8080}, "initialDelaySeconds": 5, "periodSeconds": 10},
                    "readinessProbe": {"httpGet": {"path": "/", "port": 8081}, "initialDelaySeconds": 5, "periodSeconds": 10},
                }
            ],
            "annotations": {"prometheus.io/scrape": "true", "prometheus.io/port": "8080"},
            "metadata": _base_metadata("kube-state-metrics", "monitoring", {"app": "kube-state-metrics"}, generation=2),
            "status": _healthy_status(1, 2),
            "spec": _default_spec(),
        },
    ],
    "cert-manager": [
        {
            "name": "cert-manager",
            "kind": "Deployment",
            "replicas": 1,
            "labels": {"app": "cert-manager", "app.kubernetes.io/name": "cert-manager"},
            "containers": [
                {
                    "name": "cert-manager-controller",
                    "image": "quay.io/jetstack/cert-manager-controller:v1.14.2",
                    "ports": [{"name": "http-metrics", "containerPort": 9402, "protocol": "TCP"}],
                    "resources": {"requests": {"cpu": "10m", "memory": "32Mi"}},
                    "args": ["--v=2", "--cluster-resource-namespace=$(POD_NAMESPACE)", "--leader-election-namespace=kube-system"],
                }
            ],
            "annotations": {"prometheus.io/scrape": "true", "prometheus.io/port": "9402"},
            "metadata": _base_metadata("cert-manager", "cert-manager", {"app": "cert-manager"}, generation=3),
            "status": _healthy_status(1, 3),
            "spec": _default_spec(),
        },
        {
            "name": "cert-manager-webhook",
            "kind": "Deployment",
            "replicas": 1,
            "labels": {"app": "cert-manager", "app.kubernetes.io/component": "webhook"},
            "containers": [
                {
                    "name": "cert-manager-webhook",
                    "image": "quay.io/jetstack/cert-manager-webhook:v1.14.2",
                    "ports": [{"name": "https", "containerPort": 10250, "protocol": "TCP"}],
                    "resources": {"requests": {"cpu": "10m", "memory": "32Mi"}},
                    "livenessProbe": {"httpGet": {"path": "/livez", "port": 6080}, "initialDelaySeconds": 60, "periodSeconds": 10},
                }
            ],
            "annotations": {},
            "metadata": _base_metadata("cert-manager-webhook", "cert-manager", {"app": "cert-manager"}, generation=3),
            "status": _healthy_status(1, 3),
            "spec": _default_spec(),
        },
    ],
    "istio-system": [
        {
            "name": "istiod",
            "kind": "Deployment",
            "replicas": 2,
            "labels": {"app": "istiod", "istio": "pilot"},
            "containers": [
                {
                    "name": "discovery",
                    "image": "docker.io/istio/pilot:1.20.2",
                    "ports": [
                        {"name": "http-monitoring", "containerPort": 15014, "protocol": "TCP"},
                        {"name": "grpc-xds", "containerPort": 15010, "protocol": "TCP"},
                        {"name": "https-dns", "containerPort": 15012, "protocol": "TCP"},
                        {"name": "https-webhook", "containerPort": 15017, "protocol": "TCP"},
                    ],
                    "resources": _standard_resources("500m", "2Gi", "2000m", "4Gi"),
                    "readinessProbe": {"httpGet": {"path": "/ready", "port": 8080}, "initialDelaySeconds": 1, "periodSeconds": 3},
                }
            ],
            "annotations": {"prometheus.io/scrape": "true", "prometheus.io/port": "15014"},
            "metadata": _base_metadata("istiod", "istio-system", {"app": "istiod"}, generation=5),
            "status": _healthy_status(2, 5),
            "spec": _default_spec(),
        },
        {
            "name": "istio-ingressgateway",
            "kind": "Deployment",
            "replicas": 2,
            "labels": {"app": "istio-ingressgateway", "istio": "ingressgateway"},
            "containers": [
                {
                    "name": "istio-proxy",
                    "image": "docker.io/istio/proxyv2:1.20.2",
                    "ports": [
                        {"name": "http2", "containerPort": 8080, "protocol": "TCP"},
                        {"name": "https", "containerPort": 8443, "protocol": "TCP"},
                        {"name": "status-port", "containerPort": 15021, "protocol": "TCP"},
                        {"name": "http-envoy-prom", "containerPort": 15090, "protocol": "TCP"},
                    ],
                    "resources": _standard_resources("100m", "128Mi", "2000m", "1Gi"),
                }
            ],
            "annotations": {"prometheus.io/scrape": "true", "prometheus.io/port": "15090"},
            "metadata": _base_metadata("istio-ingressgateway", "istio-system", {"app": "istio-ingressgateway"}, generation=4),
            "status": _healthy_status(2, 4),
            "spec": _default_spec(),
        },
    ],
    "logging": [
        {
            "name": "fluentd",
            "kind": "DaemonSet",
            "replicas": 6,
            "labels": {"app": "fluentd", "k8s-app": "fluentd-logging"},
            "containers": [
                {
                    "name": "fluentd",
                    "image": "fluent/fluentd-kubernetes-daemonset:v1.16-debian-elasticsearch8-1",
                    "ports": [{"name": "metrics", "containerPort": 24231, "protocol": "TCP"}],
                    "resources": _standard_resources("200m", "256Mi", "1000m", "1Gi"),
                    "env": [
                        {"name": "FLUENT_ELASTICSEARCH_HOST", "value": "elasticsearch.logging.svc"},
                        {"name": "FLUENT_ELASTICSEARCH_PORT", "value": "9200"},
                    ],
                    "volumeMounts": [
                        {"name": "varlog", "mountPath": "/var/log"},
                        {"name": "dockercontainers", "mountPath": "/var/lib/docker/containers", "readOnly": True},
                    ],
                }
            ],
            "volumes": [
                {"name": "varlog", "hostPath": {"path": "/var/log"}},
                {"name": "dockercontainers", "hostPath": {"path": "/var/lib/docker/containers"}},
            ],
            "annotations": {"prometheus.io/scrape": "true", "prometheus.io/port": "24231"},
            "metadata": _base_metadata("fluentd", "logging", {"app": "fluentd"}, generation=2),
            "status": _healthy_status(6, 2),
            "spec": {},
        },
        {
            "name": "elasticsearch",
            "kind": "StatefulSet",
            "replicas": 3,
            "labels": {"app": "elasticsearch"},
            "containers": [
                {
                    "name": "elasticsearch",
                    "image": "docker.elastic.co/elasticsearch/elasticsearch:8.12.0",
                    "ports": [
                        {"name": "http", "containerPort": 9200, "protocol": "TCP"},
                        {"name": "transport", "containerPort": 9300, "protocol": "TCP"},
                    ],
                    "resources": _standard_resources("1000m", "4Gi", "4000m", "8Gi"),
                    "env": [
                        {"name": "cluster.name", "value": "logs"},
                        {"name": "discovery.seed_hosts", "value": "elasticsearch-headless"},
                        {"name": "ES_JAVA_OPTS", "value": "-Xms4g -Xmx4g"},
                    ],
                    "volumeMounts": [{"name": "elasticsearch-data", "mountPath": "/usr/share/elasticsearch/data"}],
                    "readinessProbe": {"httpGet": {"path": "/_cluster/health?local=true", "port": 9200}, "initialDelaySeconds": 30, "periodSeconds": 10},
                }
            ],
            "volumes": [{"name": "elasticsearch-data", "persistentVolumeClaim": {"claimName": "elasticsearch-data"}}],
            "annotations": {},
            "metadata": _base_metadata("elasticsearch", "logging", {"app": "elasticsearch"}, generation=2),
            "status": _healthy_status(3, 2),
            "spec": {"serviceName": "elasticsearch-headless"},
        },
    ],
}

# ---------------------------------------------------------------------------
# Services
# ---------------------------------------------------------------------------

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
                {"name": "grpc", "port": 9000, "targetPort": 9000},
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
        {
            "name": "redis-headless",
            "type": "ClusterIP",
            "clusterIP": "None",
            "ports": [{"name": "redis", "port": 6379, "targetPort": 6379}],
            "selector": {"app": "redis"},
        },
        {
            "name": "order-service",
            "type": "ClusterIP",
            "ports": [
                {"name": "http", "port": 8080, "targetPort": 8080},
                {"name": "metrics", "port": 9090, "targetPort": 9090},
            ],
            "selector": {"app": "order-service"},
        },
        {
            "name": "inventory-api",
            "type": "ClusterIP",
            "ports": [
                {"name": "http", "port": 8000, "targetPort": 8000},
                {"name": "metrics", "port": 9090, "targetPort": 9090},
            ],
            "selector": {"app": "inventory-api"},
        },
        {
            "name": "notification-service",
            "type": "ClusterIP",
            "ports": [
                {"name": "http", "port": 3000, "targetPort": 3000},
                {"name": "metrics", "port": 9090, "targetPort": 9090},
            ],
            "selector": {"app": "notification-service"},
        },
        {
            "name": "search-service",
            "type": "ClusterIP",
            "ports": [
                {"name": "http", "port": 8080, "targetPort": 8080},
                {"name": "metrics", "port": 8081, "targetPort": 8081},
            ],
            "selector": {"app": "search-service"},
        },
        {
            "name": "auth-proxy",
            "type": "ClusterIP",
            "ports": [
                {"name": "http", "port": 8080, "targetPort": 8080},
                {"name": "metrics", "port": 9090, "targetPort": 9090},
            ],
            "selector": {"app": "auth-proxy"},
        },
        {
            "name": "rabbitmq",
            "type": "ClusterIP",
            "ports": [
                {"name": "amqp", "port": 5672, "targetPort": 5672},
                {"name": "management", "port": 15672, "targetPort": 15672},
                {"name": "metrics", "port": 15692, "targetPort": 15692},
            ],
            "selector": {"app": "rabbitmq"},
        },
        {
            "name": "rabbitmq-headless",
            "type": "ClusterIP",
            "clusterIP": "None",
            "ports": [{"name": "amqp", "port": 5672, "targetPort": 5672}],
            "selector": {"app": "rabbitmq"},
        },
        {
            "name": "postgres",
            "type": "ClusterIP",
            "ports": [{"name": "postgres", "port": 5432, "targetPort": 5432}],
            "selector": {"app": "postgres"},
        },
        {
            "name": "postgres-headless",
            "type": "ClusterIP",
            "clusterIP": "None",
            "ports": [{"name": "postgres", "port": 5432, "targetPort": 5432}],
            "selector": {"app": "postgres"},
        },
    ],
    "staging": [
        {
            "name": "payment-service-staging",
            "type": "ClusterIP",
            "ports": [
                {"name": "http", "port": 8080, "targetPort": 8080},
                {"name": "metrics", "port": 9090, "targetPort": 9090},
            ],
            "selector": {"app": "payment-service"},
        },
        {
            "name": "user-api-staging",
            "type": "ClusterIP",
            "ports": [
                {"name": "http", "port": 8080, "targetPort": 8080},
                {"name": "metrics", "port": 8081, "targetPort": 8081},
            ],
            "selector": {"app": "user-api"},
        },
        {
            "name": "order-service-staging",
            "type": "ClusterIP",
            "ports": [
                {"name": "http", "port": 8080, "targetPort": 8080},
                {"name": "metrics", "port": 9090, "targetPort": 9090},
            ],
            "selector": {"app": "order-service"},
        },
    ],
    "monitoring": [
        {
            "name": "prometheus",
            "type": "ClusterIP",
            "ports": [{"name": "http", "port": 9090, "targetPort": 9090}],
            "selector": {"app": "prometheus", "component": "server"},
        },
        {
            "name": "grafana",
            "type": "ClusterIP",
            "ports": [{"name": "http", "port": 3000, "targetPort": 3000}],
            "selector": {"app": "grafana"},
        },
        {
            "name": "alertmanager",
            "type": "ClusterIP",
            "ports": [{"name": "http", "port": 9093, "targetPort": 9093}],
            "selector": {"app": "alertmanager"},
        },
        {
            "name": "node-exporter",
            "type": "ClusterIP",
            "ports": [{"name": "metrics", "port": 9100, "targetPort": 9100}],
            "selector": {"app": "node-exporter"},
        },
        {
            "name": "kube-state-metrics",
            "type": "ClusterIP",
            "ports": [
                {"name": "http-metrics", "port": 8080, "targetPort": 8080},
                {"name": "telemetry", "port": 8081, "targetPort": 8081},
            ],
            "selector": {"app": "kube-state-metrics"},
        },
    ],
    "kube-system": [
        {
            "name": "kube-dns",
            "type": "ClusterIP",
            "clusterIP": "10.96.0.10",
            "ports": [
                {"name": "dns", "port": 53, "targetPort": 53, "protocol": "UDP"},
                {"name": "dns-tcp", "port": 53, "targetPort": 53, "protocol": "TCP"},
                {"name": "metrics", "port": 9153, "targetPort": 9153},
            ],
            "selector": {"k8s-app": "kube-dns"},
        },
        {
            "name": "metrics-server",
            "type": "ClusterIP",
            "ports": [{"name": "https", "port": 443, "targetPort": 10250}],
            "selector": {"k8s-app": "metrics-server"},
        },
    ],
    "cert-manager": [
        {
            "name": "cert-manager",
            "type": "ClusterIP",
            "ports": [{"name": "http-metrics", "port": 9402, "targetPort": 9402}],
            "selector": {"app": "cert-manager"},
        },
        {
            "name": "cert-manager-webhook",
            "type": "ClusterIP",
            "ports": [{"name": "https", "port": 443, "targetPort": 10250}],
            "selector": {"app": "cert-manager", "app.kubernetes.io/component": "webhook"},
        },
    ],
    "istio-system": [
        {
            "name": "istiod",
            "type": "ClusterIP",
            "ports": [
                {"name": "grpc-xds", "port": 15010, "targetPort": 15010},
                {"name": "https-dns", "port": 15012, "targetPort": 15012},
                {"name": "https-webhook", "port": 443, "targetPort": 15017},
                {"name": "http-monitoring", "port": 15014, "targetPort": 15014},
            ],
            "selector": {"app": "istiod"},
        },
        {
            "name": "istio-ingressgateway",
            "type": "LoadBalancer",
            "ports": [
                {"name": "http2", "port": 80, "targetPort": 8080},
                {"name": "https", "port": 443, "targetPort": 8443},
                {"name": "status-port", "port": 15021, "targetPort": 15021},
            ],
            "selector": {"app": "istio-ingressgateway"},
        },
    ],
    "logging": [
        {
            "name": "elasticsearch",
            "type": "ClusterIP",
            "ports": [
                {"name": "http", "port": 9200, "targetPort": 9200},
                {"name": "transport", "port": 9300, "targetPort": 9300},
            ],
            "selector": {"app": "elasticsearch"},
        },
        {
            "name": "elasticsearch-headless",
            "type": "ClusterIP",
            "clusterIP": "None",
            "ports": [
                {"name": "http", "port": 9200, "targetPort": 9200},
                {"name": "transport", "port": 9300, "targetPort": 9300},
            ],
            "selector": {"app": "elasticsearch"},
        },
    ],
}

# Pre-existing ServiceMonitors (verbose: more monitors across namespaces)
CANNED_SERVICE_MONITORS = {
    "production": [
        {
            "name": "nginx-ingress-monitor",
            "namespace": "production",
            "labels": {"team": "platform"},
            "selector": {"matchLabels": {"app": "nginx-ingress"}},
            "endpoints": [{"port": "metrics", "interval": "30s", "path": "/metrics"}],
        },
        {
            "name": "payment-service-monitor",
            "namespace": "production",
            "labels": {"team": "payments"},
            "selector": {"matchLabels": {"app": "payment-service"}},
            "endpoints": [{"port": "metrics", "interval": "30s", "path": "/metrics"}],
        },
    ],
    "monitoring": [
        {
            "name": "prometheus-self-monitor",
            "namespace": "monitoring",
            "labels": {"team": "platform"},
            "selector": {"matchLabels": {"app": "prometheus"}},
            "endpoints": [{"port": "http", "interval": "15s", "path": "/metrics"}],
        },
    ],
}

# ---------------------------------------------------------------------------
# Prometheus metric metadata (expanded)
# ---------------------------------------------------------------------------

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
    "nginx_ingress_controller_response_size_bytes": {
        "type": "histogram",
        "help": "Response size in bytes",
        "unit": "bytes",
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
    "jvm_gc_pause_seconds": {
        "type": "summary",
        "help": "JVM garbage collection pause time",
        "unit": "seconds",
    },
    "go_goroutines": {
        "type": "gauge",
        "help": "Number of goroutines",
        "unit": "",
    },
    "go_gc_duration_seconds": {
        "type": "summary",
        "help": "Go garbage collection duration",
        "unit": "seconds",
    },
    "container_cpu_usage_seconds_total": {
        "type": "counter",
        "help": "Cumulative cpu time consumed by container",
        "unit": "seconds",
    },
    "container_memory_usage_bytes": {
        "type": "gauge",
        "help": "Current memory usage in bytes including cache",
        "unit": "bytes",
    },
    "kube_pod_container_status_restarts_total": {
        "type": "counter",
        "help": "Number of restarts for the container",
        "unit": "",
    },
    "kube_deployment_status_replicas_available": {
        "type": "gauge",
        "help": "Number of available replicas per deployment",
        "unit": "",
    },
    "node_cpu_seconds_total": {
        "type": "counter",
        "help": "Seconds the cpus spent in each mode",
        "unit": "seconds",
    },
    "node_memory_MemAvailable_bytes": {
        "type": "gauge",
        "help": "Available memory in bytes",
        "unit": "bytes",
    },
    "node_filesystem_avail_bytes": {
        "type": "gauge",
        "help": "Available filesystem space in bytes",
        "unit": "bytes",
    },
}

# ---------------------------------------------------------------------------
# Prometheus query data (dict keyed by query substring -> results)
# ---------------------------------------------------------------------------

PROMETHEUS_QUERY_DATA = {
    "up": [
        {"metric": {"__name__": "up", "job": "nginx-ingress", "namespace": "production", "instance": "nginx-ingress-abc12:10254"}, "value": [1709000000, "1"]},
        {"metric": {"__name__": "up", "job": "payment-service", "namespace": "production", "instance": "payment-service-def34:9090"}, "value": [1709000000, "1"]},
        {"metric": {"__name__": "up", "job": "user-api", "namespace": "production", "instance": "user-api-ghi56:8081"}, "value": [1709000000, "1"]},
        {"metric": {"__name__": "up", "job": "order-service", "namespace": "production", "instance": "order-service-jkl78:9090"}, "value": [1709000000, "1"]},
        {"metric": {"__name__": "up", "job": "inventory-api", "namespace": "production", "instance": "inventory-api-mno90:9090"}, "value": [1709000000, "1"]},
        {"metric": {"__name__": "up", "job": "notification-service", "namespace": "production", "instance": "notification-service-pqr12:9090"}, "value": [1709000000, "1"]},
        {"metric": {"__name__": "up", "job": "search-service", "namespace": "production", "instance": "search-service-stu34:8081"}, "value": [1709000000, "1"]},
        {"metric": {"__name__": "up", "job": "auth-proxy", "namespace": "production", "instance": "auth-proxy-vwx56:9090"}, "value": [1709000000, "1"]},
        {"metric": {"__name__": "up", "job": "celery-worker", "namespace": "production", "instance": "celery-worker-yza78:9090"}, "value": [1709000000, "1"]},
        {"metric": {"__name__": "up", "job": "rabbitmq", "namespace": "production", "instance": "rabbitmq-0:15692"}, "value": [1709000000, "1"]},
        {"metric": {"__name__": "up", "job": "rabbitmq", "namespace": "production", "instance": "rabbitmq-1:15692"}, "value": [1709000000, "1"]},
        {"metric": {"__name__": "up", "job": "rabbitmq", "namespace": "production", "instance": "rabbitmq-2:15692"}, "value": [1709000000, "1"]},
        {"metric": {"__name__": "up", "job": "prometheus", "namespace": "monitoring", "instance": "prometheus-server-0:9090"}, "value": [1709000000, "1"]},
        {"metric": {"__name__": "up", "job": "alertmanager", "namespace": "monitoring", "instance": "alertmanager-0:9093"}, "value": [1709000000, "1"]},
        {"metric": {"__name__": "up", "job": "grafana", "namespace": "monitoring", "instance": "grafana-abc12:3000"}, "value": [1709000000, "1"]},
        {"metric": {"__name__": "up", "job": "node-exporter", "namespace": "monitoring", "instance": "10.0.1.10:9100"}, "value": [1709000000, "1"]},
        {"metric": {"__name__": "up", "job": "node-exporter", "namespace": "monitoring", "instance": "10.0.1.11:9100"}, "value": [1709000000, "1"]},
        {"metric": {"__name__": "up", "job": "node-exporter", "namespace": "monitoring", "instance": "10.0.1.12:9100"}, "value": [1709000000, "1"]},
        {"metric": {"__name__": "up", "job": "kube-state-metrics", "namespace": "monitoring", "instance": "kube-state-metrics-xyz99:8080"}, "value": [1709000000, "1"]},
        {"metric": {"__name__": "up", "job": "istiod", "namespace": "istio-system", "instance": "istiod-abc12:15014"}, "value": [1709000000, "1"]},
    ],
    "nginx_ingress_controller_requests_total": [
        {"metric": {"__name__": "nginx_ingress_controller_requests_total", "status": "200", "namespace": "production", "ingress": "payment-service", "pod": "nginx-ingress-abc12"}, "value": [1709000000, "125432"]},
        {"metric": {"__name__": "nginx_ingress_controller_requests_total", "status": "200", "namespace": "production", "ingress": "user-api", "pod": "nginx-ingress-abc12"}, "value": [1709000000, "89201"]},
        {"metric": {"__name__": "nginx_ingress_controller_requests_total", "status": "200", "namespace": "production", "ingress": "order-service", "pod": "nginx-ingress-abc12"}, "value": [1709000000, "67432"]},
        {"metric": {"__name__": "nginx_ingress_controller_requests_total", "status": "201", "namespace": "production", "ingress": "payment-service", "pod": "nginx-ingress-abc12"}, "value": [1709000000, "15234"]},
        {"metric": {"__name__": "nginx_ingress_controller_requests_total", "status": "301", "namespace": "production", "ingress": "search-service", "pod": "nginx-ingress-abc12"}, "value": [1709000000, "3421"]},
        {"metric": {"__name__": "nginx_ingress_controller_requests_total", "status": "400", "namespace": "production", "ingress": "payment-service", "pod": "nginx-ingress-abc12"}, "value": [1709000000, "1523"]},
        {"metric": {"__name__": "nginx_ingress_controller_requests_total", "status": "404", "namespace": "production", "ingress": "user-api", "pod": "nginx-ingress-abc12"}, "value": [1709000000, "892"]},
        {"metric": {"__name__": "nginx_ingress_controller_requests_total", "status": "500", "namespace": "production", "ingress": "payment-service", "pod": "nginx-ingress-abc12"}, "value": [1709000000, "342"]},
        {"metric": {"__name__": "nginx_ingress_controller_requests_total", "status": "500", "namespace": "production", "ingress": "order-service", "pod": "nginx-ingress-abc12"}, "value": [1709000000, "128"]},
        {"metric": {"__name__": "nginx_ingress_controller_requests_total", "status": "502", "namespace": "production", "ingress": "search-service", "pod": "nginx-ingress-abc12"}, "value": [1709000000, "56"]},
        {"metric": {"__name__": "nginx_ingress_controller_requests_total", "status": "503", "namespace": "production", "ingress": "user-api", "pod": "nginx-ingress-abc12"}, "value": [1709000000, "23"]},
        {"metric": {"__name__": "nginx_ingress_controller_requests_total", "status": "200", "namespace": "production", "ingress": "payment-service", "pod": "nginx-ingress-def34"}, "value": [1709000000, "118920"]},
        {"metric": {"__name__": "nginx_ingress_controller_requests_total", "status": "500", "namespace": "production", "ingress": "payment-service", "pod": "nginx-ingress-def34"}, "value": [1709000000, "298"]},
        {"metric": {"__name__": "nginx_ingress_controller_requests_total", "status": "200", "namespace": "production", "ingress": "payment-service", "pod": "nginx-ingress-ghi56"}, "value": [1709000000, "112345"]},
        {"metric": {"__name__": "nginx_ingress_controller_requests_total", "status": "500", "namespace": "production", "ingress": "payment-service", "pod": "nginx-ingress-ghi56"}, "value": [1709000000, "312"]},
    ],
    "http_requests_total": [
        {"metric": {"__name__": "http_requests_total", "job": "payment-service", "namespace": "production", "method": "GET", "path": "/api/v1/payments", "pod": "payment-service-abc12"}, "value": [1709000000, "34521"]},
        {"metric": {"__name__": "http_requests_total", "job": "payment-service", "namespace": "production", "method": "POST", "path": "/api/v1/payments", "pod": "payment-service-abc12"}, "value": [1709000000, "12345"]},
        {"metric": {"__name__": "http_requests_total", "job": "payment-service", "namespace": "production", "method": "GET", "path": "/api/v1/payments", "pod": "payment-service-def34"}, "value": [1709000000, "31209"]},
        {"metric": {"__name__": "http_requests_total", "job": "payment-service", "namespace": "production", "method": "POST", "path": "/api/v1/payments", "pod": "payment-service-def34"}, "value": [1709000000, "11832"]},
        {"metric": {"__name__": "http_requests_total", "job": "user-api", "namespace": "production", "method": "GET", "path": "/api/v1/users", "pod": "user-api-abc12"}, "value": [1709000000, "54321"]},
        {"metric": {"__name__": "http_requests_total", "job": "user-api", "namespace": "production", "method": "POST", "path": "/api/v1/users", "pod": "user-api-abc12"}, "value": [1709000000, "8432"]},
        {"metric": {"__name__": "http_requests_total", "job": "user-api", "namespace": "production", "method": "GET", "path": "/api/v1/users", "pod": "user-api-def34"}, "value": [1709000000, "52198"]},
        {"metric": {"__name__": "http_requests_total", "job": "order-service", "namespace": "production", "method": "GET", "path": "/api/v1/orders", "pod": "order-service-abc12"}, "value": [1709000000, "28765"]},
        {"metric": {"__name__": "http_requests_total", "job": "order-service", "namespace": "production", "method": "POST", "path": "/api/v1/orders", "pod": "order-service-abc12"}, "value": [1709000000, "9876"]},
        {"metric": {"__name__": "http_requests_total", "job": "inventory-api", "namespace": "production", "method": "GET", "path": "/api/v1/inventory", "pod": "inventory-api-abc12"}, "value": [1709000000, "42100"]},
        {"metric": {"__name__": "http_requests_total", "job": "search-service", "namespace": "production", "method": "GET", "path": "/api/v1/search", "pod": "search-service-abc12"}, "value": [1709000000, "67890"]},
        {"metric": {"__name__": "http_requests_total", "job": "auth-proxy", "namespace": "production", "method": "GET", "path": "/oauth/callback", "pod": "auth-proxy-abc12"}, "value": [1709000000, "15678"]},
    ],
    "http_request_duration_seconds": [
        {"metric": {"__name__": "http_request_duration_seconds", "job": "payment-service", "quantile": "0.5", "pod": "payment-service-abc12"}, "value": [1709000000, "0.045"]},
        {"metric": {"__name__": "http_request_duration_seconds", "job": "payment-service", "quantile": "0.9", "pod": "payment-service-abc12"}, "value": [1709000000, "0.152"]},
        {"metric": {"__name__": "http_request_duration_seconds", "job": "payment-service", "quantile": "0.99", "pod": "payment-service-abc12"}, "value": [1709000000, "0.245"]},
        {"metric": {"__name__": "http_request_duration_seconds", "job": "payment-service", "quantile": "0.5", "pod": "payment-service-def34"}, "value": [1709000000, "0.048"]},
        {"metric": {"__name__": "http_request_duration_seconds", "job": "payment-service", "quantile": "0.9", "pod": "payment-service-def34"}, "value": [1709000000, "0.161"]},
        {"metric": {"__name__": "http_request_duration_seconds", "job": "payment-service", "quantile": "0.99", "pod": "payment-service-def34"}, "value": [1709000000, "0.267"]},
        {"metric": {"__name__": "http_request_duration_seconds", "job": "user-api", "quantile": "0.5", "pod": "user-api-abc12"}, "value": [1709000000, "0.032"]},
        {"metric": {"__name__": "http_request_duration_seconds", "job": "user-api", "quantile": "0.9", "pod": "user-api-abc12"}, "value": [1709000000, "0.098"]},
        {"metric": {"__name__": "http_request_duration_seconds", "job": "user-api", "quantile": "0.99", "pod": "user-api-abc12"}, "value": [1709000000, "0.198"]},
        {"metric": {"__name__": "http_request_duration_seconds", "job": "order-service", "quantile": "0.5", "pod": "order-service-abc12"}, "value": [1709000000, "0.038"]},
        {"metric": {"__name__": "http_request_duration_seconds", "job": "order-service", "quantile": "0.99", "pod": "order-service-abc12"}, "value": [1709000000, "0.312"]},
        {"metric": {"__name__": "http_request_duration_seconds", "job": "search-service", "quantile": "0.5", "pod": "search-service-abc12"}, "value": [1709000000, "0.125"]},
        {"metric": {"__name__": "http_request_duration_seconds", "job": "search-service", "quantile": "0.99", "pod": "search-service-abc12"}, "value": [1709000000, "0.892"]},
    ],
    "process_resident_memory_bytes": [
        {"metric": {"__name__": "process_resident_memory_bytes", "job": "payment-service", "pod": "payment-service-abc12", "container": "payment-service"}, "value": [1709000000, "134217728"]},
        {"metric": {"__name__": "process_resident_memory_bytes", "job": "payment-service", "pod": "payment-service-abc12", "container": "istio-proxy"}, "value": [1709000000, "67108864"]},
        {"metric": {"__name__": "process_resident_memory_bytes", "job": "payment-service", "pod": "payment-service-def34", "container": "payment-service"}, "value": [1709000000, "142606336"]},
        {"metric": {"__name__": "process_resident_memory_bytes", "job": "user-api", "pod": "user-api-abc12", "container": "user-api"}, "value": [1709000000, "536870912"]},
        {"metric": {"__name__": "process_resident_memory_bytes", "job": "user-api", "pod": "user-api-abc12", "container": "istio-proxy"}, "value": [1709000000, "71303168"]},
        {"metric": {"__name__": "process_resident_memory_bytes", "job": "order-service", "pod": "order-service-abc12", "container": "order-service"}, "value": [1709000000, "157286400"]},
        {"metric": {"__name__": "process_resident_memory_bytes", "job": "search-service", "pod": "search-service-abc12", "container": "search-service"}, "value": [1709000000, "1073741824"]},
        {"metric": {"__name__": "process_resident_memory_bytes", "job": "celery-worker", "pod": "celery-worker-abc12", "container": "celery-worker"}, "value": [1709000000, "268435456"]},
        {"metric": {"__name__": "process_resident_memory_bytes", "job": "nginx-ingress", "pod": "nginx-ingress-abc12", "container": "nginx-ingress-controller"}, "value": [1709000000, "104857600"]},
    ],
    "go_goroutines": [
        {"metric": {"__name__": "go_goroutines", "job": "payment-service", "pod": "payment-service-abc12"}, "value": [1709000000, "42"]},
        {"metric": {"__name__": "go_goroutines", "job": "payment-service", "pod": "payment-service-def34"}, "value": [1709000000, "38"]},
        {"metric": {"__name__": "go_goroutines", "job": "order-service", "pod": "order-service-abc12"}, "value": [1709000000, "55"]},
        {"metric": {"__name__": "go_goroutines", "job": "order-service", "pod": "order-service-def34"}, "value": [1709000000, "48"]},
        {"metric": {"__name__": "go_goroutines", "job": "auth-proxy", "pod": "auth-proxy-abc12"}, "value": [1709000000, "23"]},
        {"metric": {"__name__": "go_goroutines", "job": "auth-proxy", "pod": "auth-proxy-def34"}, "value": [1709000000, "21"]},
    ],
    "jvm_memory_used_bytes": [
        {"metric": {"__name__": "jvm_memory_used_bytes", "job": "user-api", "area": "heap", "pod": "user-api-abc12"}, "value": [1709000000, "268435456"]},
        {"metric": {"__name__": "jvm_memory_used_bytes", "job": "user-api", "area": "nonheap", "pod": "user-api-abc12"}, "value": [1709000000, "89128960"]},
        {"metric": {"__name__": "jvm_memory_used_bytes", "job": "user-api", "area": "heap", "pod": "user-api-def34"}, "value": [1709000000, "285212672"]},
        {"metric": {"__name__": "jvm_memory_used_bytes", "job": "user-api", "area": "nonheap", "pod": "user-api-def34"}, "value": [1709000000, "92274688"]},
        {"metric": {"__name__": "jvm_memory_used_bytes", "job": "user-api", "area": "heap", "pod": "user-api-ghi56"}, "value": [1709000000, "301989888"]},
        {"metric": {"__name__": "jvm_memory_used_bytes", "job": "search-service", "area": "heap", "pod": "search-service-abc12"}, "value": [1709000000, "1073741824"]},
        {"metric": {"__name__": "jvm_memory_used_bytes", "job": "search-service", "area": "nonheap", "pod": "search-service-abc12"}, "value": [1709000000, "134217728"]},
        {"metric": {"__name__": "jvm_memory_used_bytes", "job": "search-service", "area": "heap", "pod": "search-service-def34"}, "value": [1709000000, "1006632960"]},
    ],
    "container_cpu_usage_seconds_total": [
        {"metric": {"__name__": "container_cpu_usage_seconds_total", "namespace": "production", "pod": "payment-service-abc12", "container": "payment-service"}, "value": [1709000000, "45231.23"]},
        {"metric": {"__name__": "container_cpu_usage_seconds_total", "namespace": "production", "pod": "payment-service-abc12", "container": "istio-proxy"}, "value": [1709000000, "12340.56"]},
        {"metric": {"__name__": "container_cpu_usage_seconds_total", "namespace": "production", "pod": "user-api-abc12", "container": "user-api"}, "value": [1709000000, "89012.34"]},
        {"metric": {"__name__": "container_cpu_usage_seconds_total", "namespace": "production", "pod": "user-api-abc12", "container": "istio-proxy"}, "value": [1709000000, "15678.90"]},
        {"metric": {"__name__": "container_cpu_usage_seconds_total", "namespace": "production", "pod": "order-service-abc12", "container": "order-service"}, "value": [1709000000, "34567.89"]},
        {"metric": {"__name__": "container_cpu_usage_seconds_total", "namespace": "production", "pod": "search-service-abc12", "container": "search-service"}, "value": [1709000000, "102345.67"]},
        {"metric": {"__name__": "container_cpu_usage_seconds_total", "namespace": "production", "pod": "nginx-ingress-abc12", "container": "nginx-ingress-controller"}, "value": [1709000000, "67890.12"]},
        {"metric": {"__name__": "container_cpu_usage_seconds_total", "namespace": "production", "pod": "celery-worker-abc12", "container": "celery-worker"}, "value": [1709000000, "78901.23"]},
    ],
    "container_memory_usage_bytes": [
        {"metric": {"__name__": "container_memory_usage_bytes", "namespace": "production", "pod": "payment-service-abc12", "container": "payment-service"}, "value": [1709000000, "142606336"]},
        {"metric": {"__name__": "container_memory_usage_bytes", "namespace": "production", "pod": "payment-service-abc12", "container": "istio-proxy"}, "value": [1709000000, "71303168"]},
        {"metric": {"__name__": "container_memory_usage_bytes", "namespace": "production", "pod": "user-api-abc12", "container": "user-api"}, "value": [1709000000, "573308928"]},
        {"metric": {"__name__": "container_memory_usage_bytes", "namespace": "production", "pod": "user-api-abc12", "container": "istio-proxy"}, "value": [1709000000, "75497472"]},
        {"metric": {"__name__": "container_memory_usage_bytes", "namespace": "production", "pod": "order-service-abc12", "container": "order-service"}, "value": [1709000000, "167772160"]},
        {"metric": {"__name__": "container_memory_usage_bytes", "namespace": "production", "pod": "search-service-abc12", "container": "search-service"}, "value": [1709000000, "1207959552"]},
        {"metric": {"__name__": "container_memory_usage_bytes", "namespace": "production", "pod": "elasticsearch-0", "container": "elasticsearch"}, "value": [1709000000, "4294967296"]},
        {"metric": {"__name__": "container_memory_usage_bytes", "namespace": "production", "pod": "rabbitmq-0", "container": "rabbitmq"}, "value": [1709000000, "536870912"]},
    ],
    "kube_pod_container_status_restarts_total": [
        {"metric": {"__name__": "kube_pod_container_status_restarts_total", "namespace": "production", "pod": "celery-worker-abc12", "container": "celery-worker"}, "value": [1709000000, "3"]},
        {"metric": {"__name__": "kube_pod_container_status_restarts_total", "namespace": "production", "pod": "payment-service-abc12", "container": "payment-service"}, "value": [1709000000, "0"]},
        {"metric": {"__name__": "kube_pod_container_status_restarts_total", "namespace": "production", "pod": "notification-service-abc12", "container": "notification-service"}, "value": [1709000000, "7"]},
        {"metric": {"__name__": "kube_pod_container_status_restarts_total", "namespace": "staging", "pod": "payment-service-staging-abc12", "container": "payment-service"}, "value": [1709000000, "12"]},
    ],
}

# ---------------------------------------------------------------------------
# Prometheus targets data
# ---------------------------------------------------------------------------

PROMETHEUS_TARGETS_DATA = [
    ("nginx-ingress", "production", 10254, "/metrics"),
    ("payment-service", "production", 9090, "/metrics"),
    ("user-api", "production", 8081, "/actuator/prometheus"),
    ("order-service", "production", 9090, "/metrics"),
    ("inventory-api", "production", 9090, "/metrics"),
    ("notification-service", "production", 9090, "/metrics"),
    ("search-service", "production", 8081, "/actuator/prometheus"),
    ("auth-proxy", "production", 9090, "/metrics"),
    ("celery-worker", "production", 9090, "/metrics"),
    ("rabbitmq", "production", 15692, "/metrics"),
    ("payment-service-staging", "staging", 9090, "/metrics"),
    ("user-api-staging", "staging", 8081, "/actuator/prometheus"),
    ("order-service-staging", "staging", 9090, "/metrics"),
    ("prometheus-server", "monitoring", 9090, "/metrics"),
    ("grafana", "monitoring", 3000, "/metrics"),
    ("alertmanager", "monitoring", 9093, "/metrics"),
    ("node-exporter", "monitoring", 9100, "/metrics"),
    ("kube-state-metrics", "monitoring", 8080, "/metrics"),
    ("coredns", "kube-system", 9153, "/metrics"),
    ("istiod", "istio-system", 15014, "/metrics"),
    ("cert-manager", "cert-manager", 9402, "/metrics"),
    ("fluentd", "logging", 24231, "/metrics"),
]

# ---------------------------------------------------------------------------
# Prometheus label values data
# ---------------------------------------------------------------------------

LABEL_VALUES_DATA = {
    "namespace": NAMESPACES,
    "job": [
        "nginx-ingress", "payment-service", "user-api", "order-service",
        "inventory-api", "notification-service", "search-service", "auth-proxy",
        "celery-worker", "rabbitmq", "postgres", "redis",
        "prometheus-server", "grafana", "alertmanager", "node-exporter", "kube-state-metrics",
        "coredns", "kube-proxy", "metrics-server", "aws-node",
        "istiod", "istio-ingressgateway",
        "cert-manager",
        "fluentd", "elasticsearch",
        "payment-service-staging", "user-api-staging", "order-service-staging",
    ],
    "instance": [
        "nginx-ingress.production.svc:10254",
        "payment-service.production.svc:9090",
        "user-api.production.svc:8081",
        "order-service.production.svc:9090",
        "inventory-api.production.svc:9090",
        "notification-service.production.svc:9090",
        "search-service.production.svc:8081",
        "auth-proxy.production.svc:9090",
        "celery-worker.production.svc:9090",
        "rabbitmq.production.svc:15692",
        "prometheus-server.monitoring.svc:9090",
        "grafana.monitoring.svc:3000",
        "alertmanager.monitoring.svc:9093",
        "10.0.1.10:9100",
        "10.0.1.11:9100",
        "10.0.1.12:9100",
        "kube-state-metrics.monitoring.svc:8080",
        "coredns.kube-system.svc:9153",
        "istiod.istio-system.svc:15014",
        "fluentd.logging.svc:24231",
    ],
    "status": ["200", "201", "204", "301", "302", "400", "401", "403", "404", "405", "429", "500", "502", "503", "504"],
}

# ---------------------------------------------------------------------------
# Alertmanager data (expanded)
# ---------------------------------------------------------------------------

EXISTING_ALERTS = [
    {
        "name": "HighErrorRate-nginx",
        "state": "firing",
        "severity": "warning",
        "expr": 'rate(nginx_ingress_controller_requests_total{status=~"5.."}[5m]) > 0.05',
        "annotations": {
            "summary": "High 5xx error rate on nginx-ingress",
            "description": "Error rate exceeds 5% over 5 minutes",
            "runbook_url": "https://wiki.internal/runbooks/nginx-5xx",
            "dashboard_url": "https://grafana.internal/d/nginx-overview",
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
            "runbook_url": "https://wiki.internal/runbooks/pod-restart-loop",
        },
    },
    {
        "name": "HighMemoryUsage",
        "state": "firing",
        "severity": "warning",
        "expr": 'container_memory_usage_bytes / container_spec_memory_limit_bytes > 0.85',
        "labels": {"namespace": "production", "pod": "search-service-abc12"},
        "annotations": {
            "summary": "Container memory usage above 85%",
            "description": "search-service in production is using 89% of its memory limit",
            "runbook_url": "https://wiki.internal/runbooks/high-memory",
        },
    },
    {
        "name": "CertExpiringSoon",
        "state": "firing",
        "severity": "warning",
        "expr": '(certmanager_certificate_expiration_timestamp_seconds - time()) / 86400 < 14',
        "labels": {"namespace": "production", "name": "api-tls"},
        "annotations": {
            "summary": "TLS certificate expiring within 14 days",
            "description": "Certificate api-tls in production expires in 11 days",
            "runbook_url": "https://wiki.internal/runbooks/cert-expiry",
        },
    },
    {
        "name": "PodCrashLoopBackOff",
        "state": "firing",
        "severity": "critical",
        "expr": 'kube_pod_container_status_waiting_reason{reason="CrashLoopBackOff"} > 0',
        "labels": {"namespace": "staging", "pod": "payment-service-staging-abc12"},
        "annotations": {
            "summary": "Pod in CrashLoopBackOff",
            "description": "payment-service-staging in staging has been crash-looping for 45 minutes",
            "runbook_url": "https://wiki.internal/runbooks/crashloop",
        },
    },
    {
        "name": "NodeDiskPressure",
        "state": "pending",
        "severity": "warning",
        "expr": 'node_filesystem_avail_bytes / node_filesystem_size_bytes < 0.1',
        "labels": {"node": "ip-10-0-1-12.ec2.internal"},
        "annotations": {
            "summary": "Node disk space below 10%",
            "description": "Node ip-10-0-1-12 has only 8% disk space remaining on /dev/xvda1",
            "runbook_url": "https://wiki.internal/runbooks/disk-pressure",
        },
    },
    {
        "name": "HighLatencyP99",
        "state": "firing",
        "severity": "warning",
        "expr": 'histogram_quantile(0.99, rate(http_request_duration_seconds_bucket[5m])) > 0.5',
        "labels": {"namespace": "production", "job": "search-service"},
        "annotations": {
            "summary": "P99 latency above 500ms",
            "description": "search-service P99 latency is 892ms, threshold 500ms",
            "runbook_url": "https://wiki.internal/runbooks/high-latency",
            "dashboard_url": "https://grafana.internal/d/service-latency",
        },
    },
    {
        "name": "RabbitMQQueueDepth",
        "state": "firing",
        "severity": "warning",
        "expr": 'rabbitmq_queue_messages > 10000',
        "labels": {"namespace": "production", "queue": "celery-default"},
        "annotations": {
            "summary": "RabbitMQ queue depth exceeds 10k",
            "description": "Queue celery-default has 14,523 messages pending",
            "runbook_url": "https://wiki.internal/runbooks/rabbitmq-queue-depth",
        },
    },
    {
        "name": "PostgresReplicationLag",
        "state": "pending",
        "severity": "critical",
        "expr": 'pg_replication_lag_seconds > 30',
        "labels": {"namespace": "production", "pod": "postgres-1"},
        "annotations": {
            "summary": "PostgreSQL replication lag above 30s",
            "description": "Replica postgres-1 has 42s replication lag",
            "runbook_url": "https://wiki.internal/runbooks/pg-replication-lag",
        },
    },
]

EXISTING_RULES = [
    {
        "name": "nginx-error-rate",
        "expr": 'rate(nginx_ingress_controller_requests_total{status=~"5.."}[5m]) > 0.05',
        "for": "5m",
        "severity": "warning",
        "annotations": {
            "summary": "High 5xx error rate on nginx-ingress",
            "runbook_url": "https://wiki.internal/runbooks/nginx-5xx",
        },
    },
    {
        "name": "pod-restart-loop",
        "expr": "increase(kube_pod_container_status_restarts_total[1h]) > 5",
        "for": "15m",
        "severity": "critical",
        "annotations": {
            "summary": "Pod restart loop detected",
            "description": "{{ $labels.pod }} in {{ $labels.namespace }} restarted {{ $value }} times",
            "runbook_url": "https://wiki.internal/runbooks/pod-restart-loop",
        },
    },
    {
        "name": "high-memory-usage",
        "expr": "container_memory_usage_bytes / container_spec_memory_limit_bytes > 0.85",
        "for": "10m",
        "severity": "warning",
        "annotations": {
            "summary": "Container memory usage above 85%",
            "description": "{{ $labels.container }} in {{ $labels.pod }} using {{ $value | humanizePercentage }} of limit",
        },
    },
    {
        "name": "high-latency-p99",
        "expr": "histogram_quantile(0.99, rate(http_request_duration_seconds_bucket[5m])) > 0.5",
        "for": "5m",
        "severity": "warning",
        "annotations": {
            "summary": "P99 latency above 500ms",
            "description": "{{ $labels.job }} P99 latency is {{ $value }}s",
        },
    },
    {
        "name": "cert-expiring-soon",
        "expr": "(certmanager_certificate_expiration_timestamp_seconds - time()) / 86400 < 14",
        "for": "1h",
        "severity": "warning",
        "annotations": {
            "summary": "TLS certificate expiring within 14 days",
            "description": "Certificate {{ $labels.name }} in {{ $labels.namespace }} expires in {{ $value }} days",
        },
    },
    {
        "name": "node-disk-pressure",
        "expr": "node_filesystem_avail_bytes / node_filesystem_size_bytes < 0.1",
        "for": "15m",
        "severity": "warning",
        "annotations": {
            "summary": "Node disk space below 10%",
            "description": "Node {{ $labels.node }} filesystem {{ $labels.mountpoint }} at {{ $value | humanizePercentage }}",
        },
    },
]
