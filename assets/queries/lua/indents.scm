; Lua indent query. `do`/`end`, `then`/`end`, `function`/`end`,
; `repeat`/`until` and `{ ... }` table constructors.

[
  (do_statement)
  (while_statement)
  (for_statement)
  (repeat_statement)
  (if_statement)
  (elseif_statement)
  (else_statement)
  (function_definition)
  (function_declaration)
  (table_constructor)
  (arguments)
  (parameters)
] @indent.begin

[
  "}"
  ")"
  "end"
  "until"
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
] @indent.ignore
