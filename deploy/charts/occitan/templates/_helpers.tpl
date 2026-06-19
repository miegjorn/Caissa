{{/*
Common labels applied to all resources.
*/}}
{{- define "occitan.labels" -}}
app.kubernetes.io/managed-by: {{ .Release.Service }}
helm.sh/chart: {{ .Chart.Name }}-{{ .Chart.Version }}
app.kubernetes.io/part-of: occitan
{{- end }}

{{/*
Selector labels for a given component.
Usage: include "occitan.selectorLabels" (dict "component" "farga" "context" .)
*/}}
{{- define "occitan.selectorLabels" -}}
app.kubernetes.io/name: {{ .component }}
app.kubernetes.io/instance: {{ .context.Release.Name }}
{{- end }}

{{/*
PostgreSQL service hostname (bitnami subchart convention).
*/}}
{{- define "occitan.postgresHost" -}}
{{- printf "%s-postgresql" .Release.Name }}
{{- end }}
