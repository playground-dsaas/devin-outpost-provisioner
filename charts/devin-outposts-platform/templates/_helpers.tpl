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

{{/*
Environment, mounts and volumes shared by the provisioner Deployment and the
`helm test` Pod, so `verify` reads exactly the token, template and worker
images the running provisioner does.
*/}}
{{- define "platform.provisioner.env" -}}
{{- $p := .Values.provisioner -}}
- name: DEVIN_API_URL
  value: {{ $p.devinApiUrl | quote }}
{{- if $p.token.existingSecret }}
- name: OUTPOSTS_TOKEN_FILE
  value: /var/run/secrets/devin/{{ $p.token.secretKey }}
{{- else }}
- name: OUTPOSTS_TOKEN_SECRET_ID
  value: {{ $p.token.awsSecretId | quote }}
{{- with $p.token.awsRegion }}
- name: AWS_REGION
  value: {{ . | quote }}
{{- end }}
{{- end }}
- name: NAMESPACE_PREFIX
  value: {{ $p.namespacePrefix | quote }}
- name: OUTPOST_NAME_PREFIX
  value: {{ $p.outpostNamePrefix | quote }}
- name: EXCLUDE_ORG_IDS
  value: {{ join "," $p.excludeOrgIds | quote }}
- name: POLL_INTERVAL_SECONDS
  value: {{ $p.pollIntervalSeconds | quote }}
- name: DEPROVISION_GRACE_SECONDS
  value: {{ $p.deprovisionGraceSeconds | quote }}
- name: POOL_TEMPLATE_PATH
  value: /etc/org-provisioner/pool-template.yaml
- name: WORKER_IMAGES_PATH
  value: /etc/org-provisioner/worker-images.yaml
- name: SYSTEM_NAMESPACE
  valueFrom:
    fieldRef:
      fieldPath: metadata.namespace
- name: METRICS_ADDR
  value: 0.0.0.0:8080
- name: RUST_LOG
  value: {{ printf "%s,org_provisioner=%s" $p.logLevel $p.logLevel | quote }}
{{- with $p.extraEnv }}
{{ toYaml . }}
{{- end }}
{{- end -}}

{{- define "platform.provisioner.volumeMounts" -}}
- name: config
  mountPath: /etc/org-provisioner
  readOnly: true
{{- if .Values.provisioner.token.existingSecret }}
- name: outposts-token
  mountPath: /var/run/secrets/devin
  readOnly: true
{{- end }}
{{- end -}}

{{- define "platform.provisioner.volumes" -}}
{{- $p := .Values.provisioner -}}
- name: config
  projected:
    sources:
      - configMap:
          name: {{ include "platform.provisioner.name" . }}-pool-template
      - configMap:
          name: {{ include "platform.provisioner.name" . }}-worker-images
{{- if $p.token.existingSecret }}
- name: outposts-token
  secret:
    secretName: {{ $p.token.existingSecret }}
    items:
      - key: {{ $p.token.secretKey }}
        path: {{ $p.token.secretKey }}
{{- end }}
{{- end -}}
