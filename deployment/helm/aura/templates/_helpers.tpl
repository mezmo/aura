{{/*
Expand the name of the chart.
*/}}
{{- define "aura.name" -}}
{{- default .Chart.Name .Values.nameOverride | trunc 63 | trimSuffix "-" }}
{{- end }}

{{/*
Create a default fully qualified app name.
We truncate at 63 chars because some Kubernetes name fields are limited to this (by the DNS naming spec).
If release name contains chart name it will be used as a full name.
*/}}
{{- define "aura.fullname" -}}
{{- if .Values.fullnameOverride }}
{{- .Values.fullnameOverride | trunc 63 | trimSuffix "-" }}
{{- else }}
{{- $name := default .Chart.Name .Values.nameOverride }}
{{- if contains $name .Release.Name }}
{{- .Release.Name | trunc 63 | trimSuffix "-" }}
{{- else }}
{{- printf "%s-%s" .Release.Name $name | trunc 63 | trimSuffix "-" }}
{{- end }}
{{- end }}
{{- end }}

{{/*
Create chart name and version as used by the chart label.
*/}}
{{- define "aura.chart" -}}
{{- printf "%s-%s" .Chart.Name .Chart.Version | replace "+" "_" | trunc 63 | trimSuffix "-" }}
{{- end }}

{{/*
Common labels
*/}}
{{- define "aura.labels" -}}
helm.sh/chart: {{ include "aura.chart" . }}
{{ include "aura.selectorLabels" . }}
{{- if .Chart.AppVersion }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
{{- end }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
{{- end }}

{{/*
Selector labels
*/}}
{{- define "aura.selectorLabels" -}}
app.kubernetes.io/name: {{ include "aura.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end }}

{{/*
Create the name of the service account to use
*/}}
{{- define "aura.serviceAccountName" -}}
{{- if .Values.serviceAccount.create }}
{{- default (include "aura.fullname" .) .Values.serviceAccount.name }}
{{- else }}
{{- default "default" .Values.serviceAccount.name }}
{{- end }}
{{- end }}

{{/*
Create the name of the config configmap
*/}}
{{- define "aura.configMapName" -}}
{{- if .Values.config.existingConfigMap }}
{{- .Values.config.existingConfigMap }}
{{- else }}
{{- printf "%s-config" (include "aura.fullname" .) }}
{{- end }}
{{- end }}

{{/*
Create the name of the secrets
*/}}
{{- define "aura.secretName" -}}
{{- if .Values.secrets.existingSecret }}
{{- .Values.secrets.existingSecret }}
{{- else }}
{{- printf "%s-secrets" (include "aura.fullname" .) }}
{{- end }}
{{- end }}

{{/*
Determine if we should create a secret
*/}}
{{- define "aura.createSecret" -}}
{{- if and (not .Values.secrets.existingSecret) (or .Values.secrets.openaiApiKey .Values.secrets.anthropicApiKey .Values.secrets.mezmoApiKey .Values.secrets.awsAccessKeyId .Values.secrets.awsSecretAccessKey) }}
{{- true }}
{{- end }}
{{- end }}

{{/*
=======================================================================
TOML Rendering Helpers
=======================================================================
Renders structured YAML values into valid TOML using Helm's built-in
toToml function (requires Helm 3.12+).
Used when config.content is empty and structured sections are provided.
*/}}

{{/*
aura.toml.section — Render a named TOML section via toToml.
Note: Helm's YAML parser turns ints into Go float64, so toToml may render
"8000.0" instead of "8000". Aura's config deserializer accepts both forms
(see lenient_int module), so no template-side fixup is needed.
*/}}
{{- define "aura.toml.section" -}}
{{- toToml (dict .name .val) -}}
{{- end }}

{{/*
aura.toml.renderConfig — Scaffold the config.toml from structured sections.
Each section is rendered independently via toToml. Only non-empty sections
are included. The layout here mirrors the TOML file structure.
*/}}
{{- define "aura.toml.renderConfig" -}}
{{- $cfg := .Values.config -}}
{{- range $section := list "llm" "agent" "vector_stores" "mcp" "tools" "orchestration" "orchestrator" "workers" -}}
{{- $val := index $cfg $section | default dict -}}
{{- if not (empty $val) -}}
{{- include "aura.toml.section" (dict "name" $section "val" $val) }}
{{ end -}}
{{- end -}}
{{- with .Values.config.extra_toml }}
{{ . }}
{{- end -}}
{{- end }}
