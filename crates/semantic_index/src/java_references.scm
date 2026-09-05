; What a reference is, for Java. The grammar separates types from values, so
; the patterns here narrow more than Python's can: `type_identifier` is only
; ever a type, and a method call is always `method_invocation`.

; Calls: `run()`, `holder.run()`, `Type.run()` are all one shape.
(method_invocation
  name: (identifier) @reference.call)

; A type named anywhere: a declaration's supertype, a parameter, a cast, a
; `new` expression, a generic argument.
(type_identifier) @reference.type

; A field read from an object: `holder.value`.
(field_access
  field: (identifier) @reference.field)

; The scope half of a qualified name: the `Map` in `Map.Entry`, the package
; segments in an import.
(scoped_identifier
  name: (identifier) @reference.value)

; An annotation names the type it applies: `@Override`.
(annotation
  name: (identifier) @reference.type)
(marker_annotation
  name: (identifier) @reference.type)

; Every other plain name: a variable read, a constant, a static import.
(identifier) @reference.value
