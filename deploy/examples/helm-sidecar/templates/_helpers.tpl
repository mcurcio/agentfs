{{/* The sidecar helpers' values, with the settings and rules read from files/. */}}
{{- define "example.agentfs" -}}
{{- $files := dict "settings" (.Files.Get "files/config") "exclude" (.Files.Get "files/exclude") "attributes" (.Files.Get "files/attributes") }}
{{- toYaml (mustMergeOverwrite (deepCopy .Values.agentfs) (dict "config" $files)) }}
{{- end }}
