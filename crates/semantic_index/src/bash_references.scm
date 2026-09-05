; What a reference is, for Bash. The grammar has no single "identifier": a
; name in call position is a `word` under a `command_name`, and every other
; name the script can rename is a `variable_name`.

; A command in call position. Most of them are external programs the project
; does not declare, and those simply find no definition; a function the script
; defines is called exactly the same way, and that is the reference worth
; having.
(command
  name: (command_name
    (word) @reference.call))

; Every read and write of a name: `NAME=`, `$NAME`, `${NAME}`. A positional or
; special parameter -- `$1`, `$@` -- is a `special_variable_name` and is
; deliberately absent: nothing declares it, so nothing can be renamed.
(variable_name) @reference.value
