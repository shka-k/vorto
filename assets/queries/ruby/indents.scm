; Ruby indent query. Ruby has no opening-bracket trigger for
; `def`/`class`/`module`/`do`/`if`/etc., so the editor's
; trailing-bracket fallback can't help — every block-opening
; node has to come from here.

[
  (method)
  (singleton_method)
  (module)
  (class)
  (singleton_class)
  (do_block)
  (block)
  (lambda)
  (if)
  (unless)
  (while)
  (until)
  (for)
  (case)
  (when)
  (begin)
  (rescue)
  (ensure)
  (method_parameters)
  (lambda_parameters)
  (block_parameters)
  (argument_list)
  (array)
  (hash)
] @indent.begin

[
  "end"
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
  (string)
  (heredoc_body)
] @indent.ignore
