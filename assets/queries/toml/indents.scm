; TOML indent query.

[
  (table)
  (table_array_element)
  (inline_table)
  (array)
] @indent.begin

[
  "}"
  "]"
] @indent.end

[
  "{"
  "}"
  "["
  "]"
] @indent.branch

[
  (comment)
  (string)
] @indent.ignore
