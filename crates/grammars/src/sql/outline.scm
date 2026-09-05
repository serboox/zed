(create_table
  (keyword_create) @context
  (keyword_table) @context
  (object_reference
    name: (identifier) @name)) @item

(column_definition
  name: (identifier) @name) @item

(create_view
  (keyword_view) @context
  (object_reference
    name: (identifier) @name)) @item

(create_materialized_view
  (keyword_materialized) @context
  (keyword_view) @context
  (object_reference
    name: (identifier) @name)) @item

(create_index
  (keyword_index) @context
  column: [(identifier) (literal)] @name) @item

(create_function
  (keyword_function) @context
  (object_reference
    name: (identifier) @name)) @item

; The trigger's own name is the first object reference; the second is the table
; it fires on. Anchoring picks the first, and `IF NOT EXISTS` puts three keywords
; in between, so that spelling needs its own pattern.
(create_trigger
  (keyword_trigger) @context
  .
  (object_reference
    name: (identifier) @name)) @item

(create_trigger
  (keyword_trigger) @context
  (keyword_exists)
  .
  (object_reference
    name: (identifier) @name)) @item

(create_sequence
  (keyword_sequence) @context
  (object_reference
    name: (identifier) @name)) @item

(create_type
  (keyword_type) @context
  (object_reference
    name: (identifier) @name)) @item

(create_schema
  (keyword_schema) @context
  (identifier) @name) @item

(create_database
  (keyword_database) @context
  (identifier) @name) @item
