{{/*
Chart name, truncated to the Kubernetes label limit.
*/}}
{{- define "vaultplane.name" -}}
{{- default .Chart.Name .Values.nameOverride | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{/*
Fully qualified resource name. Honors fullnameOverride, otherwise composes
`<release>-<chart>` (or just `<release>` when release name already contains
the chart name).
*/}}
{{- define "vaultplane.fullname" -}}
{{- if .Values.fullnameOverride -}}
{{- .Values.fullnameOverride | trunc 63 | trimSuffix "-" -}}
{{- else -}}
{{- $name := default .Chart.Name .Values.nameOverride -}}
{{- if contains $name .Release.Name -}}
{{- .Release.Name | trunc 63 | trimSuffix "-" -}}
{{- else -}}
{{- printf "%s-%s" .Release.Name $name | trunc 63 | trimSuffix "-" -}}
{{- end -}}
{{- end -}}
{{- end -}}

{{/*
Common labels applied to every resource managed by this chart.
*/}}
{{- define "vaultplane.labels" -}}
helm.sh/chart: {{ printf "%s-%s" .Chart.Name .Chart.Version | replace "+" "_" | trunc 63 | trimSuffix "-" }}
{{ include "vaultplane.selectorLabels" . }}
{{- if .Chart.AppVersion }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
{{- end }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
{{- end -}}

{{/*
Selector labels (the subset of labels stable across upgrades).
*/}}
{{- define "vaultplane.selectorLabels" -}}
app.kubernetes.io/name: {{ include "vaultplane.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end -}}

{{/*
ServiceAccount name (managed by this chart, or referenced externally).
*/}}
{{- define "vaultplane.serviceAccountName" -}}
{{- if .Values.serviceAccount.create -}}
{{- default (include "vaultplane.fullname" .) .Values.serviceAccount.name -}}
{{- else -}}
{{- default "default" .Values.serviceAccount.name -}}
{{- end -}}
{{- end -}}

{{/*
Secret name (managed by this chart, or referenced externally). Errors at
render time if secret.create=false and no name is given.
*/}}
{{- define "vaultplane.secretName" -}}
{{- if .Values.secret.create -}}
{{- default (printf "%s-secrets" (include "vaultplane.fullname" .)) .Values.secret.name -}}
{{- else -}}
{{- required "secret.name is required when secret.create=false" .Values.secret.name -}}
{{- end -}}
{{- end -}}

{{/*
Image reference: repo + (tag or chart appVersion).
*/}}
{{- define "vaultplane.image" -}}
{{- $tag := default .Chart.AppVersion .Values.image.tag -}}
{{- printf "%s:%s" .Values.image.repository $tag -}}
{{- end -}}
