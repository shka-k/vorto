; C++ indent query. Extends C with templates, lambdas, and
; constructor field initialisers.

[
  (compound_statement)
  (field_declaration_list)
  (enumerator_list)
  (initializer_list)
  (argument_list)
  (parameter_list)
  (template_parameter_list)
  (template_argument_list)
  (field_initializer_list)
  (lambda_capture_specifier)
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
  (string_literal)
  (raw_string_literal)
] @indent.ignore
