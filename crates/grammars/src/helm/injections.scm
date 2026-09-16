; Everything outside the actions is YAML, and it is one document rather than a
; run of fragments: a mapping opened above an `{{ if }}` is closed below its
; `{{ end }}`, and read piece by piece neither half is YAML at all.
((text) @injection.content
  (#set! injection.language "yaml")
  (#set! injection.combined))

((comment) @injection.content
  (#set! injection.language "comment"))
