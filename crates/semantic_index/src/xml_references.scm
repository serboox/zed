; What a reference is, for XML. This grammar spells every name `Name`, and none
; of the eight identifier-shaped kinds the query built from a grammar's own node
; kinds looks for exists here, so that fallback compiles to no patterns at all
; and finds nothing -- the same silence assembly and COBOL were in.

; Every mention of an element type: the opening tag, the closing tag and the
; empty-element form. All three, because renaming an element means changing all
; three, and the grammar carries no link between a tag and its partner for a
; query to follow.
(STag
  (Name) @reference.value)
(ETag
  (Name) @reference.value)
(EmptyElemTag
  (Name) @reference.value)

; An attribute name. Captured as a value rather than as a field: which element
; an attribute belongs to is what makes it that element's field, and only a
; schema says so -- the document does not.
(Attribute
  (Name) @reference.value)

; A general entity used, `&chapter;`, and a parameter entity used, `%common;`.
(EntityRef
  (Name) @reference.value)
(PEReference
  (Name) @reference.value)

; The declaring positions an internal subset holds, so that a rename has to
; change them as well. The name an `ATTLIST` declares is the element's, not the
; attribute's; the attribute names it lists are `AttDef`s under it.
(elementdecl
  (Name) @reference.value)
(AttlistDecl
  (Name) @reference.value)
(AttDef
  (Name) @reference.value)
(GEDecl
  (Name) @reference.value)
