; What a reference is, for SQL. Every name in this grammar is an `identifier`;
; what it names is told by the node above it.

; A table, view, type or sequence named anywhere: in `FROM`, in a `JOIN`, in a
; `CREATE`, as a function's return type. All of them write it as the `name`
; of an `object_reference`, and the qualifier before it -- `schema.` -- is a
; separate `identifier` this pattern deliberately leaves alone.
(object_reference
  name: (identifier) @reference.type)

; A function called: `now()`, `shop.greet(who)`.
(invocation
  (object_reference
    name: (identifier) @reference.call))

; A column read: `SELECT display_name`, `WHERE customers.display_name = $1`.
(field
  column: (identifier) @reference.field)

; A column declared, so that a rename has to change its definition too.
(column_definition
  name: (identifier) @reference.field)

; Every other plain name: an alias, a schema qualifier, a role, a parameter.
(identifier) @reference.value
