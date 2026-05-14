; Python indent query. `:` at the end of a header line is the
; universal indent trigger here — the trailing-bracket fallback
; in the editor doesn't catch it, so we lean on tree-sitter to
; pick up `def`, `class`, `if`, `for`, `while`, `try`, etc. via
; the `block` body and to handle multi-line `()` / `[]` / `{}`.

[
  (block)
  (parameters)
  (lambda_parameters)
  (argument_list)
  (list)
  (tuple)
  (set)
  (dictionary)
  (parenthesized_expression)
  (list_comprehension)
  (set_comprehension)
  (dictionary_comprehension)
  (generator_expression)
] @indent.begin

[
  "]"
  ")"
  "}"
] @indent.end

[
  "("
  ")"
  "["
  "]"
  "{"
  "}"
] @indent.branch

[
  (string)
  (comment)
] @indent.ignore
