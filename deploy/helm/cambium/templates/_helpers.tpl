{{- define "cambium.name" -}}cambium{{- end -}}
{{- define "cambium.fullname" -}}{{- if contains "cambium" .Release.Name -}}{{ .Release.Name | trunc 63 | trimSuffix "-" }}{{- else -}}{{ .Release.Name | trunc 40 | trimSuffix "-" }}-cambium{{- end -}}{{- end -}}
{{- define "cambium.labels" -}}
app.kubernetes.io/name: {{ include "cambium.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
helm.sh/chart: {{ .Chart.Name }}-{{ .Chart.Version }}
{{- end -}}
{{- define "cambium.selector" -}}
app.kubernetes.io/name: {{ include "cambium.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end -}}
{{- define "cambium.image" -}}
{{- if .Values.image.digest -}}{{ .Values.image.repository }}@{{ .Values.image.digest }}{{- else -}}{{ .Values.image.repository }}:{{ .Values.image.tag }}{{- end -}}
{{- end -}}
{{- define "cambium.podSecurity" -}}
runAsNonRoot: true
runAsUser: 65532
runAsGroup: 65532
fsGroup: 65532
seccompProfile: { type: RuntimeDefault }
{{- end -}}
{{- define "cambium.containerSecurity" -}}
allowPrivilegeEscalation: false
readOnlyRootFilesystem: true
privileged: false
capabilities: { drop: ["ALL"] }
{{- end -}}
{{- define "cambium.env" -}}
envFrom:
  - configMapRef: { name: {{ include "cambium.fullname" . }} }
  - secretRef: { name: {{ .Values.existingSecret }} }
{{- with .Values.extraEnv }}
env:
{{ toYaml . | indent 2 }}
{{- end }}
{{- end -}}
