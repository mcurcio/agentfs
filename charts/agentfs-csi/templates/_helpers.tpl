{{/*
The driver is one per cluster: its name (agentfs.mcurcio.github.io) and
cluster-wide objects are fixed, so object names do not carry the release
name.
*/}}

{{- define "agentfs-csi.driverName" -}}
agentfs.mcurcio.github.io
{{- end }}

{{/* The labels of every object; pod templates carry only the selector. */}}
{{- define "agentfs-csi.labels" -}}
helm.sh/chart: {{ printf "%s-%s" .Chart.Name .Chart.Version | replace "+" "_" | trunc 63 | trimSuffix "-" }}
app.kubernetes.io/part-of: agentfs
app.kubernetes.io/instance: {{ .Release.Name }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
{{- with .Chart.AppVersion }}
app.kubernetes.io/version: {{ . | trunc 63 | quote }}
{{- end }}
{{- end }}

{{/* An image reference: (dict "image" <values> "tag" <default tag>). */}}
{{- define "agentfs-csi.image" -}}
{{- $tag := .image.tag | default .tag -}}
{{- .image.repository }}{{ with $tag }}:{{ . }}{{ end }}{{ with .image.digest }}@{{ . }}{{ end }}
{{- end }}

{{- define "agentfs-csi.agentfsImage" -}}
image: {{ include "agentfs-csi.image" (dict "image" .Values.image "tag" .Chart.AppVersion) }}
{{- with .Values.image.pullPolicy }}
imagePullPolicy: {{ . }}
{{- end }}
{{- end }}

{{- define "agentfs-csi.sidecarImage" -}}
image: {{ include "agentfs-csi.image" (dict "image" .image "tag" "") }}
{{- end }}

{{- define "agentfs-csi.credentialsSecret" -}}
{{- .Values.credentials.existingSecret | default "agentfs-csi-credentials" }}
{{- end }}

{{/*
The git server's Secret (server.yaml): `htpasswd` for the server, `token`
for the profiles, mounted into the node plugin as credentials key
`agentfs-server` (node.yaml). One Secret, one source of the password.
*/}}
{{- define "agentfs-csi.serverSecret" -}}
{{- .Values.server.credentials.existingSecret | default "agentfs-git-server" }}
{{- end }}

{{- define "agentfs-csi.serverImage" -}}
image: {{ include "agentfs-csi.image" (dict "image" .Values.server.image "tag" .Chart.AppVersion) }}
{{- with .Values.server.image.pullPolicy }}
imagePullPolicy: {{ . }}
{{- end }}
{{- end }}

{{/* The git server's containers: non-root, a read-only root filesystem. */}}
{{- define "agentfs-csi.serverSecurityContext" -}}
securityContext:
  allowPrivilegeEscalation: false
  capabilities: { drop: [ALL] }
  readOnlyRootFilesystem: true
{{- end }}

{{/* The token of csi-provisioner (never mounted into agentfs-csi). */}}
{{- define "agentfs-csi.apiTokenVolume" -}}
- name: api-token
  projected:
    sources:
      - serviceAccountToken: { path: token, expirationSeconds: 3600 }
      - configMap:
          name: kube-root-ca.crt
          items: [{ key: ca.crt, path: ca.crt }]
      - downwardAPI:
          items:
            - path: namespace
              fieldRef: { fieldPath: metadata.namespace }
{{- end }}

{{/* The CSI sidecars' container security context. */}}
{{- define "agentfs-csi.sidecarSecurityContext" -}}
securityContext:
  # Root (with no capabilities) to reach agentfs-csi's socket, which is
  # root's and mode 0660.
  runAsUser: 0
  allowPrivilegeEscalation: false
  capabilities: { drop: [ALL] }
  readOnlyRootFilesystem: true
  seccompProfile: { type: RuntimeDefault }
{{- end }}
