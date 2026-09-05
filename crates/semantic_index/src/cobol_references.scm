; What a reference is, for COBOL. The grammar has no `identifier`: a data name
; mentioned in a statement is a `WORD` under a `qualified_word`, a data item
; declares itself as an `entry_name`, and a program as a `program_name`.

; Every mention of a data item, a file, an index or an index key, including the
; qualifiers of `OF`/`IN` chains, which are separate `WORD`s of the same
; `qualified_word`.
;
; A paragraph or section named in `PERFORM` or `GO TO` is spelled exactly this
; way too, and is captured here rather than as a call: it sits under a `label`,
; and a query cannot say "a `qualified_word` that is not under a `label`", so
; telling the two apart would mean capturing the labels twice.
(qualified_word
  (WORD) @reference.value)

; The declaration sites, so that a rename has to change them as well.
(entry_name) @reference.value
(program_name) @reference.value
(paragraph_header
  name: (WORD) @reference.value)
(section_header
  name: (WORD) @reference.value)
