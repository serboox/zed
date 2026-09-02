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

; A field named in a struct expression: `Holder { value: 42 }`. The grammar
; gives this its own node, and the name is a `field_identifier` rather than
; the `identifier` the widest pattern below would catch -- so without this
; pattern a field's own initialiser was not a reference to it, which is
; exactly the place a rename has to change. The shorthand form,
; `Holder { value }`, *is* a plain `identifier` and needs no pattern here.
(field_initializer
  field: (field_identifier) @reference.field)

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

; Every other occurrence of a plain name. A constant read, a function passed
; by name, an enum variant, a name brought in by `use` -- none of these are a
; call, a field or a type, and all of them are references a person renaming
; the symbol would have to change.
;
; This is deliberately the widest pattern in the file, and it is safe only
; because of what sits above it: the Rust side drops any match at a position
; the outline query already calls a declaration, and the index declines to
; answer at all about a name that means more than one thing in the project.
; Without that second rule this pattern would be a precision disaster --
; measured, before it existed, at 0.9 per cent.
(identifier) @reference.value
