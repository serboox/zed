use std::error::Error;
use std::ops::Range;

use anyhow::{Context as _, Result};
use jsonschema::error::ValidationErrorKind;
use jsonschema::{Retrieve, Uri, Validator};
use serde_json::Value;

use crate::reading::{Document, Unreadable, pointer_to};

/// What these diagnostics are attributed to, shown beside each one.
pub const SOURCE: &str = "json-schema";

/// Builds a validator that will never reach the network.
///
/// The schemas this editor serves are not all self-contained: the bundled
/// `package.json` one refers out to seven schemas on schemastore.org. The
/// validator's own retriever would fetch those over blocking HTTP, on
/// whichever thread happened to be compiling the schema -- so it is replaced
/// by one that answers every request with the schema that accepts anything.
/// The sections behind those references go unchecked, which is the honest
/// outcome; the rest of the file is still checked, which fetching would have
/// bought at the price of an editor that stalls on a captive portal.
pub fn validator_for(schema: &Value) -> Result<Validator> {
    jsonschema::options()
        .with_retriever(NothingIsFetched)
        .build(schema)
        .context("building a validator for this schema")
}

struct NothingIsFetched;

impl Retrieve for NothingIsFetched {
    fn retrieve(&self, _: &Uri<String>) -> Result<Value, Box<dyn Error + Send + Sync>> {
        Ok(Value::Object(serde_json::Map::new()))
    }
}

/// Everything the schema has to say about this text, placed on the bytes it
/// is about.
///
/// A text holding no document at all yields nothing: a file the reader has
/// just created has nothing to check, and an error on it would be an error on
/// every new file. A text that will not read yields the one fault that stopped
/// it, not a schema complaint per line -- past an unbalanced brace the value
/// is not the reader's value any more, and everything the schema would say
/// about it is about a document nobody wrote.
pub fn what_the_schema_said(text: &str, validator: &Validator) -> Vec<(Range<usize>, Complaint)> {
    let Some(read) = Document::read(text) else {
        return Vec::new();
    };
    let document = match read {
        Ok(document) => document,
        Err(unreadable) => return vec![unreadable_is_one_complaint(unreadable)],
    };
    let mut said = Vec::new();
    for error in validator.iter_errors(&document.value) {
        let at = error.instance_path().as_str();
        match error.kind() {
            // The validator points these at the object, and names the
            // properties separately. The reader wants the name they wrote, so
            // each unexpected one becomes its own complaint on its own name.
            ValidationErrorKind::AdditionalProperties { unexpected }
            | ValidationErrorKind::UnevaluatedProperties { unexpected } => {
                for name in unexpected {
                    let inner = pointer_to(at, name);
                    let range = document
                        .name_at(&inner)
                        .or_else(|| document.value_at(&inner))
                        .or_else(|| document.opening_of(at));
                    said.extend(range.map(|range| {
                        (
                            range,
                            Complaint {
                                message: format!("`{name}` is not a property this schema allows"),
                                is_a_fault: false,
                            },
                        )
                    }));
                }
            }
            // The property is missing, so it has no bytes of its own. The
            // opening brace of the value that should have held it does.
            ValidationErrorKind::Required { .. } => {
                said.extend(document.opening_of(at).map(|range| {
                    (
                        range,
                        Complaint {
                            message: error.to_string(),
                            is_a_fault: false,
                        },
                    )
                }));
            }
            _ => {
                said.extend(document.value_at(at).map(|range| {
                    (
                        range,
                        Complaint {
                            message: error.to_string(),
                            is_a_fault: false,
                        },
                    )
                }));
            }
        }
    }
    said
}

/// One thing the checker has to say, and how much weight to give it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Complaint {
    pub message: String,
    /// Whether nothing further can be said about the file. Only a document
    /// that will not read is one. A schema violation is not: these schemas
    /// are checked without fetching what they refer out to, and several of
    /// them describe a format more narrowly than the tools that read it do,
    /// so a violation is a strong hint rather than a verdict -- and painting
    /// a working file red over a hint is worse than saying it more quietly.
    pub is_a_fault: bool,
}

