; TSX indent query. Same as TypeScript plus JSX element bodies.

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
  (object_type)
  (interface_body)
  (enum_body)
  (type_parameters)
  (type_arguments)
  (jsx_element)
  (jsx_opening_element)
  (jsx_self_closing_element)
  (jsx_expression)
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
  (comment)
  (string)
  (template_string)
  (regex)
] @indent.ignore
