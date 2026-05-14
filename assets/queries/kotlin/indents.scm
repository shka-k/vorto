; Kotlin indent query.

[
  (class_body)
  (enum_class_body)
  (function_body)
  (lambda_literal)
  (anonymous_initializer)
  (control_structure_body)
  (when_expression)
  (value_arguments)
  (function_value_parameters)
  (collection_literal)
] @indent.begin

[
  "}"
  "]"
  ")"
  ">"
] @indent.end

[
  "{"
  "}"
  "("
  ")"
  "["
  "]"
] @indent.branch

[
  (line_comment)
  (multiline_comment)
  (string_literal)
] @indent.ignore
