; HTML indent query. Multi-line elements/scripts/styles span from
; the opening tag's row to the closing tag's row; auto-indent fires
; on the opener row when those rows differ.

[
  (element)
  (script_element)
  (style_element)
] @indent.begin

[
  (comment)
] @indent.ignore
