(function_definition
  name: (word) @name) @item

; Only the script's own top-level names. An assignment inside a function is a
; local, and recording those would fill the project's symbols with `i` and `tmp`.
(program
  (variable_assignment
    name: (variable_name) @name) @item)
