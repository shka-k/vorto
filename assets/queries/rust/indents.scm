; Indent query for Rust. Captures any block-like node that opens an
; indented region; the auto-indent path on Enter / `o` adds one extra
; level whenever a new line is inserted on the same row that opens
; one of these nodes.

[
  (block)
  (declaration_list)
  (field_declaration_list)
  (enum_variant_list)
  (struct_pattern)
  (tuple_pattern)
  (parameters)
  (arguments)
  (array_expression)
  (tuple_expression)
  (match_block)
  (match_arm)
  (use_list)
  (token_tree)
  (token_tree_pattern)
  (closure_parameters)
  (where_clause)
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
  (line_comment)
  (block_comment)
  (string_literal)
  (raw_string_literal)
] @indent.ignore
