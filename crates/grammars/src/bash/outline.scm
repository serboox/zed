(function_definition
  name: (word) @name) @item

; Only the script's own top-level names. An assignment inside a function is a
; local, and recording those would fill the project's symbols with `i` and `tmp`.
(program
  (variable_assignment
    name: (variable_name) @name) @item)

; `export NAME=`, `readonly NAME=`, `declare NAME=` at the top of the script.
; The grammar wraps those in a `declaration_command`, so the pattern above --
; which asks for a direct child of the program -- does not see them. The same
; wrapper holds `local` inside a function, and that one stays out because the
; program is still the parent this pattern asks for.
(program
  (declaration_command
    (variable_assignment
      name: (variable_name) @name) @item))
