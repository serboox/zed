; What a reference is, for C++. The C patterns, plus the four shapes C++ adds
; for writing a name: a qualified one, a template's, a namespace's, and the
; member a constructor's initialiser list names.
;
; A declaration's own name is not excluded here -- the grammar writes `int x;`
; and a read of `x` with the same node kind -- so the C++ side of this
; measurement drops a match at a position the outline query already knows to
; be a declaration.

; Calls: the function's name, however it is written. A method call is a field
; selection to the grammar; a static or namespaced call is a qualified name,
; whose own last element is an `identifier` the widest pattern below reaches.
(call_expression
  function: [
    (identifier) @reference.call
    (field_expression
      field: (field_identifier) @reference.call)
  ])

; A member read from a class, struct or union, through `.` or `->`.
(field_expression
  field: (field_identifier) @reference.field)

; The member a constructor's initialiser list names -- `Holder() : value(1) {}`
; -- and the one a designated initialiser names. Both get a node of their own,
; so the widest pattern would not reach either, and both are places renaming
; the member has to change.
(field_initializer
  (field_identifier) @reference.field)

(field_designator
  (field_identifier) @reference.field)

; Uses of a type: a class, struct, union or enum name, a `typedef` or `using`
; alias, a template parameter's name where it is used. `Outer::Inner` and
; `Vector<Element>` both write the name itself as a `type_identifier`, so this
; one pattern covers the qualified and the templated forms as well. A
; primitive type is its own node kind and is deliberately not here: `int` is
; not a name anybody renames.
(type_identifier) @reference.type

; A namespace named where it is used: `namespace fs = std::filesystem;` and
; every `fs::` after it. Its own node kind, so it needs its own pattern.
(namespace_identifier) @reference.value

; Every other occurrence of a plain name: a variable, a function passed by
; name, an enum constant, the last element of a qualified name, a macro's name
; where it is used.
;
; This is deliberately the widest pattern in the file, and it is safe only
; because of what sits above it: a declaring position is dropped, and the
; index declines to answer at all about a name that means more than one thing
; in the project.
(identifier) @reference.value
