# tree-sitter-gotmpl

The Go template grammar, taken from
<https://github.com/ngalaiko/tree-sitter-go-template> at commit
`aa71f63de226c5592dfbfc1f29949522d7c95fac`, under the MIT licence in
`LICENSE`.

Only the generated parser is here. The crate published alongside that grammar
pins `tree-sitter` 0.19, whose `Language` is a different type from the 0.26 this
workspace uses, so the parser is compiled here and declared against the
workspace's own `tree-sitter` instead. There is no external scanner, so
`src/parser.c` is the whole of it.

To take a newer version: copy `src/parser.c` and `src/tree_sitter/parser.h`
from that repository, and check that the `LANGUAGE_VERSION` at the top of
`parser.c` is one the workspace's `tree-sitter` still reads.
