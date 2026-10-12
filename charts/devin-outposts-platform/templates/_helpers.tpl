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

{{- define "platform.operator.serviceAccountName" -}}
{{- required "operator.serviceAccount.name is required (the provisioner binds it in each org namespace)" .Values.operator.serviceAccount.name -}}
{{- end -}}

{{/* ClusterRole the provisioner binds to the operator in each org namespace. */}}
{{- define "platform.operator.orgRole" -}}
{{- printf "%s-%s-org" .Release.Namespace (include "platform.operator.serviceAccountName" .) -}}
{{- end -}}
