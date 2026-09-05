; What a reference is, for PHP. The grammar spells almost every name `name`
; -- a class, a function, a method, a constant and a property all use it --
; and the one name it marks apart is a variable, which is a `variable_name`
; wrapping a `name`. Types it does mark: a type written in a signature sits
; under `named_type`, so in this language a type use is one of the few things
; the grammar can say by itself rather than by position.

; Calls. A free function, an instance method and a static method are three
; different nodes to this grammar, so each is named. A leading `\` or a
; namespace prefix makes the callee a `qualified_name`, whose last segment is
; the function.
(function_call_expression
  function: [
    (name) @reference.call
    (qualified_name
      (name) @reference.call)
  ])
(member_call_expression
  name: (name) @reference.call)
(nullsafe_member_call_expression
  name: (name) @reference.call)
(scoped_call_expression
  name: (name) @reference.call)

; A type: a parameter's or a return's type, a `catch` clause's type list, an
; `extends` or an `implements` clause, a `new`, an attribute.
(named_type [
  (name) @reference.type
  (qualified_name
    (name) @reference.type)
])
(base_clause [
  (name) @reference.type
  (qualified_name
    (name) @reference.type)
])
(class_interface_clause [
  (name) @reference.type
  (qualified_name
    (name) @reference.type)
])
(object_creation_expression [
  (name) @reference.type
  (qualified_name
    (name) @reference.type)
])
(attribute [
  (name) @reference.type
  (qualified_name
    (name) @reference.type)
])

; A property read from an object: `$holder->value`, `Holder::$shared`.
(member_access_expression
  name: (name) @reference.field)
(nullsafe_member_access_expression
  name: (name) @reference.field)
(scoped_property_access_expression
  name: (variable_name
    (name) @reference.field))

; A named argument names the parameter it fills: `open(path: $name)`.
; Renaming that parameter has to change this, which is why it is captured
; rather than left to the widest pattern.
(argument
  name: (name) @reference.field)

; Every other name: a variable, a global constant, a class constant, a
; namespace segment, a `use` import. `Holder::LIMIT` is deliberately left
; here rather than given its own pattern: the grammar puts the class and the
; constant side by side under `class_constant_access_expression` with no
; field on either, so a pattern would have to guess which of the two it had,
; and would claim knowledge the grammar does not have.
(name) @reference.value
