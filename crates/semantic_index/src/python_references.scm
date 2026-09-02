; What a reference is, for Python. This file is short for a reason worth
; stating: the grammar has one node kind for almost every name there is.
; There is no `type_identifier` and no `field_identifier` -- a class used as a
; type, an attribute read from an object, a function called, a keyword
; argument's name and a local variable are all `identifier`. So the patterns
; below do not narrow anything down; they exist to *name* what each match is,
; which is what the error catalogue reads.
;
; Everything that keeps this honest therefore lives outside the query: a match
; at a position the outline query calls a declaration is dropped, and the
; index declines to answer at all about a name that means more than one thing
; in the project. Python has more such names than Rust or Go do, and the
; measurement says so rather than hiding it.

; Calls: the function's name, however it is written. `thing.method()` and
; `module.function()` are the same shape to the grammar.
(call
  function: [
    (identifier) @reference.call
    (attribute
      attribute: (identifier) @reference.call)
  ])

; An attribute read from an object: `holder.value`. A method call's own
; attribute also matches this pattern, and the Python side of this
; measurement keeps that one occurrence once, not once per pattern that
; described it.
(attribute
  attribute: (identifier) @reference.field)

; A keyword argument names the parameter it fills: `open(file=path)`. Renaming
; that parameter has to change this, which is why it is captured rather than
; left to the widest pattern -- the catalogue can then say that a divergence
; here is about a parameter and not about a variable.
(keyword_argument
  name: (identifier) @reference.field)

; Every other occurrence of a plain name: a class used as a type or a base, a
; constant read, a function passed by name, a decorator, a module in an
; import, a variable.
;
; This is the widest pattern in the file and, in this language, very nearly
; the only one that matters. It is safe only because of the two rules above
; it: a declaring position is dropped, and an ambiguous name is not answered
; about at all.
(identifier) @reference.value
