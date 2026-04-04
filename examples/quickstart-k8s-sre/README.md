# Kubernetes SRE Orchestration Quickstart

Deploy an AI-powered Kubernetes SRE agent that uses **orchestration mode** to coordinate
specialized workers -- one for cluster inspection, one for metrics analysis -- each with
access to only the tools they need.

## What You'll Build

```
                         User Query
                             |
                             v
          +------------------------------------------+
          |        Aura (Coordinator)                 |
          |  Routes requests to the right specialist  |
          |                                           |
          |  +-------------------+ +----------------+ |
          |  | cluster_inspector | | metrics_analyst| |
          |  | K8s tools only    | | Prom tools only| |
          |  +--------+----------+ +-------+--------+ |
          +-----------|--------------------|----------+
                      |                    |
                      v                    v
              +---------------+   +-----------------+
              | K8s MCP       |   | Prometheus MCP  |
              | Server        |   | Server          |
              +-------+-------+   +--------+--------+
                      |                    |
                      v                    v
              +---------------+   +-----------------+
              | Kubernetes    |   | Prometheus      |
              | API           |   | (OTel Demo)     |
              +---------------+   +-----------------+
```

The **coordinator** receives user queries and dispatches them to:

- **cluster_inspector** -- filtered to Kubernetes MCP tools (pods, deployments, logs, events)
- **metrics_analyst** -- filtered to Prometheus MCP tools (PromQL queries, alerts, targets)

## Prerequisites

