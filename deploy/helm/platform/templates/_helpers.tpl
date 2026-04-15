{{/*
Expand the name of the chart.
*/}}
{{- define "platform.name" -}}
{{- default .Chart.Name .Values.nameOverride | trunc 63 | trimSuffix "-" }}
{{- end }}

{{/*
Create a default fully qualified app name.
We truncate at 63 chars because some Kubernetes name fields are limited to this (by the DNS naming spec).
If release name contains chart name it will be used as a full name.
*/}}
{{- define "platform.fullname" -}}
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
{{- define "platform.chart" -}}
{{- printf "%s-%s" .Chart.Name .Chart.Version | replace "+" "_" | trunc 63 | trimSuffix "-" }}
{{- end }}

{{/*
Common labels.
*/}}
{{- define "platform.labels" -}}
helm.sh/chart: {{ include "platform.chart" . }}
{{ include "platform.selectorLabels" . }}
{{- if .Chart.AppVersion }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
{{- end }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
{{- end }}

{{/*
Selector labels.
*/}}
{{- define "platform.selectorLabels" -}}
app.kubernetes.io/name: {{ include "platform.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end }}

{{/*
Create the name of the service account to use.
*/}}
{{- define "platform.serviceAccountName" -}}
{{- if .Values.serviceAccount.create }}
{{- default (include "platform.fullname" .) .Values.serviceAccount.name }}
{{- else }}
{{- default "default" .Values.serviceAccount.name }}
{{- end }}
{{- end }}

{{/*
Name of the Secret resource.
*/}}
{{- define "platform.secretName" -}}
{{- include "platform.fullname" . }}
{{- end }}

{{/*
Name of the ConfigMap resource.
*/}}
{{- define "platform.configmapName" -}}
{{- include "platform.fullname" . }}
{{- end }}

{{/*
PostgreSQL host — subchart service name or external.
*/}}
{{- define "platform.postgresql.host" -}}
{{- if .Values.postgresql.enabled }}
{{- printf "%s-postgresql" .Release.Name }}
{{- else }}
{{- /* Extracted from externalDatabase.url — users set the full URL directly */ -}}
{{- "" }}
{{- end }}
{{- end }}

{{/*
DATABASE_URL — subchart auto-wired or external override.
*/}}
{{- define "platform.databaseUrl" -}}
{{- if .Values.postgresql.enabled }}
{{- $host := printf "%s-postgresql" .Release.Name }}
{{- $user := .Values.postgresql.auth.username }}
{{- $db := .Values.postgresql.auth.database }}
{{- printf "postgres://%s:$(POSTGRES_PASSWORD)@%s:5432/%s" $user $host $db }}
{{- else }}
{{- .Values.externalDatabase.url }}
{{- end }}
{{- end }}

{{/*
VALKEY_URL — subchart auto-wired or external override.
*/}}
{{- define "platform.valkeyUrl" -}}
{{- if .Values.valkey.enabled }}
{{- printf "redis://%s-valkey-master:6379" .Release.Name }}
{{- else }}
{{- .Values.externalValkey.url }}
{{- end }}
{{- end }}

{{/*
MINIO_ENDPOINT — subchart auto-wired or external override.
*/}}
{{- define "platform.minioEndpoint" -}}
{{- if .Values.minio.enabled }}
{{- printf "http://%s-minio:9000" .Release.Name }}
{{- else }}
{{- .Values.externalMinio.endpoint }}
{{- end }}
{{- end }}

{{/*
Determine CORS origins from ingress hosts when not explicitly set.
*/}}
{{- define "platform.corsOrigins" -}}
{{- if .Values.ingress.enabled }}
{{- $origins := list }}
{{- range .Values.ingress.hosts }}
{{- if .Values.ingress.tls }}
{{- $origins = append $origins (printf "https://%s" .host) }}
{{- else }}
{{- $origins = append $origins (printf "http://%s" .host) }}
{{- end }}
{{- end }}
{{- join "," $origins }}
{{- else }}
{{- "" }}
{{- end }}
{{- end }}

{{/*
WebAuthn RP ID — from values or first ingress host.
*/}}
{{- define "platform.webauthnRpId" -}}
{{- if .Values.platform.webauthn.rpId }}
{{- .Values.platform.webauthn.rpId }}
{{- else if .Values.ingress.enabled }}
{{- (index .Values.ingress.hosts 0).host }}
{{- else }}
{{- "localhost" }}
{{- end }}
{{- end }}

{{/*
WebAuthn RP Origin — from values or first ingress host.
*/}}
{{- define "platform.webauthnRpOrigin" -}}
{{- if .Values.platform.webauthn.rpOrigin }}
{{- .Values.platform.webauthn.rpOrigin }}
{{- else if and .Values.ingress.enabled .Values.ingress.tls }}
{{- printf "https://%s" (index .Values.ingress.hosts 0).host }}
{{- else if .Values.ingress.enabled }}
{{- printf "http://%s" (index .Values.ingress.hosts 0).host }}
{{- else }}
{{- "http://localhost:8080" }}
{{- end }}
{{- end }}
