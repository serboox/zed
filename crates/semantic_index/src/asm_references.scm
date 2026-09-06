; What a reference is, for assembly. This grammar has no `identifier` at all:
; a name is a `word` or an `ident`, so the query built from a grammar's own
; node kinds finds nothing here -- which for assembly is not the truthful
; answer it is for YAML. A label is exactly the kind of name a reader wants
; to follow.

; A label's own name, and the name a constant is given. Both spellings of a
; label are captured because the outline query records both. A declaring
; position is dropped again before the answer leaves -- that is the pipeline's
; rule, not this query's.
(label
  [(word) (ident)] @reference.value)
(const
  name: (word) @reference.value)

; A name written as an operand: `call greet`, `jmp .Lloop`, `mov rax, count`.
;
; A register is spelled as an `ident` wrapping a `reg` in this grammar and is
; captured along with them. That is deliberate rather than overlooked: nothing
; in a project declares `rax`, so such an occurrence resolves to no declaration
; and is dropped, and the alternative -- asking a query for an `ident` that has
; no `reg` under it -- is not something a query can say.
(instruction
  (ident) @reference.value)
(meta
  (ident) @reference.value)
(ptr
  (ident) @reference.value)
