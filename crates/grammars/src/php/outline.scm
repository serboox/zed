(namespace_definition
  "namespace" @context
  name: (namespace_name) @name) @item

(class_declaration
  "class" @context
  name: (name) @name) @item

(interface_declaration
  "interface" @context
  name: (name) @name) @item

(trait_declaration
  "trait" @context
  name: (name) @name) @item

(enum_declaration
  "enum" @context
  name: (name) @name) @item

(function_definition
  "function" @context
  name: (name) @name) @item

(method_declaration
  "function" @context
  name: (name) @name) @item

(property_declaration
  (property_element
    (variable_name) @name)) @item

(const_declaration
  (const_element
    (name) @name)) @item