- [Docker](https://docs.docker.com/get-docker/)
- [kubectl](https://kubernetes.io/docs/tasks/tools/)
- [kind](https://kind.sigs.k8s.io/docs/user/quick-start/#installation)
- [Helm](https://helm.sh/docs/intro/install/) 3.12+
- An OpenAI API key (or another [supported LLM provider](../../examples/reference.toml))

## Setup

All commands assume you're in the repo root.

### 1. Create a KIND cluster

```bash
kind create cluster --name aura-sre
```

### 2. Deploy the OpenTelemetry Demo

The [OpenTelemetry Demo](https://opentelemetry.io/docs/demo/) deploys a microservices
application with Prometheus, Grafana, and Jaeger -- giving your SRE agent real
workloads and metrics to inspect.

```bash
helm repo add open-telemetry https://open-telemetry.github.io/opentelemetry-helm-charts
helm install otel-demo open-telemetry/opentelemetry-demo
```

Wait for pods to come up (this takes a few minutes on first pull):

```bash
kubectl get pods -w
```

> **Tip:** Not every pod needs to be `Running` before proceeding. As long as the
> Prometheus pod is ready, you can continue.

Verify Prometheus is running and note the service name:

```bash
kubectl get svc | grep prometheus
```

You should see a service like `prometheus` on port `9090`. Note the name --
if it differs, use it in the `--set` flag in step 3 below.

### 3. Deploy the MCP servers

Both MCP servers have community Helm charts. Install them with:

```bash
# Kubernetes MCP Server — read-only cluster access
# Binds the built-in "view" ClusterRole for read access to cluster resources.
helm install kubernetes-mcp-server \
  oci://ghcr.io/containers/charts/kubernetes-mcp-server \
  --set ingress.enabled=false \
  --set config.read_only=true \
  --set 'rbac.extraClusterRoleBindings[0].name=view' \
  --set 'rbac.extraClusterRoleBindings[0].roleRef.name=view' \
  --set 'rbac.extraClusterRoleBindings[0].roleRef.external=true'

# Prometheus MCP Server — connected to the OTel Demo's Prometheus
# Override probes to use TCP (the MCP server has no GET health endpoint).
helm install prometheus-mcp-server \
  oci://ghcr.io/pab1it0/charts/prometheus-mcp-server \
  --set prometheus.url="http://prometheus:9090" \
  --set livenessProbe.httpGet=null \
  --set 'livenessProbe.tcpSocket.port=http' \
  --set readinessProbe.httpGet=null \
  --set 'readinessProbe.tcpSocket.port=http'
```

> **Different Prometheus service name?** Run `kubectl get svc | grep prometheus`
> and replace the URL above with the correct service name from step 2.

Verify both MCP servers are healthy before proceeding:

```bash
# Check the Kubernetes MCP server can reach the API
kubectl logs -l app.kubernetes.io/name=kubernetes-mcp-server --tail=5

# Check the Prometheus MCP server connected to Prometheus
kubectl logs -l app.kubernetes.io/name=prometheus-mcp-server --tail=5
```

Wait for them to start:

```bash
kubectl wait --for=condition=ready pod -l app.kubernetes.io/name=kubernetes-mcp-server --timeout=120s
kubectl wait --for=condition=ready pod -l app.kubernetes.io/name=prometheus-mcp-server --timeout=120s
```

### 4. Deploy Aura

```bash
export OPENAI_API_KEY="sk-..."

helm install aura ./deployment/helm/aura \
  -f examples/quickstart-k8s-sre/aura-values.yaml \
  --set secrets.openaiApiKey="$OPENAI_API_KEY"
```

> **Using a different LLM provider?** Edit `aura-values.yaml` and update the `[llm]`
> section. See [`examples/reference.toml`](../reference.toml) for all provider options.

Wait for Aura:

```bash
kubectl wait --for=condition=ready pod -l app.kubernetes.io/name=aura --timeout=120s
```

### 5. Try it out

Launch [Aura CLI](https://hub.docker.com/r/mezmo/aura-cli) inside the cluster,
pointed at the Aura service:

```bash
kubectl run -it --rm aura-cli \
  --image=mezmo/aura-cli \
  --restart=Never \
  --env="AURA_AGENT_API_URL=http://aura:80" \
  --env="AURA_AGENT_MODEL=kubernetes-sre"
```

This drops you into an interactive REPL. Try these queries:

```
Check the health of my cluster. Are all pods running? Are there any high CPU or memory usage concerns?
```

```
What services are running in the default namespace and what are their error rates?
```

```
Are there any pods in CrashLoopBackOff? If so, show me their logs and related metrics.
```

```
Show me the top 5 pods by memory usage and check if any are close to their limits.
```

The coordinator dispatches to both workers: **cluster_inspector** checks pod
status and events, while **metrics_analyst** queries Prometheus for resource usage.
You can toggle the SSE event panel with `/stream` to watch the orchestration in
real time.

Type `/quit` to exit.

## How the orchestration config works

Open `aura-values.yaml` and look at the `config.content` section. The key pieces:

**`[orchestration]`** -- enables orchestration mode. The coordinator agent receives
every query and decides whether to answer directly, ask for clarification, or
dispatch to workers.

**`[orchestration.worker.cluster_inspector]`** -- a worker with `mcp_filter` set to
only Kubernetes tool names. Even though both MCP servers are connected, this worker
can only see and use K8s tools.

**`[orchestration.worker.metrics_analyst]`** -- a worker with `mcp_filter` set to
only Prometheus tool names. It can only query metrics, not touch the cluster.

This separation means each worker operates with least-privilege access to tools,
and the coordinator handles synthesis across domains.

## Customizing tool filters

The `mcp_filter` arrays in `aura-values.yaml` list the exact tool names exposed
by each MCP server. The names in this quickstart were sourced from the upstream
repos:

- [kubernetes-mcp-server tools](https://github.com/containers/kubernetes-mcp-server) -- core toolset, `read_only = true`
- [prometheus-mcp-server tools](https://github.com/pab1it0/prometheus-mcp-server) -- all tools

To verify the tools Aura actually discovered at runtime:

```bash
kubectl logs -l app.kubernetes.io/name=aura | grep -i "tool"
```

> **Important:** If a tool name in `mcp_filter` doesn't match any real tool, it's
> silently ignored. If *none* match, the worker has zero tools and will fail.

## Cleanup

```bash
helm uninstall aura
helm uninstall prometheus-mcp-server
helm uninstall kubernetes-mcp-server
helm uninstall otel-demo
kind delete cluster --name aura-sre
```
