use std::ops::Range;

use jsonschema::Validator;
use schema_documents::{Complaint, unreadable_is_one_complaint};

use crate::reading::read;

/// The one thing wrong with this text that no schema is needed to see,
/// placed on the bytes the reading stopped at.
///
/// A text holding no document at all yields nothing: a file the reader has
/// just created, or one holding only comments, has nothing wrong with it yet,
/// and an error on it would be an error on every new file. A text that will
/// not read yields the one fault that stopped it and not a complaint per
/// line -- past an unbalanced brace everything further is a consequence of
/// the first mistake rather than a second one.
pub fn what_the_reader_said(text: &str) -> Vec<(Range<usize>, Complaint)> {
    match read(text) {
        Some(Err(unreadable)) => vec![unreadable_is_one_complaint(unreadable)],
        Some(Ok(_)) | None => Vec::new(),
    }
}

/// Everything the schema has to say about this text, placed on the bytes it
/// is about.
///
/// A text holding no document at all yields nothing, and so does one that
/// will not read: [`what_the_reader_said`] has already said where the reading
/// stopped, and past an unbalanced brace the value is not the reader's value
/// any more, so everything the schema would say about it is about a document
/// nobody wrote.
pub fn what_the_schema_said(text: &str, validator: &Validator) -> Vec<(Range<usize>, Complaint)> {
    let Some(Ok(document)) = read(text) else {
        return Vec::new();
    };
    schema_documents::what_the_schema_said(text, &document, validator)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use schema_documents::validator_for;
    use serde_json::{Value, json};

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
    /// where the reader is typing is the whole of what is useful, and it
    /// comes from the reader rather than from the schema.
    #[test]
    fn a_document_that_will_not_read_is_one_fault_and_not_a_pile_of_schema_complaints() {
        let text = r#"{"name": "zed", "tab_size": }"#;
        assert!(checked(text).is_empty(), "{:?}", checked(text));

        let mut said = what_the_reader_said(text);
        assert_eq!(said.len(), 1, "{said:?}");
        let (_, complaint) = said.remove(0);
        assert!(complaint.is_a_fault, "and it is the kind that stops work");
        assert_eq!(complaint.message, "expected a value");
    }

    /// The same fault, found without any schema at all. This is what nearly
    /// every JSON file in a project gets, because nearly none of them are
    /// covered by a schema.
    #[test]
    fn a_document_that_will_not_read_needs_no_schema_to_say_so() {
        let mut said = what_the_reader_said("{\n  \"a\": ,\n}\n");
        assert_eq!(said.len(), 1, "{said:?}");
        let (range, complaint) = said.remove(0);
        assert_eq!(range, 9..10, "the `,` the value should have been");
        assert!(complaint.is_a_fault);
    }

    #[test]
    fn a_document_that_reads_has_nothing_the_reader_objects_to() {
        for text in ["", "  \n ", "// still thinking\n", r#"{"name": "zed"}"#] {
            assert!(what_the_reader_said(text).is_empty(), "{text:?}");
        }
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

    /// Which references go unchecked, and which do not. Only a reference the
    /// schema cannot resolve from its own text reaches the retriever that
    /// answers with the schema accepting anything -- a reference into the
    /// document's own definitions is resolved and applied like any other
    /// keyword. Reading the limit as "no `$ref` is checked" understates what
    /// this does; reading it as "every `$ref` is checked" overstates it.
    #[test]
    fn a_reference_inside_the_schema_itself_is_resolved_and_applied() {
        let schema = json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "type": "object",
            "properties": { "name": { "$ref": "#/$defs/short_string" } },
            "$defs": { "short_string": { "type": "string", "maxLength": 4 } },
        });
        let validator = validator_for(&schema).expect("the schema builds");
        assert!(what_the_schema_said(r#"{"name": "zed"}"#, &validator).is_empty());
        assert_eq!(
            what_the_schema_said(r#"{"name": "zed the editor"}"#, &validator).len(),
            1,
            "the referred-to section is applied, so its `maxLength` is too"
        );
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
