; What a reference is, for JavaScript. The same file as the TypeScript one
; minus its type patterns: this grammar has no `type_identifier` at all, and a
; query naming a node kind the grammar does not have fails to compile rather
; than quietly matching nothing.
;
; A declaration's own name is not excluded here -- the grammar gives no
; structural way to tell a declaring position from a using one -- so the
; JavaScript side of this measurement drops a match at a position the outline
; query already knows to be a declaration.

; Calls: the function's name, however it is written. `thing.method()` and
; `namespace.fn()` are the same shape to the grammar.
(call_expression
  function: [
    (identifier) @reference.call
    (member_expression
      property: (property_identifier) @reference.call)
  ])

; A property read from an object: `holder.value`. A method call's own property
; also matches this pattern, and the JavaScript side of this measurement keeps
; that one occurrence once, not once per pattern that described it.
(member_expression
  property: (property_identifier) @reference.field)

; A property named in an object literal, `{ value: 42 }`. Renaming the
; property has to change this, and the grammar gives the key its own node
; kind, so the widest pattern below would not reach it.
(pair
  key: (property_identifier) @reference.field)

; The shorthand form, `{ value }`, where one word is both the property and the
; variable read into it. It is captured once and counts for both, because
; nothing here can tell which of the two a rename meant.
(shorthand_property_identifier) @reference.field

; Every other occurrence of a plain name: a variable, a function passed by
; name, a class used as a value, an imported name.
;
; This is deliberately the widest pattern in the file, and it is safe only
; because of what sits above it: a declaring position is dropped, and the
; index declines to answer at all about a name that means more than one thing
; in the project.
(identifier) @reference.value
