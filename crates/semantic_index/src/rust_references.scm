; What a reference is, for Rust: an occurrence of a name that is not the
; place where it was declared. Captures are named the way a tags.scm file's
; `@reference.*` captures conventionally are -- this fork ships no tags.scm to
; copy, so these patterns are written from the grammar's own node types,
; verified against tree-sitter-rust's own node-types.json, the same way
; outline.scm and highlights.scm already read the grammar for definitions and
; syntax colouring.
;
; A declaration's own name is not excluded here: `struct Foo` matches
; `(type_identifier)` exactly as a use of `Foo` elsewhere would, because the
; grammar gives no structural way to tell a declaring position apart from a
; using one in general -- a type's own name and a reference to it are the
; same node kind in every context that names one. The Rust side of this
; measurement drops a match at a position it already knows, from the outline
; query, to be a declaration's own name.

; Calls: the function name, however it is written.
(call_expression
  function: [
    (identifier) @reference.call
    (scoped_identifier
      name: (identifier) @reference.call)
    (field_expression
      field: (field_identifier) @reference.call)
  ])

; Calls with an explicit turbofish, e.g. `Vec::<T>::new()`: the grammar gives
; these their own node instead of nesting them under `call_expression`.
(generic_function
  function: [
    (identifier) @reference.call
    (scoped_identifier
      name: (identifier) @reference.call)
    (field_expression
      field: (field_identifier) @reference.call)
  ])

; Field access: reading or writing a named field. A method call's own field
; expression also matches this pattern -- `x.foo()` is both a call and,
; structurally, a field access -- and the Rust side of this measurement keeps
; that one occurrence once, not once per pattern that happened to describe
; it.
(field_expression
  field: (field_identifier) @reference.field)

; Macro invocations: the macro's own name, not its argument tokens -- those
; are opaque to this grammar, and a real reference living only inside them
; (an interpolated `format!` argument, for instance) is exactly the kind of
; gap this measurement's error catalogue exists to name.
(macro_invocation
  macro: [
    (identifier) @reference.macro
    (scoped_identifier
      name: (identifier) @reference.macro)
  ])

; Uses of a type: every type name the grammar sees, wherever it is written --
; a type annotation, a generic argument, a return type, a bound, and so on.
; An imported name in a `use` declaration is not a `type_identifier` in this
; grammar at all (it is a plain `identifier`), so it is deliberately out of
; this query's scope; a reference reachable only through an import shows up
; honestly as a miss, named as such in the error catalogue, rather than
; silently supported by a pattern this file does not claim to have.
(type_identifier) @reference.type
