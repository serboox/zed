#[cfg(test)]
mod tests {
    use gpui::{AppContext as _, TestAppContext};
    use unindent::Unindent as _;

    async fn outline_of(
        cx: &mut TestAppContext,
        name: &str,
        grammar: tree_sitter::Language,
        text: &str,
    ) -> Vec<(String, usize)> {
        let language = crate::language(name, grammar);
        let text = text.unindent();
        let buffer = cx.new(|cx| language::Buffer::local(text, cx).with_language(language, cx));
        let outline = buffer.read_with(cx, |buffer, _| buffer.snapshot().outline(None));
        outline
            .items
            .iter()
            .map(|item| (item.text.to_string(), item.depth))
            .collect()
    }

    fn named(items: &[(String, usize)]) -> Vec<&str> {
        items.iter().map(|(text, _)| text.as_str()).collect()
    }

    #[gpui::test]
    async fn java_reads_its_types_and_members(cx: &mut TestAppContext) {
        let items = outline_of(
            cx,
            "java",
            tree_sitter_java::LANGUAGE.into(),
            r#"
            package example;

            public interface Greeter {
                String greet(String name);
            }

            public class Loud implements Greeter {
                private final String suffix;

                public Loud(String suffix) {
                    this.suffix = suffix;
                }

                public String greet(String name) {
                    return name + suffix;
                }
            }
            "#,
        )
        .await;
        let names = named(&items);
        assert!(
            names.iter().any(|text| text.contains("Greeter")),
            "the interface is missing from {names:?}"
        );
        assert!(
            names.iter().any(|text| text.contains("Loud")),
            "the class is missing from {names:?}"
        );
        assert!(
            names.iter().any(|text| text.contains("greet")),
            "the method is missing from {names:?}"
        );
    }

    #[gpui::test]
    async fn csharp_reads_its_types_and_members(cx: &mut TestAppContext) {
        let items = outline_of(
            cx,
            "csharp",
            tree_sitter_c_sharp::LANGUAGE.into(),
            r#"
            namespace Example
            {
                public class Loud
                {
                    public string Suffix { get; set; }

                    public string Greet(string name)
                    {
                        return name + Suffix;
                    }
                }
            }
            "#,
        )
        .await;
        let names = named(&items);
        for wanted in ["Example", "Loud", "Greet", "Suffix"] {
            assert!(
                names.iter().any(|text| text.contains(wanted)),
                "{wanted} is missing from {names:?}"
            );
        }
    }

    #[gpui::test]
    async fn php_reads_its_types_and_functions(cx: &mut TestAppContext) {
        let items = outline_of(
            cx,
            "php",
            tree_sitter_php::LANGUAGE_PHP.into(),
            r#"
            <?php
            namespace Example;

            interface Greeter {
                public function greet(string $name): string;
            }

            class Loud implements Greeter {
                public function greet(string $name): string {
                    return $name;
                }
            }

            function shout(string $name): string {
                return $name;
            }
            "#,
        )
        .await;
        let names = named(&items);
        for wanted in ["Greeter", "Loud", "greet", "shout"] {
            assert!(
                names.iter().any(|text| text.contains(wanted)),
                "{wanted} is missing from {names:?}"
            );
        }
    }

    #[gpui::test]
    async fn ruby_reads_its_modules_classes_and_methods(cx: &mut TestAppContext) {
        let items = outline_of(
            cx,
            "ruby",
            tree_sitter_ruby::LANGUAGE.into(),
            r#"
            module Example
              class Loud
                def greet(name)
                  name
                end

                def self.build
                  new
                end
              end
            end
            "#,
        )
        .await;
        let names = named(&items);
        for wanted in ["Example", "Loud", "greet", "build"] {
            assert!(
                names.iter().any(|text| text.contains(wanted)),
                "{wanted} is missing from {names:?}"
            );
        }
    }

