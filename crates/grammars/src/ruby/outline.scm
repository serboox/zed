(module
  "module" @context
  name: (_) @name) @item

(class
  "class" @context
  name: (_) @name) @item

(singleton_class
  "class" @context
  value: (_) @name) @item

(method
  "def" @context
  name: (_) @name) @item

(singleton_method
  "def" @context
  object: (_) @context
  name: (_) @name) @item

(assignment
  left: (constant) @name) @item
