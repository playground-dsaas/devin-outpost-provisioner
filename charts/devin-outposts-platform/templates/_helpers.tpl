{{- define "platform.provisioner.name" -}}
{{- default "org-provisioner" .Values.provisioner.nameOverride | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{- define "platform.provisioner.labels" -}}
helm.sh/chart: {{ printf "%s-%s" .Chart.Name .Chart.Version | replace "+" "_" | trunc 63 | trimSuffix "-" }}
{{ include "platform.provisioner.selectorLabels" . }}
app.kubernetes.io/version: {{ .Values.provisioner.image.tag | quote }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
{{- end -}}

{{- define "platform.provisioner.selectorLabels" -}}
app.kubernetes.io/name: {{ include "platform.provisioner.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end -}}

{{- define "platform.provisioner.serviceAccountName" -}}
{{- default (include "platform.provisioner.name" .) .Values.provisioner.serviceAccount.name -}}
{{- end -}}

{{/* Fails the render early when the token source is misconfigured. */}}
{{- define "platform.provisioner.tokenCheck" -}}
{{- $t := .Values.provisioner.token -}}
{{- if and (not $t.existingSecret) (not $t.awsSecretId) -}}
{{- fail "set provisioner.token.existingSecret (Kubernetes Secret) or provisioner.token.awsSecretId (AWS Secrets Manager)" -}}
{{- end -}}
{{- if and $t.existingSecret $t.awsSecretId -}}
{{- fail "set only one of provisioner.token.existingSecret and provisioner.token.awsSecretId" -}}
{{- end -}}
{{- end -}}
