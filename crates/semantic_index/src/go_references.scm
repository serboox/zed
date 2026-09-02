; What a reference is, for Go: an occurrence of a name that is not the place
; where it was declared. Written from tree-sitter-go's own node types, the
; same way `rust_references.scm` is written from Rust's, and named with the
; `@reference.*` captures a tags.scm file conventionally uses.
;
; A declaration's own name is not excluded here, because the grammar gives no
; structural way to tell a declaring position from a using one: `func Foo()`
; and a call to `Foo` both write an `identifier`. The Go side of this
; measurement drops a match at a position the outline query already knows to
; be a declaration.

; Calls: the function's name, however it is written. `pkg.Fn()` and
; `value.Method()` are the same shape to the grammar -- a selector -- and both
; are a rename's business.
(call_expression
  function: [
    (identifier) @reference.call
    (selector_expression
      field: (field_identifier) @reference.call)
  ])

; A field or method selected from a value. A method call's own selector also
; matches this pattern -- `x.Fn()` is both a call and, structurally, a
; selection -- and the Go side of this measurement keeps that one occurrence
; once, not once per pattern that described it.
(selector_expression
  field: (field_identifier) @reference.field)

; Uses of a type: every type name the grammar sees, wherever it is written --
; a variable's type, a parameter, a result, a slice or map element, a type
; argument, an embedded field. `pkg.Type` writes the name as a
; `type_identifier` under `qualified_type`, so this one pattern covers the
; qualified form as well.
(type_identifier) @reference.type

; A field named in a composite literal, `Holder{value: 42}`, is deliberately
; *not* given its own pattern. The grammar cannot tell a struct's key from a
; map's: both are a `keyed_element` whose key is an expression, and a pattern
; here would claim knowledge the grammar does not have. The key is a plain
; identifier, so the widest pattern below finds it anyway -- and for a map
; literal it finds a value expression, which is honestly what it is.

; Every other occurrence of a plain name: a constant read, a function passed
; by name, a variable, a package qualifier, a struct literal's key.
;
; This is deliberately the widest pattern in the file, and it is safe only
; because of what sits above it: the Go side drops any match at a position the
; outline query already calls a declaration, and the index declines to answer
; at all about a name that means more than one thing in the project. Without
; that second rule this pattern would be a precision disaster -- measured on
; Rust, before the rule existed, at 0.9 per cent.
(identifier) @reference.value
