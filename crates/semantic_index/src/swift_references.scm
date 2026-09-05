; What a reference is, for Swift. This grammar does separate types from
; values: a type name is a `type_identifier` and always sits under a
; `user_type`, while every other name is a `simple_identifier`. What it does
; not give is a `function` field on a call -- the callee is simply the first
; child of a `call_expression` -- so the two call patterns below anchor on
; that position instead of naming a field.

; Calls: `run()` and `holder.run()`.
(call_expression
  .
  (simple_identifier) @reference.call)
(call_expression
  .
  (navigation_expression
    suffix: (navigation_suffix
      suffix: (simple_identifier) @reference.call)))

; A type named anywhere: an annotation, a return type, an inheritance clause,
; a generic argument, a constructed type. All of them write the name as a
; `type_identifier` under a `user_type`, so this one pattern covers every
; type position the language has.
(user_type
  (type_identifier) @reference.type)

; A member read from a value: `holder.value`. A method call's own suffix also
; matches this pattern, and the Swift side of this measurement keeps that one
; occurrence once, not once per pattern that described it.
(navigation_suffix
  suffix: (simple_identifier) @reference.field)

; An argument label names the parameter it fills: `open(path: name)`.
; Renaming that parameter has to change this, which is why it is captured
; rather than left to the widest pattern.
(value_argument
  name: (value_argument_label
    (simple_identifier) @reference.field))

; Every other plain name: a variable read, a function passed by name, an enum
; case, a label on a statement.
(simple_identifier) @reference.value
