; JavaScript indent query.

[
  (statement_block)
  (class_body)
  (object)
  (array)
  (object_pattern)
  (array_pattern)
  (formal_parameters)
  (arguments)
  (switch_body)
  (named_imports)
  (parenthesized_expression)
] @indent.begin

[
  "}"
  "]"
  ")"
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
  (comment)
  (string)
  (template_string)
  (regex)
] @indent.ignore
