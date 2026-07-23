//! Curated MCP servers `/mcp add` can install without hand-authoring TOML.
//!
//! Each entry carries everything the guided flow needs: the config shape to
//! write, the credentials to collect (as `{{ env.VAR }}` placeholders so
//! secrets land in `.env`, never in the TOML), the prerequisites to show
//! before asking for anything, and a starter prompt that exercises the
//! integration once connected.

/// A blessed MCP server the wizard can install.
pub(crate) struct CatalogEntry {
    /// Default `[mcp.servers.<key>]` name; also the display name.
    pub key: &'static str,
    /// One-line description written into the config.
    pub description: &'static str,
    /// What the user must have ready, and the access AURA will get.
    pub prerequisites: &'static str,
    pub template: Template,
    /// First prompt to try once the server is connected.
    pub starter_prompt: &'static str,
}

pub(crate) enum Template {
    /// `transport = "http_streamable"` with static auth headers.
    Http {
        url: &'static str,
        headers: &'static [HeaderTemplate],
    },
    /// `transport = "stdio"` (no credential collection).
    Stdio {
        cmd: &'static [&'static str],
        args: &'static [&'static str],
    },
}

/// One static header whose value holds a `{{ env.VAR }}` placeholder.
pub(crate) struct HeaderTemplate {
    pub header: &'static str,
    /// Literal header value written to the config, e.g.
    /// `"Bearer {{ env.MEZMO_API_KEY }}"`.
    pub value_template: &'static str,
    /// The env var the placeholder references; its value is collected
    /// with masked input and written to `.env`.
    pub env_var: &'static str,
    /// Masked-input prompt shown when collecting the secret.
    pub secret_prompt: &'static str,
}

pub(crate) const CATALOG: &[CatalogEntry] = &[
    CatalogEntry {
        key: "mezmo",
        description: "Mezmo log analysis, export, and pipeline monitoring",
        prerequisites: "Requires a Mezmo service API key (Mezmo UI: Settings > API Keys).\n\
                        AURA will be able to query logs, exports, and pipeline state on your account.",
        template: Template::Http {
            url: "https://mcp.mezmo.com/mcp",
            headers: &[HeaderTemplate {
                header: "Authorization",
                value_template: "Bearer {{ env.MEZMO_API_KEY }}",
                env_var: "MEZMO_API_KEY",
                secret_prompt: "Mezmo service API key",
            }],
        },
        starter_prompt: "What are the most common errors in my logs from the last hour?",
    },
    CatalogEntry {
        key: "pagerduty",
        description: "PagerDuty incident management, on-call schedules, and escalation",
        prerequisites: "Requires a PagerDuty API token (User Settings > API Access).\n\
                        AURA will be able to read incidents, on-call schedules, and escalation policies.",
        template: Template::Http {
            url: "https://mcp.pagerduty.com/mcp",
            headers: &[HeaderTemplate {
                header: "Authorization",
                value_template: "Token {{ env.PAGERDUTY_API_TOKEN }}",
                env_var: "PAGERDUTY_API_TOKEN",
                secret_prompt: "PagerDuty API token",
            }],
        },
        starter_prompt: "Who is on call right now, and are there any open incidents?",
    },
    CatalogEntry {
        key: "datadog",
        description: "Datadog metrics, monitors, dashboards, and APM traces",
        prerequisites: "Requires a Datadog API key and application key (Organization Settings).\n\
                        AURA will be able to read metrics, monitors, dashboards, and traces.",
        template: Template::Http {
            url: "https://mcp.datadoghq.com/api/unstable/mcp-server/mcp",
            headers: &[
                HeaderTemplate {
                    header: "DD-API-KEY",
                    value_template: "{{ env.DD_API_KEY }}",
                    env_var: "DD_API_KEY",
                    secret_prompt: "Datadog API key",
                },
                HeaderTemplate {
                    header: "DD-APPLICATION-KEY",
                    value_template: "{{ env.DD_APPLICATION_KEY }}",
                    env_var: "DD_APPLICATION_KEY",
                    secret_prompt: "Datadog application key",
                },
            ],
        },
        starter_prompt: "Which Datadog monitors are currently alerting?",
    },
    CatalogEntry {
        key: "kubernetes",
        description: "Kubernetes cluster inspection via kubernetes-mcp-server",
        prerequisites: "Requires Node.js (npx) and a working kubeconfig context.\n\
                        AURA will access your cluster with your kubeconfig's permissions.",
        template: Template::Stdio {
            cmd: &["npx"],
            args: &["-y", "kubernetes-mcp-server@latest"],
        },
        starter_prompt: "What pods are running in my cluster, and are any of them unhealthy?",
    },
];
