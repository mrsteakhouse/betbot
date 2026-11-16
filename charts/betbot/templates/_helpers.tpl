{{- define "betbot.name" -}}
{{- default .Chart.Name .Values.nameOverride | trunc 63 | trimSuffix "-" }}
{{- end }}

{{/* CronJob names are limited to 52 characters. */}}
{{- define "betbot.fullname" -}}
{{- if .Values.fullnameOverride }}
{{- .Values.fullnameOverride | trunc 52 | trimSuffix "-" }}
{{- else }}
{{- $name := default .Chart.Name .Values.nameOverride }}
{{- if contains $name .Release.Name }}
{{- .Release.Name | trunc 52 | trimSuffix "-" }}
{{- else }}
{{- printf "%s-%s" .Release.Name $name | trunc 52 | trimSuffix "-" }}
{{- end }}
{{- end }}
{{- end }}

{{- define "betbot.chart" -}}
{{- printf "%s-%s" .Chart.Name .Chart.Version | replace "+" "_" | trunc 63 | trimSuffix "-" }}
{{- end }}

{{- define "betbot.selectorLabels" -}}
app.kubernetes.io/name: {{ include "betbot.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end }}

{{- define "betbot.labels" -}}
helm.sh/chart: {{ include "betbot.chart" . }}
{{ include "betbot.selectorLabels" . }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
{{- end }}

{{- define "betbot.image" -}}
{{- printf "%s:%s" .Values.image.repository (default .Chart.AppVersion .Values.image.tag) }}
{{- end }}

{{/*
App config (config.yaml). `existingSecret` is chart-only and removed here (the app
rejects unknown fields); credentialsDir (tenants) and urlFile (notifiers) are
derived from it. `omit` returns a copy, so .Values is never mutated.
`notifiers` is only rendered when non-empty.
*/}}
{{- define "betbot.config" -}}
{{- $tenants := list -}}
{{- range .Values.config.tenants -}}
{{- $t := omit . "existingSecret" -}}
{{- if .existingSecret -}}
{{- if .credentialsDir -}}
{{- fail (printf "tenant %q: set either existingSecret or credentialsDir, not both" .name) -}}
{{- end -}}
{{- if not (mustRegexMatch "^[A-Za-z0-9][A-Za-z0-9._-]*$" .name) -}}
{{- fail (printf "tenant name %q is not usable as a mount path component (allowed: [A-Za-z0-9._-], must start alphanumeric)" .name) -}}
{{- end -}}
{{- $_ := set $t "credentialsDir" (printf "/secrets/%s" .name) -}}
{{- end -}}
{{- $tenants = append $tenants $t -}}
{{- end -}}
{{- $notifiers := list -}}
{{- range .Values.config.notifiers -}}
{{- $n := omit . "existingSecret" -}}
{{- if not (mustRegexMatch "^[A-Za-z0-9][A-Za-z0-9._-]*$" .name) -}}
{{- fail (printf "notifier name %q is not usable as a mount path component (allowed: [A-Za-z0-9._-], must start alphanumeric)" .name) -}}
{{- end -}}
{{- if and .existingSecret .urlFile -}}
{{- fail (printf "notifier %q: set either existingSecret or urlFile, not both" .name) -}}
{{- end -}}
{{- if and (not .existingSecret) (not .urlFile) -}}
{{- fail (printf "notifier %q: existingSecret (or urlFile) is required" .name) -}}
{{- end -}}
{{- if .existingSecret -}}
{{- $_ := set $n "urlFile" (printf "/notifiers/%s/url" .name) -}}
{{- end -}}
{{- $notifiers = append $notifiers $n -}}
{{- end -}}
{{- $config := dict "tenantTimeoutSeconds" .Values.config.tenantTimeoutSeconds "tenants" $tenants -}}
{{- if $notifiers -}}
{{- $_ := set $config "notifiers" $notifiers -}}
{{- end -}}
{{- toYaml $config -}}
{{- end }}
