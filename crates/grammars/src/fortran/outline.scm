(program_statement
  "program" @context
  (name) @name) @item

(module_statement
  "module" @context
  (name) @name) @item

(submodule_statement
  (name) @name) @item

(subroutine_statement
  "subroutine" @context
  (name) @name) @item

(function_statement
  "function" @context
  (name) @name) @item

(derived_type_statement
  (type_name) @name) @item

(interface_statement
  "interface" @context
  (name)? @name) @item
