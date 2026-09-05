; What a reference is, for C#. The grammar has one node kind for every name:
; there is no `type_identifier` and no `field_identifier`, so `Holder`, `Run`
; and `Value` are all `identifier`. What it does have is fields on the nodes
; around a name, and that is what the patterns below read -- a name in a
; `type:` or a `returns:` field is a type because of where it sits, not
; because of anything the name itself says.

; Calls: `Run()`, `holder.Run()`, `Type.Run()`, `Run<T>()` are all one shape.
(invocation_expression
  function: [
    (identifier) @reference.call
    (generic_name
      (identifier) @reference.call)
    (member_access_expression
      name: [
        (identifier) @reference.call
        (generic_name
          (identifier) @reference.call)
      ])
    (qualified_name
      name: [
        (identifier) @reference.call
        (generic_name
          (identifier) @reference.call)
      ])
  ])

; A member read from a value: `holder.Value`. A method call's own name also
; matches this pattern -- `x.Run()` is both a call and, structurally, a member
; access -- and the C# side of this measurement keeps that one occurrence
; once, not once per pattern that described it.
(member_access_expression
  name: (identifier) @reference.field)

; A type named where the grammar has a field that says so.
(variable_declaration
  type: (identifier) @reference.type)
(parameter
  type: (identifier) @reference.type)
(method_declaration
  returns: (identifier) @reference.type)
(property_declaration
  type: (identifier) @reference.type)
(object_creation_expression
  type: (identifier) @reference.type)
(cast_expression
  type: (identifier) @reference.type)
(catch_declaration
  type: (identifier) @reference.type)
(declaration_expression
  type: (identifier) @reference.type)
(array_type
  type: (identifier) @reference.type)
(nullable_type
  type: (identifier) @reference.type)

; A type named in a position the grammar marks with a node rather than a
; field: a base class or interface list, a generic argument, the name a
; generic type is written with, an attribute.
(base_list
  (identifier) @reference.type)
(type_argument_list
  (identifier) @reference.type)
(generic_name
  (identifier) @reference.type)
(attribute
  name: (identifier) @reference.type)

; The last segment of a dotted name: the `Console` in `System.Console`. The
; grammar writes a namespace segment and a type name the same way, so this
; says only that the name was written as part of a qualified one -- calling
; it a type would be wrong in a `using` directive, which is the other place
; this shape appears.
(qualified_name
  name: (identifier) @reference.value)

; A named argument names the parameter it fills: `Open(path: name)`. Renaming
; that parameter has to change this, which is why it is captured rather than
; left to the widest pattern -- the catalogue can then say a divergence here
; is about a parameter and not about a variable.
(argument
  name: (identifier) @reference.field)

; Every other plain name: a variable read, a constant, a namespace segment, a
; `using` alias, a type in a position no field marks.
(identifier) @reference.value
