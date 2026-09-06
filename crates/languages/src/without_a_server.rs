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
        cx.executor().run_until_parked();
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

    #[gpui::test]
    async fn cobol_reads_its_program_paragraphs_and_data(cx: &mut TestAppContext) {
        // Written in fixed format with a sequence number in columns 1-6: the
        // grammar only parses fixed format, code has to start in column 8, and
        // the sequence numbers are what survives `unindent` stripping the
        // literal's own indentation.
        let items = outline_of(
            cx,
            "cobol",
            arborium_cobol::language().into(),
            r#"
            000100 IDENTIFICATION DIVISION.
            000200 PROGRAM-ID. GREETER.
            000300
            000400 DATA DIVISION.
            000500 WORKING-STORAGE SECTION.
            000600 01 WS-GREETING          PIC X(20) VALUE "Hello, ".
            000700 01 WS-NAME              PIC X(30) VALUE SPACES.
            000800
            000900 PROCEDURE DIVISION.
            001000 MAIN-SECTION SECTION.
            001100 GREET-THE-WORLD.
            001200     MOVE "World" TO WS-NAME
            001300     DISPLAY WS-GREETING WS-NAME
            001400     STOP RUN.
            "#,
        )
        .await;
        let names = named(&items);
        for wanted in [
            "GREETER",
            "MAIN-SECTION",
            "GREET-THE-WORLD",
            "WS-GREETING",
            "WS-NAME",
        ] {
            assert!(
                names.iter().any(|text| text.contains(wanted)),
                "{wanted} is missing from {names:?}"
            );
        }
    }

    #[gpui::test]
    async fn sql_reads_the_objects_it_defines(cx: &mut TestAppContext) {
        let items = outline_of(
            cx,
            "sql",
            tree_sitter_sequel::LANGUAGE.into(),
            r#"
            CREATE SCHEMA shop;

            CREATE TABLE shop.customers (
                customer_id INT PRIMARY KEY,
                display_name TEXT NOT NULL
            );

            CREATE INDEX customers_by_name ON shop.customers (display_name);

            CREATE VIEW loud_customers AS
                SELECT display_name FROM shop.customers;

            CREATE FUNCTION greet(who TEXT) RETURNS TEXT AS $$
                SELECT 'hello ' || who;
            $$ LANGUAGE sql;
            "#,
        )
        .await;
        let names = named(&items);
        for wanted in [
            "shop",
            "customers",
            "customer_id",
            "display_name",
            "customers_by_name",
            "loud_customers",
            "greet",
        ] {
            assert!(
                names.iter().any(|text| text.contains(wanted)),
                "{wanted} is missing from {names:?}"
            );
        }
        let columns_sit_under_their_table = items
            .iter()
            .filter(|(text, depth)| text.contains("customer_id") && *depth > 0)
            .count();
        assert_eq!(
            columns_sit_under_their_table, 1,
            "a column has to nest under its table in {items:?}"
        );
    }

    #[gpui::test]
    async fn bash_reads_its_functions_and_top_level_names(cx: &mut TestAppContext) {
        let items = outline_of(
            cx,
            "bash",
            tree_sitter_bash::LANGUAGE.into(),
            r#"
            GREETING="hello"
            export SHOUTED="HELLO"

            greet() {
                local who="$1"
                echo "$GREETING $who"
            }

            function shout {
                greet "$1" | tr a-z A-Z
            }
            "#,
        )
        .await;
        let names = named(&items);
        for wanted in ["GREETING", "SHOUTED", "greet", "shout"] {
            assert!(
                names.iter().any(|text| text.contains(wanted)),
                "{wanted} is missing from {names:?}"
            );
        }
        assert!(
            !names.iter().any(|text| text.contains("who")),
            "a local is not a name the project declares, but {names:?} has one"
        );
    }

    #[gpui::test]
    async fn sql_names_a_trigger_once_and_after_itself(cx: &mut TestAppContext) {
        let items = outline_of(
            cx,
            "sql",
            tree_sitter_sequel::LANGUAGE.into(),
            r#"
            CREATE TABLE shop.customers (
                customer_id INT
            );

            CREATE TRIGGER stamp_customers
                BEFORE INSERT ON shop.customers
                FOR EACH ROW EXECUTE FUNCTION stamp();

            CREATE TRIGGER IF NOT EXISTS audit_customers
                AFTER UPDATE ON shop.customers
                FOR EACH ROW EXECUTE FUNCTION audit();
            "#,
        )
        .await;
        let names = named(&items);
        for wanted in ["stamp_customers", "audit_customers"] {
            assert_eq!(
                names.iter().filter(|text| text.contains(wanted)).count(),
                1,
                "{wanted} has to be named once in {names:?}"
            );
        }
        assert_eq!(
            names
                .iter()
                .filter(|text| text.contains("customers"))
                .count(),
            3,
            "the table is named once and each trigger once, in {names:?}"
        );
        assert!(
            !names.iter().any(|text| text.contains("shop")),
            "a schema qualifier is not a definition, but {names:?} has one"
        );
    }

    #[gpui::test]
    async fn proto_reads_its_messages_services_and_fields(cx: &mut TestAppContext) {
        let items = outline_of(
            cx,
            "proto",
            tree_sitter_proto::LANGUAGE.into(),
            r#"
            syntax = "proto3";

            package shop.v1;

            enum Currency {
              CURRENCY_UNSPECIFIED = 0;
              CURRENCY_EUR = 1;
            }

            message Order {
              message Line {
                string sku = 1;
                int32 quantity = 2;
              }

              string order_id = 1;
              repeated Line lines = 2;
              map<string, string> labels = 3;

              oneof payment {
                string card_token = 4;
                string invoice_reference = 5;
              }
            }

            service Orders {
              rpc PlaceOrder(Order) returns (Order);
              rpc ListOrders(Order) returns (stream Order) {
                option deprecated = true;
              }
            }
            "#,
        )
        .await;
        let names = named(&items);
        for wanted in [
            "Currency",
            "CURRENCY_EUR",
            "Order",
            "Line",
            "sku",
            "order_id",
            "lines",
            "labels",
            "payment",
            "card_token",
            "Orders",
            "PlaceOrder",
            "ListOrders",
        ] {
            assert!(
                names.iter().any(|text| text.contains(wanted)),
                "{wanted} is missing from {names:?}"
            );
        }

        let depth_of = |wanted: &str| {
            items
                .iter()
                .find(|(text, _)| text.contains(wanted))
                .map(|(_, depth)| *depth)
        };
        let message = depth_of("Order").expect("the message is in the outline");
        let nested = depth_of("Line").expect("the nested message is in the outline");
        let field = depth_of("sku").expect("the nested message's field is in the outline");
        assert!(
            nested > message && field > nested,
            "a message inside a message, and its field inside that, in {items:?}"
        );

        let method = depth_of("PlaceOrder").expect("the method is in the outline");
        let service = depth_of("Orders").expect("the service is in the outline");
        assert!(
            method > service,
            "an rpc has to sit under its service, in {items:?}"
        );

        assert!(
            !names.iter().any(|text| text.contains("deprecated")),
            "an option set on an rpc is not a symbol, but {names:?} has one"
        );
    }

    #[gpui::test]
    async fn xml_reads_its_elements_and_keeps_them_nested(cx: &mut TestAppContext) {
        let items = outline_of(
            cx,
            "xml",
            tree_sitter_xml::LANGUAGE_XML.into(),
            r#"
            <?xml version="1.0" encoding="UTF-8"?>
            <!-- a build file, near enough -->
            <project xmlns="http://maven.apache.org/POM/4.0.0">
                <groupId>dev.example</groupId>
                <dependencies>
                    <dependency scope="test">
                        <artifactId>junit</artifactId>
                    </dependency>
                    <dependency scope="runtime"/>
                </dependencies>
            </project>
            "#,
        )
        .await;
        let names = named(&items);
        for wanted in ["project", "groupId", "dependencies", "artifactId"] {
            assert!(
                names.iter().any(|text| text.contains(wanted)),
                "{wanted} is missing from {names:?}"
            );
        }
        assert_eq!(
            names
                .iter()
                .filter(|text| text.contains("dependency"))
                .count(),
            2,
            "the paired element and the empty one are both elements, in {names:?}"
        );
        assert!(
            !names.iter().any(|text| text.contains("scope")),
            "an attribute is not an element, but {names:?} has one"
        );

        let depth_of = |wanted: &str| {
            items
                .iter()
                .find(|(text, _)| text.contains(wanted))
                .map(|(_, depth)| *depth)
        };
        let root = depth_of("project").expect("the root element is in the outline");
        let nested = depth_of("artifactId").expect("the nested element is in the outline");
        assert!(
            nested > root,
            "an element inside three others has to sit below them, but {items:?}"
        );
    }
}
