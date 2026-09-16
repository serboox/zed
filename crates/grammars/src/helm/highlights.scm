; What the template itself says. Everything between the actions is YAML, and
; is read as YAML -- see injections.scm.

[
  (field)
  (field_identifier)
] @property

(variable) @variable

(function_call
  function: (identifier) @function)

(method_call
  method: (selector_expression
    field: (field_identifier) @function))

[
  "if"
  "else"
  "end"
  "range"
  "with"
  "template"
  "define"
  "block"
] @keyword

[
  "|"
  ":="
] @operator

[
  "{{"
  "}}"
  "{{-"
  "-}}"
  "("
  ")"
] @punctuation.bracket

[
  "."
  ","
] @punctuation.delimiter

[
  (interpreted_string_literal)
  (raw_string_literal)
  (rune_literal)
] @string

(escape_sequence) @string.escape

[
  (int_literal)
  (float_literal)
  (imaginary_literal)
] @number

[
  (true)
  (false)
  (nil)
] @constant

(comment) @comment
