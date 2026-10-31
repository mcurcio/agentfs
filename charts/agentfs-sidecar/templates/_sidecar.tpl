{{/*
agentfs as a phase 1 native sidecar (design §17.1, docs/deployment.md),
as helpers for a workload's own chart (docs/helm.md §4). Each takes the
values map of this library (its alias's key):

  initContainers:          {{ include "agentfs-sidecar.initContainers" .Values.agentfs | nindent 8 }}
  a workload's mounts:     {{ include "agentfs-sidecar.workspaceMount" .Values.agentfs | nindent 12 }}
  the sandbox overlay's:   {{ include "agentfs-sidecar.sandboxOverlayMount" .Values.agentfs | nindent 12 }}
  volumes:                 {{ include "agentfs-sidecar.volumes" .Values.agentfs | nindent 8 }}
  pod annotations:         {{ include "agentfs-sidecar.podAnnotations" .Values.agentfs | nindent 8 }}
  objects:                 agentfs-sidecar.configMap, .secret, .claim

The pod must also stop slowly enough for the final commit
(terminationGracePeriodSeconds: 60) and run one at a time on the claim
(strategy Recreate): docs/deployment.md.

  /data                 the claim: backing/ (the files) and state/ (history)
  /agentfs              an emptyDir shared by every container
  /agentfs/workspace    the FUSE mount: what the workload uses
  /agentfs-sandbox/overlay  with sandboxOverlay.enabled, the sandbox
                        overlay (design §2.4): for a container that starts
                        other containers (sandboxOverlay.mountPath)
*/}}

{{- define "agentfs-sidecar.image" -}}
{{- if not (or .image.tag .image.digest) }}
{{- fail "agentfs-sidecar: set image.tag or image.digest (pin a commit tag or a digest)" }}
{{- end }}
image: {{ .image.repository }}{{ with .image.tag }}:{{ . }}{{ end }}{{ with .image.digest }}@{{ . }}{{ end }}
{{- with .image.pullPolicy }}
imagePullPolicy: {{ . }}
{{- end }}
{{- end }}

{{/* The init container and the recorder, for the pod's initContainers. */}}
{{- define "agentfs-sidecar.initContainers" -}}
{{- /* values.schema.json checks the rest; these defaults are empty. */}}
{{- if not .volume }}
{{- fail "agentfs-sidecar: set volume (the agentfs volume name)" }}
{{- end }}
{{- if and .sandboxOverlay.enabled (not .sandboxOverlay.owner) }}
{{- fail "agentfs-sidecar: set sandboxOverlay.owner (UID:GID, the sandbox overlay's view owner)" }}
{{- end }}
# Creates the state area on first start, checks it on every later one,
# creates /data/backing, and sets the operator config include.
- name: agentfs-init
  {{- include "agentfs-sidecar.image" . | nindent 2 }}
  args: [init, --include, /etc/agentfs/config]
  env:
    - { name: AGENTFS_VOLUME, value: {{ .volume | quote }} }
    - { name: AGENTFS_STATE, value: /data/state }
    - { name: AGENTFS_ROOT, value: /data/backing }
  securityContext:
    runAsUser: 0
    runAsGroup: 0
    allowPrivilegeEscalation: false
    capabilities:
      drop: [ALL]
    readOnlyRootFilesystem: true
  volumeMounts:
    - { name: data, mountPath: /data }
    - { name: config, mountPath: /etc/agentfs, readOnly: true }
# The recorder: a native sidecar. Workload containers start once its
# startup probe sees the mount, and it is stopped after them.
- name: agentfs
  {{- include "agentfs-sidecar.image" . | nindent 2 }}
  restartPolicy: Always
  args: [mount, /data/backing, /agentfs/workspace]
  env:
    - { name: AGENTFS_STATE, value: /data/state }
    - { name: AGENTFS_ROOT, value: /data/backing }
    - { name: AGENTFS_ON_STALE_MOUNT, value: {{ .onStaleMount }} }
    - { name: AGENTFS_SEAL_MOUNTPOINT, value: {{ .sealMountpoint | toString | quote }} }
    - name: AGENTFS_POD
      valueFrom: { fieldRef: { fieldPath: metadata.name } }
    - name: AGENTFS_POD_UID
      valueFrom: { fieldRef: { fieldPath: metadata.uid } }
    - name: AGENTFS_NAMESPACE
      valueFrom: { fieldRef: { fieldPath: metadata.namespace } }
    - name: AGENTFS_NODE
      valueFrom: { fieldRef: { fieldPath: spec.nodeName } }
    {{- with .env }}
    {{- toYaml . | nindent 4 }}
    {{- end }}
    {{- if .sandboxOverlay.enabled }}
    # The sandbox overlay's mount point, in an emptyDir of its own (it may
    # not share a parent with /agentfs/workspace), and its view owner.
    - { name: AGENTFS_SANDBOX_OVERLAY, value: {{ include "agentfs-sidecar.sandboxOverlayPath" . | quote }} }
    - { name: AGENTFS_SANDBOX_OWNER, value: {{ .sandboxOverlay.owner | quote }} }
    {{- end }}
  # Privileged: Bidirectional propagation, mount(2) and /dev/fuse
  # (docs/deployment.md "Security trade-offs").
  securityContext:
    privileged: true
    runAsUser: 0
    runAsGroup: 0
    readOnlyRootFilesystem: true
  volumeMounts:
    - { name: data, mountPath: /data }
    - { name: agentfs-mount, mountPath: /agentfs, mountPropagation: Bidirectional }
    - { name: config, mountPath: /data/state/git/info/exclude, subPath: exclude, readOnly: true }
    - { name: config, mountPath: /data/state/git/info/attributes, subPath: attributes, readOnly: true }
    - { name: config, mountPath: /etc/agentfs, readOnly: true }
    - { name: credentials, mountPath: /run/secrets/agentfs, readOnly: true }
    {{- if .sandboxOverlay.enabled }}
    - { name: {{ .sandboxOverlay.volumeName }}, mountPath: {{ .sandboxOverlay.mountPath }}, mountPropagation: Bidirectional }
    {{- end }}
  # Passes for a live mount and for the stale one a crash left (5 minutes
  # for journal recovery at start). The sandbox overlay is mounted after
  # the main mount, so probing it gates on both.
  startupProbe:
    exec:
      command: [agentfs, probe, --allow-stale, {{ include "agentfs-sidecar.probeArgs" . }}]
    periodSeconds: 2
    failureThreshold: 150
  # A dead daemon shows as a NotReady pod. No livenessProbe: the kubelet
  # already restarts a daemon that exits.
  readinessProbe:
    exec:
      command: [agentfs, probe, {{ include "agentfs-sidecar.probeArgs" . }}]
    periodSeconds: 10
    timeoutSeconds: 5
  {{- with .resources }}
  resources:
    {{- toYaml . | nindent 4 }}
  {{- end }}
{{- end }}

