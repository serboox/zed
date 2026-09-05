(binary_operator
  lhs: (identifier) @name
  operator: ["<-" "=" "<<-"]
  rhs: (function_definition)) @item

(binary_operator
  lhs: (string) @name
  operator: ["<-" "="]
  rhs: (function_definition)) @item
