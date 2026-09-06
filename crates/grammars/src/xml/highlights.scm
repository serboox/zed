(Comment) @comment

(STag
  (Name) @tag)
(ETag
  (Name) @tag)
(EmptyElemTag
  (Name) @tag)

(Attribute
  (Name) @attribute)
(Attribute
  (AttValue) @string)

(CDSect
  (CData) @string.special)

[
  (EntityRef)
  (CharRef)
  (PEReference)
] @string.escape

"xml" @keyword

[
  "version"
  "encoding"
  "standalone"
] @attribute

(VersionNum) @number
(EncName) @string.special

[
  "yes"
  "no"
] @boolean

(PI
  (PITarget) @keyword)
(XmlModelPI
  "xml-model" @keyword)
(StyleSheetPI
  "xml-stylesheet" @keyword)
(PseudoAtt
  (Name) @attribute)
(PseudoAtt
  (PseudoAttValue) @string)

(doctypedecl
  "DOCTYPE" @keyword)
(doctypedecl
  (Name) @type)

(elementdecl
  "ELEMENT" @keyword)
(elementdecl
  (Name) @tag)
(contentspec
  (_
    (Name) @tag))

"#PCDATA" @type

[
  "EMPTY"
  "ANY"
] @type

(AttlistDecl
  "ATTLIST" @keyword)
(AttlistDecl
  (Name) @tag)
(AttDef
  (Name) @attribute)
(Enumeration
  (Nmtoken) @string)
(DefaultDecl
  (AttValue) @string)

[
  (StringType)
  (TokenizedType)
] @type

(NotationType
  "NOTATION" @type)

[
  "#REQUIRED"
  "#IMPLIED"
  "#FIXED"
] @keyword

(GEDecl
  "ENTITY" @keyword)
(GEDecl
  (Name) @constant)
(GEDecl
  (EntityValue) @string)
(PEDecl
  "ENTITY" @keyword)
(PEDecl
  (Name) @constant)
(PEDecl
  (EntityValue) @string)
(NDataDecl
  "NDATA" @keyword)
(NotationDecl
  "NOTATION" @keyword)
(NotationDecl
  (Name) @constant)

[
  "PUBLIC"
  "SYSTEM"
] @keyword

(PubidLiteral) @string
(SystemLiteral
  (URI) @link_uri)

[
  "\""
  "'"
] @string

[
  "="
  "%"
  "|"
  ","
  "*"
  "?"
  "+"
] @operator

[
  "("
  ")"
  "["
  "]"
] @punctuation.bracket

[
  "<"
  ">"
  "</"
  "/>"
  "<?"
  "?>"
  "<!"
  "<!["
  "]]>"
  "&"
  "&#"
  "&#x"
  ";"
] @punctuation.delimiter