{{/*
The workload container's mount: /agentfs read-only, so a write while no
daemon is mounted fails with EROFS instead of landing unrecorded; the FUSE
mount propagated into /agentfs/workspace stays writable.
*/}}
{{- define "agentfs-sidecar.workspaceMount" -}}
- name: agentfs-mount
  mountPath: /agentfs
  mountPropagation: HostToContainer
  readOnly: true
  recursiveReadOnly: Disabled
{{- end }}

{{/* The sandbox overlay's mount point: <mountPath>/overlay. */}}
{{- define "agentfs-sidecar.sandboxOverlayPath" -}}
{{ .sandboxOverlay.mountPath }}/overlay
{{- end }}

{{/* What the sidecar's probes check: the sandbox overlay when served, else the mount. */}}
{{- define "agentfs-sidecar.probeArgs" -}}
{{- if .sandboxOverlay.enabled -}}
--sandbox-overlay, {{ include "agentfs-sidecar.sandboxOverlayPath" . }}
{{- else -}}
/agentfs/workspace
{{- end }}
{{- end }}

{{/*
The mount for a container that starts other containers (for example a
Docker daemon), with sandboxOverlay.enabled: only the sandbox overlay's
emptyDir, never the main mount's. It binds paths under
<mountPath>/overlay into its sandboxes; a container runtime binds with
private propagation by default, so sandboxes started before a sidecar
recovery keep the dead mount (docs/deployment.md §12).
*/}}
{{- define "agentfs-sidecar.sandboxOverlayMount" -}}
- name: {{ .sandboxOverlay.volumeName }}
  mountPath: {{ .sandboxOverlay.mountPath }}
  mountPropagation: HostToContainer
{{- end }}

{{/* The pod's volumes for the helpers above. */}}
{{- define "agentfs-sidecar.volumes" -}}
- name: data
  persistentVolumeClaim:
    claimName: {{ .claim.name }}
- name: agentfs-mount
  emptyDir: {}
- name: config
  configMap:
    name: {{ .config.name }}
- name: credentials
  secret:
    secretName: {{ .credentials.secretName }}
    defaultMode: 0400
{{- if .sandboxOverlay.enabled }}
- name: {{ .sandboxOverlay.volumeName }}
  emptyDir: {}
{{- end }}
{{- end }}

{{/* Rolls the pod when the settings or rules change: they are read at mount (design §4.1). */}}
{{- define "agentfs-sidecar.podAnnotations" -}}
checksum/agentfs-config: {{ include "agentfs-sidecar.configMap" . | sha256sum }}
{{- end }}

{{- define "agentfs-sidecar.labels" -}}
{{- with .labels }}
labels:
  {{- toYaml . | nindent 2 }}
{{- end }}
{{- end }}

{{/* The operator's settings and rules. */}}
{{- define "agentfs-sidecar.configMap" -}}
apiVersion: v1
kind: ConfigMap
metadata:
  name: {{ .config.name }}
  {{- include "agentfs-sidecar.labels" . | nindent 2 }}
data:
  config: {{ .config.settings | quote }}
  exclude: {{ .config.exclude | quote }}
  attributes: {{ .config.attributes | quote }}
{{- end }}

{{/* The credentials Secret, from credentials.stringData. */}}
{{- define "agentfs-sidecar.secret" -}}
apiVersion: v1
kind: Secret
metadata:
  name: {{ .credentials.secretName }}
  {{- include "agentfs-sidecar.labels" . | nindent 2 }}
type: Opaque
{{- with .credentials.stringData }}
stringData:
  {{- toYaml . | nindent 2 }}
{{- end }}
{{- end }}

{{/*
The claim: backing/ and state/ as siblings on one filesystem, mounted once
at /data (recording freezes files with reflinks, which fail across mounts;
design §3.2).
*/}}
{{- define "agentfs-sidecar.claim" -}}
apiVersion: v1
kind: PersistentVolumeClaim
metadata:
  name: {{ .claim.name }}
  {{- include "agentfs-sidecar.labels" . | nindent 2 }}
spec:
  accessModes:
    {{- toYaml .claim.accessModes | nindent 4 }}
  resources:
    requests:
      storage: {{ .claim.size }}
  {{- with .claim.storageClassName }}
  storageClassName: {{ . }}
  {{- end }}
{{- end }}
