; C indent query.

[
  (compound_statement)
  (field_declaration_list)
  (enumerator_list)
  (initializer_list)
  (argument_list)
  (parameter_list)
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
  (string_literal)
] @indent.ignore