    #[gpui::test]
    async fn swift_reads_its_types_and_functions(cx: &mut TestAppContext) {
        let items = outline_of(
            cx,
            "swift",
            tree_sitter_swift::LANGUAGE.into(),
            r#"
            protocol Greeter {
                func greet(name: String) -> String
            }

            struct Loud: Greeter {
                func greet(name: String) -> String {
                    return name
                }
            }
            "#,
        )
        .await;
        let names = named(&items);
        for wanted in ["Greeter", "Loud", "greet"] {
            assert!(
                names.iter().any(|text| text.contains(wanted)),
                "{wanted} is missing from {names:?}"
            );
        }
    }

    #[gpui::test]
    async fn r_reads_the_functions_it_assigns(cx: &mut TestAppContext) {
        let items = outline_of(
            cx,
            "r",
            tree_sitter_r::LANGUAGE.into(),
            r#"
            greet <- function(name) {
              paste("hello", name)
            }

            shout = function(name) {
              toupper(name)
            }
            "#,
        )
        .await;
        let names = named(&items);
        for wanted in ["greet", "shout"] {
            assert!(
                names.iter().any(|text| text.contains(wanted)),
                "{wanted} is missing from {names:?}"
            );
        }
    }

    #[gpui::test]
    async fn perl_reads_its_packages_and_subs(cx: &mut TestAppContext) {
        let items = outline_of(
            cx,
            "perl",
            tree_sitter_perl::LANGUAGE.into(),
            r#"
            package Example::Loud;

            sub greet {
                my ($name) = @_;
                return $name;
            }

            1;
            "#,
        )
        .await;
        let names = named(&items);
        for wanted in ["Example::Loud", "greet"] {
            assert!(
                names.iter().any(|text| text.contains(wanted)),
                "{wanted} is missing from {names:?}"
            );
        }
    }

    #[gpui::test]
    async fn fortran_reads_its_program_units(cx: &mut TestAppContext) {
        let items = outline_of(
            cx,
            "fortran",
            tree_sitter_fortran::LANGUAGE.into(),
            r#"
            module greeting
            contains
              subroutine greet(name)
                character(len=*) :: name
              end subroutine greet

              function shout(name) result(loud)
                character(len=*) :: name
                character(len=32) :: loud
              end function shout
            end module greeting
            "#,
        )
        .await;
        let names = named(&items);
        for wanted in ["greeting", "greet", "shout"] {
            assert!(
                names.iter().any(|text| text.contains(wanted)),
                "{wanted} is missing from {names:?}"
            );
        }
    }

    #[gpui::test]
    async fn pascal_reads_its_units_and_procedures(cx: &mut TestAppContext) {
        let items = outline_of(
            cx,
            "pascal",
            tree_sitter_pascal::LANGUAGE.into(),
            r#"
            unit Greeting;

            interface

            procedure Greet(const Name: string);

            implementation

            procedure Greet(const Name: string);
            begin
            end;

            end.
            "#,
        )
        .await;
        let names = named(&items);
        for wanted in ["Greeting", "Greet"] {
            assert!(
                names.iter().any(|text| text.contains(wanted)),
                "{wanted} is missing from {names:?}"
            );
        }
    }

    #[gpui::test]
    async fn assembly_reads_its_labels(cx: &mut TestAppContext) {
        let items = outline_of(
            cx,
            "asm",
            tree_sitter_asm::LANGUAGE.into(),
            r#"
            _start:
                mov rax, 1
                ret

            greet:
                ret
            "#,
        )
        .await;
        let names = named(&items);
        for wanted in ["_start", "greet"] {
            assert!(
                names.iter().any(|text| text.contains(wanted)),
                "{wanted} is missing from {names:?}"
            );
        }
    }

    #[gpui::test]
    async fn visual_basic_reads_its_subs_and_functions(cx: &mut TestAppContext) {
        let items = outline_of(
            cx,
            "vb6",
            tree_sitter_vb6::language(),
            r#"
            Public Sub Greet(name As String)
                MsgBox name
            End Sub

            Public Function Shout(name As String) As String
                Shout = UCase(name)
            End Function
            "#,
        )
        .await;
        let names = named(&items);
        for wanted in ["Greet", "Shout"] {
            assert!(
                names.iter().any(|text| text.contains(wanted)),
                "{wanted} is missing from {names:?}"
            );
        }
    }
}
