(object_reference
  name: (identifier) @type)

(invocation
  (object_reference
    name: (identifier) @function.call))

[
  (keyword_gist)
  (keyword_btree)
  (keyword_hash)
  (keyword_spgist)
  (keyword_gin)
  (keyword_brin)
  (keyword_array)
  (keyword_object_id)
] @function.call

(relation
  alias: (identifier) @variable)

(field
  name: (identifier) @field)

(term
  alias: (identifier) @variable)

((term
   value: (cast
    name: (keyword_cast) @function.call
    parameter: [(literal)]?)))

(literal) @string
(comment) @comment
(marginalia) @comment

((literal) @number
   (#match? @number "^[-+]?%d+$"))

((literal) @float
  (#match? @float "^[-+]?%d*\.%d*$"))

(parameter) @variable.special

[
 (keyword_true)
 (keyword_false)
] @boolean

[
 (keyword_asc)
 (keyword_desc)
 (keyword_terminated)
 (keyword_escaped)
 (keyword_unsigned)
 (keyword_nulls)
 (keyword_last)
 (keyword_delimited)
 (keyword_replication)
 (keyword_auto_increment)
 (keyword_default)
 (keyword_collate)
 (keyword_concurrently)
 (keyword_engine)
 (keyword_always)
 (keyword_generated)
 (keyword_preceding)
 (keyword_following)
 (keyword_first)
 (keyword_current_timestamp)
 (keyword_immutable)
 (keyword_atomic)
 (keyword_parallel)
 (keyword_leakproof)
 (keyword_safe)
 (keyword_cost)
 (keyword_strict)
] @attribute

[
 (keyword_materialized)
 (keyword_recursive)
 (keyword_temp)
 (keyword_temporary)
 (keyword_unlogged)
 (keyword_external)
 (keyword_volatile)
] @storageclass

[
 (keyword_case)
 (keyword_when)
 (keyword_then)
 (keyword_else)
] @keyword.control

[
  (keyword_select)
  (keyword_from)
  (keyword_where)
  (keyword_index)
  (keyword_join)
  (keyword_primary)
  (keyword_delete)
  (keyword_create)
  (keyword_show)
  (keyword_insert)
  (keyword_merge)
  (keyword_distinct)
  (keyword_replace)
  (keyword_update)
  (keyword_into)
  (keyword_values)
  (keyword_value)
  (keyword_set)
  (keyword_left)
  (keyword_right)
  (keyword_outer)
  (keyword_inner)
  (keyword_full)
  (keyword_order)
  (keyword_partition)
  (keyword_group)
  (keyword_with)
  (keyword_without)
  (keyword_as)
  (keyword_having)
  (keyword_limit)
  (keyword_offset)
  (keyword_table)
  (keyword_tables)
  (keyword_key)
  (keyword_references)
  (keyword_foreign)
  (keyword_constraint)
  (keyword_force)
  (keyword_use)
  (keyword_for)
  (keyword_if)
  (keyword_exists)
  (keyword_column)
  (keyword_columns)
  (keyword_cross)
  (keyword_lateral)
  (keyword_natural)
  (keyword_alter)
  (keyword_drop)
  (keyword_add)
  (keyword_view)
  (keyword_end)
  (keyword_is)
  (keyword_using)
  (keyword_between)
  (keyword_window)
  (keyword_no)
  (keyword_data)
  (keyword_type)
  (keyword_rename)
  (keyword_to)
  (keyword_schema)
  (keyword_owner)
  (keyword_all)
  (keyword_any)
  (keyword_some)
  (keyword_returning)
  (keyword_begin)
  (keyword_commit)
  (keyword_rollback)
  (keyword_transaction)
  (keyword_only)
  (keyword_like)
  (keyword_similar)
  (keyword_over)
  (keyword_change)
  (keyword_modify)
  (keyword_after)
  (keyword_before)
  (keyword_range)
  (keyword_rows)
  (keyword_groups)
  (keyword_exclude)
  (keyword_current)
  (keyword_ties)
  (keyword_others)
  (keyword_format)
  (keyword_fields)
  (keyword_row)
  (keyword_sort)
  (keyword_comment)
  (keyword_truncate)
  (keyword_analyze)
  (keyword_explain)
  (keyword_verbose)
  (keyword_vacuum)
  (keyword_cache)
  (keyword_language)
  (keyword_conflict)
  (keyword_declare)
  (keyword_filter)
  (keyword_function)
  (keyword_return)
  (keyword_returns)
  (keyword_trigger)
  (keyword_role)
  (keyword_sequence)
  (keyword_database)
  (keyword_user)
  (keyword_extension)
  (keyword_procedure)
  (keyword_copy)
  (keyword_execute)
  (keyword_match)
] @keyword

[
 (keyword_restrict)
 (keyword_unique)
 (keyword_cascade)
 (keyword_check)
 (keyword_option)
 (keyword_local)
 (keyword_nowait)
] @type.qualifier

[
  (keyword_int)
  (keyword_null)
  (keyword_boolean)
  (keyword_binary)
  (keyword_varbinary)
  (keyword_bit)
  (keyword_inet)
  (keyword_character)
  (keyword_smallserial)
  (keyword_serial)
  (keyword_bigserial)
  (keyword_smallint)
  (keyword_mediumint)
  (keyword_bigint)
  (keyword_tinyint)
  (keyword_decimal)
  (keyword_float)
  (keyword_double)
  (keyword_numeric)
  (keyword_real)
  (double)
  (keyword_money)
  (keyword_char)
  (keyword_nchar)
  (keyword_varchar)
  (keyword_nvarchar)
  (keyword_varying)
  (keyword_text)
  (keyword_string)
  (keyword_uuid)
  (keyword_json)
  (keyword_jsonb)
  (keyword_xml)
  (keyword_bytea)
  (keyword_enum)
  (keyword_date)
  (keyword_datetime)
  (keyword_time)
  (keyword_timestamp)
  (keyword_timestamptz)
  (keyword_interval)
] @type.builtin

[
  (keyword_in)
  (keyword_and)
  (keyword_or)
  (keyword_not)
  (keyword_by)
  (keyword_on)
  (keyword_do)
  (keyword_union)
  (keyword_except)
  (keyword_intersect)
] @keyword.operator

[
  "+"
  "-"
  "*"
  "/"
  "%"
  "^"
  ":="
  "="
  "<"
  "<="
  "!="
  ">="
  ">"
  "<>"
  (op_other)
  (op_unary_other)
] @operator

[
  "("
  ")"
] @punctuation.bracket

[
  ";"
  ","
  "."
] @punctuation.delimiter
