; Python indent query. `:` at the end of a header line is the universal
; indent trigger here, but the highlighter's auto-indent check fires
; on @indent.begin nodes whose *start row* equals the cursor row — and
; tree-sitter-python's `block` node starts on the first body line, not
; on the header. So `(block) @indent.begin` never matches the `def`/
; `if`/... row and Python lines after `:` don't auto-indent. Capture
; the compound-statement nodes themselves, which do start on the
; header row, plus the orphan clauses (`elif:` / `else:` / `except:` /
; `finally:` / `case:`) that head their own row.
[
  (function_definition)
  (class_definition)
  (if_statement)
  (elif_clause)
  (else_clause)
  (for_statement)
  (while_statement)
  (try_statement)
  (except_clause)
  (finally_clause)
  (with_statement)
  (match_statement)
  (case_clause)
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
