{{- define "lulan.fullname" -}}
{{- .Release.Name }}-lulan
{{- end }}
{{- define "lulan.labels" -}}
app.kubernetes.io/name: lulan
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end }}
{{- define "lulan.secretName" -}}
{{- if .Values.existingSecret }}{{ .Values.existingSecret }}{{ else }}{{ include "lulan.fullname" . }}{{ end }}
{{- end }}
