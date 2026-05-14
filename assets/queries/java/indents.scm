; Java indent query.

[
  (block)
  (class_body)
  (interface_body)
  (enum_body)
  (annotation_type_body)
  (constructor_body)
  (argument_list)
  (formal_parameters)
  (type_parameters)
  (type_arguments)
  (array_initializer)
  (switch_block)
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
  (block_comment)
  (string_literal)
] @indent.ignore
