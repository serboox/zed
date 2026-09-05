(class_declaration
  (modifiers)? @context
  "class" @context
  name: (identifier) @name) @item

(interface_declaration
  (modifiers)? @context
  "interface" @context
  name: (identifier) @name) @item

(enum_declaration
  (modifiers)? @context
  "enum" @context
  name: (identifier) @name) @item

(record_declaration
  (modifiers)? @context
  "record" @context
  name: (identifier) @name) @item

(annotation_type_declaration
  (modifiers)? @context
  name: (identifier) @name) @item

(method_declaration
  (modifiers)? @context
  type: (_) @context
  name: (identifier) @name) @item

(constructor_declaration
  (modifiers)? @context
  name: (identifier) @name) @item

(field_declaration
  (modifiers)? @context
  type: (_) @context
  declarator: (variable_declarator
    name: (identifier) @name)) @item