fn unreadable_is_one_complaint(unreadable: Unreadable) -> (Range<usize>, Complaint) {
    (
        unreadable.range,
        Complaint {
            message: unreadable.message,
            is_a_fault: true,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use serde_json::json;

    fn a_schema() -> Value {
        json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "type": "object",
            "additionalProperties": false,
            "required": ["name"],
            "properties": {
                "name": { "type": "string" },
                "tab_size": { "type": "integer" },
                "theme": { "enum": ["One Dark", "One Light"] },
                "tags": { "type": "array", "items": { "type": "string" } },
            },
        })
    }

    fn checked(text: &str) -> Vec<(Range<usize>, Complaint)> {
        let validator = validator_for(&a_schema()).expect("the schema builds");
        what_the_schema_said(text, &validator)
    }

    /// Where the complaint lands is the whole point: a message with the wrong
    /// range is a squiggle under working code. Every case below asserts the
    /// bytes, not only the count.
    fn only_complaint(text: &str) -> (String, String) {
        let mut said = checked(text);
        assert_eq!(said.len(), 1, "{said:?}");
        let (range, complaint) = said.remove(0);
        (text[range].to_string(), complaint.message)
    }

    #[test]
    fn a_document_that_satisfies_its_schema_says_nothing() {
        let said = checked(r#"{"name": "zed", "tab_size": 2, "theme": "One Dark"}"#);
        assert!(said.is_empty(), "{said:?}");
    }

    #[test]
    fn a_value_of_the_wrong_type_is_marked_on_that_value_and_nothing_else() {
        let (marked, message) = only_complaint(r#"{"name": "zed", "tab_size": "two"}"#);
        assert_eq!(marked, r#""two""#, "the value, not the property");
        assert!(message.contains("integer"), "{message}");
    }

    /// The value is nested, so the pointer the validator answers with is
    /// nested too. A reader that only handled the top level would put this on
    /// the array instead of on the item inside it.
    #[test]
    fn a_wrong_value_inside_an_array_is_marked_on_the_item_and_not_the_array() {
        let (marked, _) = only_complaint(r#"{"name": "zed", "tags": ["a", 2, "c"]}"#);
        assert_eq!(marked, "2");
    }

    #[test]
    fn a_missing_required_property_is_marked_on_the_opening_brace_of_its_object() {
        let (marked, message) = only_complaint(r#"{"tab_size": 2}"#);
        assert_eq!(marked, "{", "not the whole object, which may be the file");
        assert!(message.contains("name"), "{message}");
    }

    /// The validator points this at the object and names the property
    /// separately, so placing it takes the extra step. Getting that step
    /// wrong puts the mark on the whole file instead of on the typo.
    #[test]
    fn a_property_the_schema_forbids_is_marked_on_the_name_the_reader_wrote() {
        let (marked, message) = only_complaint(r#"{"name": "zed", "tabsize": 2}"#);
        assert_eq!(marked, r#""tabsize""#);
        assert_eq!(message, "`tabsize` is not a property this schema allows");
    }

    #[test]
    fn two_forbidden_properties_are_two_complaints_each_on_its_own_name() {
        let said = checked(r#"{"name": "zed", "tabsize": 2, "colour": "red"}"#);
        assert_eq!(said.len(), 2, "{said:?}");
    }

    #[test]
    fn a_value_outside_the_schemas_list_is_marked_on_that_value() {
        let (marked, message) = only_complaint(r#"{"name": "zed", "theme": "Solarized"}"#);
        assert_eq!(marked, r#""Solarized""#);
        assert!(message.contains("One Dark"), "{message}");
    }

    /// A file mid-edit is unreadable most of the time, and the schema has an
    /// opinion about every part of the half-document that results. One fault
    /// where the reader is typing is the whole of what is useful.
    #[test]
    fn a_document_that_will_not_read_is_one_fault_and_not_a_pile_of_schema_complaints() {
        let mut said = checked(r#"{"name": "zed", "tab_size": }"#);
        assert_eq!(said.len(), 1, "{said:?}");
        let (_, complaint) = said.remove(0);
        assert!(complaint.is_a_fault, "and it is the kind that stops work");
        assert_eq!(complaint.message, "expected a value");
    }

    /// The schema is not satisfied by any of this, and none of it is said:
    /// there is no document here to have an opinion about yet.
    #[test]
    fn a_text_holding_no_document_says_nothing_rather_than_that_it_is_wrong() {
        for text in ["", "  \n ", "// still thinking\n"] {
            assert!(checked(text).is_empty(), "{text:?}");
        }
    }

    /// Comments and trailing commas are not the schema's business. They do
    /// not change the value, so a commented file and the same file without
    /// its comments must be judged identically -- and the surviving complaint
    /// must still land on the right bytes of the commented text, not of some
    /// stripped copy of it.
    #[test]
    fn comments_and_trailing_commas_are_not_schema_violations() {
        let commented = "{\n  // the name\n  \"name\": \"zed\",\n  \"tab_size\": 2,\n}\n";
        assert!(checked(commented).is_empty(), "{:?}", checked(commented));

        let wrong = "{\n  // the name\n  \"name\": \"zed\",\n  \"tab_size\": \"two\",\n}\n";
        let (marked, _) = only_complaint(wrong);
        assert_eq!(marked, r#""two""#);
    }

    /// A schema that refers out to the network is built without going there.
    /// The referred-to section goes unchecked; everything else still does not.
    #[test]
    fn a_reference_to_a_schema_on_the_network_is_not_fetched_and_accepts_anything() {
        let schema = json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "type": "object",
            "properties": {
                "prettier": { "$ref": "https://www.schemastore.org/prettierrc.json" },
                "tab_size": { "type": "integer" },
            },
        });
        let validator = validator_for(&schema).expect("built without fetching anything");
        let said = what_the_schema_said(r#"{"prettier": {"anything": [1, 2]}}"#, &validator);
        assert!(said.is_empty(), "{said:?}");

        let said = what_the_schema_said(r#"{"tab_size": "two"}"#, &validator);
        assert_eq!(said.len(), 1, "the rest of the schema still applies");
    }

    /// The editor's own schemas are written for an older draft than the
    /// current one, and are the schemas this will spend nearly all of its
    /// time on. A validator that could not build them would be silent
    /// everywhere that matters.
    #[test]
    fn an_older_draft_is_built_and_applied_like_any_other() {
        for meta in [
            "http://json-schema.org/draft-04/schema#",
            "http://json-schema.org/draft-07/schema#",
            "https://json-schema.org/draft/2019-09/schema",
        ] {
            let schema = json!({
                "$schema": meta,
                "type": "object",
                "properties": { "tab_size": { "type": "integer" } },
            });
            let validator = validator_for(&schema).unwrap_or_else(|_| panic!("{meta} builds"));
            let said = what_the_schema_said(r#"{"tab_size": "two"}"#, &validator);
            assert_eq!(said.len(), 1, "{meta}");
        }
    }
}
