(comment) @annotation

(message
  "message" @context
  (message_name) @name) @item

(enum
  "enum" @context
  (enum_name) @name) @item

(service
  "service" @context
  (service_name) @name) @item

(rpc
  "rpc" @context
  (rpc_name) @name) @item

(oneof
  "oneof" @context
  (identifier) @name) @item

; The message an `extend` block reopens. Its own name is the full identifier,
; which is what a reader looking for the extension would search for.
(extend
  "extend" @context
  (full_ident) @name) @item

; A field's type is context rather than part of its name, so the outline reads
; as a list of field names with their types beside them.
(field
  (type) @context
  (identifier) @name) @item

(oneof_field
  (type) @context
  (identifier) @name) @item

(map_field
  "map" @context
  (identifier) @name) @item

(enum_field
  (identifier) @name) @item
