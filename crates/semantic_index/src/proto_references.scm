; What a reference is, for Proto. Unlike assembly, COBOL and XML, the query
; built from a grammar's own node kinds does compile here: this grammar spells
; every name `identifier`, so the fallback captures all of them. It also
; captures `constant`, which is this grammar's word for a *value* -- a literal
; or a name used as one -- so the fallback puts every string and number in the
; file into the index as a reference. This query keeps the names and leaves the
; values where they are.

; Every mention of a message or an enum by type: a field's type, a map's value
; type, an rpc's request and its response. `shop.v1.Order` writes each segment
; as its own identifier and the grammar carries no link between them, so all of
; them are captured -- renaming the message changes the last, renaming its
; package changes the first.
(message_or_enum_type
  (identifier) @reference.type)

; The message an `extend` block reopens, and the package the file declares.
(extend
  (full_ident
    (identifier) @reference.type))
(package
  (full_ident
    (identifier) @reference.type))

; The declaring positions, so that a rename has to change them as well. The
; grammar gives each of these its own node kind, which is why they are named
; here rather than left to a widest-pattern rule the way Go's are.
(message_name) @reference.type
(enum_name) @reference.type
(service_name) @reference.type
(rpc_name) @reference.call

; A field, wherever it is declared: in a message, in a oneof, or as a map. The
; oneof's own name is a field too -- it is the name generated code gives the
; union -- and so is an enum's value.
(field
  (identifier) @reference.field)
(oneof_field
  (identifier) @reference.field)
(map_field
  (identifier) @reference.field)
(oneof
  (identifier) @reference.field)
(enum_field
  (identifier) @reference.field)

; An option's name. A bare one, `option java_package = ...`, names a field on a
; descriptor message that is not in this project; a parenthesized one names a
; custom option this project may well declare, and that one a rename has to
; follow.
(option
  (identifier) @reference.value)
(option
  (full_ident
    (identifier) @reference.value))
(field_option
  (full_ident
    (identifier) @reference.value))
(enum_value_option
  (full_ident
    (identifier) @reference.value))
