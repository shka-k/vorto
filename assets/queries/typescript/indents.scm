; TypeScript indent query. Inherits everything JavaScript needs
; and adds the type-system containers (interface/enum/object type,
; generic parameter / argument lists).

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
