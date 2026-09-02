; What a reference is, for C. Written from tree-sitter-c's own node types.
;
; A declaration's own name is not excluded here -- the grammar writes `int x;`
; and a read of `x` with the same node kind -- so the C side of this
; measurement drops a match at a position the outline query already knows to
; be a declaration.

; Calls: the function's name, however it is written. A call through a struct
; member -- `handlers.open(...)` -- is a field selection to the grammar.
(call_expression
  function: [
    (identifier) @reference.call
    (field_expression
      field: (field_identifier) @reference.call)
  ])

; A member read from a struct or a union, through `.` or `->`; the grammar
; spells both the same way and puts the operator in its own field.
(field_expression
  field: (field_identifier) @reference.field)

; The member named in a designated initialiser: `struct Point p = {.x = 1}`.
; The grammar wraps it in a designator of its own, so the widest pattern below
; would not reach it -- and it is exactly a place renaming the member has to
; change.
(field_designator
  (field_identifier) @reference.field)

; Uses of a type: a `struct`, `union` or `enum` tag, and any name a `typedef`
; introduced. A primitive type is its own node kind and is deliberately not
; here: `int` is not a name anybody renames.
(type_identifier) @reference.type

; Every other occurrence of a plain name: a variable, a function passed by
; name, an enum constant, a macro's name where it is used.
;
; This is deliberately the widest pattern in the file, and it is safe only
; because of what sits above it: a declaring position is dropped, and the
; index declines to answer at all about a name that means more than one thing
; in the project.
(identifier) @reference.value
