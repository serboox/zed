; What a reference is, for Ruby. The grammar has no type node and no field
; node, because the language has neither: a class is just a constant, and
; `holder.value` is a method call whether it reads an attribute or computes
; something. So the narrowing here is honestly small -- a superclass clause
; is the only position where a name is certainly a type -- and the rest of
; the file names what the grammar does mark: a call's method, an instance or
; class variable, and a hash key.

; Calls: `run`, `holder.run`, `Thing.run`. An attribute read written without
; parentheses is this same shape, and the grammar cannot tell the two apart.
(call
  method: [
    (identifier) @reference.call
    (constant) @reference.call
  ])

; The one position a name is certainly a type: `class Child < Parent`.
(superclass [
  (constant) @reference.type
  (scope_resolution
    name: (constant) @reference.type)
])

; State read off an object: `@value`, `@@count`. The sigil is part of the
; node's own text, and renaming the attribute has to change this.
(instance_variable) @reference.field
(class_variable) @reference.field

; A hash key: `open(path: name)` and `{ path: name }` are the same node, so
; this covers a keyword argument's name as well as a literal's key. It is a
; field rather than a value because in the argument case it names the
; parameter it fills.
(pair
  key: (hash_key_symbol) @reference.field)

; A constant: a class, a module, or a constant value. The grammar writes all
; three the same way, so this says only that a constant was named -- and the
; `Bar` in `Foo::Bar` lands here too, since a scope resolution's name is a
; constant like any other.
(constant) @reference.value

; Every other plain name: a local variable, a method passed by name, a
; parameter's use, a global.
;
; This is the widest pattern in the file, and it is safe only because of what
; sits outside the query: a match at a position the outline query already
; calls a declaration is dropped, and the index declines to answer at all
; about a name that means more than one thing in the project.
(identifier) @reference.value
(global_variable) @reference.value
