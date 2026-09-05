(comments) @comment

[
  (string_single_quoted)
  (string_double_quoted)
  (string_q_quoted)
  (string_qq_quoted)
] @string

(function_definition
  name: (_) @function)

(package_statement
  (package_name) @type)

[
  (scalar_variable)
  (array_variable)
  (hash_variable)
  (package_variable)
] @variable
