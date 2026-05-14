; Go indent query. Grouped declarations (`const ( ... )`, `var
; ( ... )`, `import ( ... )`, `type ( ... )`) are captured via
; their `*_spec_list` containers.

[
  (block)
  (composite_literal)
  (field_declaration_list)
  (interface_type)
  (struct_type)
  (parameter_list)
  (argument_list)
  (select_statement)
  (expression_switch_statement)
  (type_switch_statement)
  (const_declaration)
  (var_declaration)
  (type_declaration)
  (import_declaration)
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
  (interpreted_string_literal)
  (raw_string_literal)
] @indent.ignore
