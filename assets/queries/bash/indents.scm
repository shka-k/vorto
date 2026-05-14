; Bash indent query. `do`/`done`, `then`/`fi`, `case`/`esac` and
; `function() { ... }` bodies all live in nodes whose start row is
; the keyword/opener line.

[
  (function_definition)
  (do_group)
  (if_statement)
  (elif_clause)
  (else_clause)
  (case_statement)
  (case_item)
  (compound_statement)
  (subshell)
] @indent.begin

[
  "}"
  "fi"
  "done"
  "esac"
] @indent.end

[
  "{"
  "}"
  "("
  ")"
] @indent.branch

[
  (comment)
  (string)
  (raw_string)
  (heredoc_body)
] @indent.ignore
