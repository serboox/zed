use std::error::Error;
use std::ops::Range;

use anyhow::{Context as _, Result};
use jsonschema::error::ValidationErrorKind;
use jsonschema::{Retrieve, Uri, Validator};
use serde_json::Value;

use crate::{Document, Unreadable, pointer_to};

/// What these diagnostics are attributed to, shown beside each one. A JSON
/// Schema is what says them, whichever of the two languages the reader wrote.
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

/// Everything the schema has to say about one document, placed on the bytes
/// it is about.
pub fn what_the_schema_said(
    text: &str,
    document: &Document,
    validator: &Validator,
) -> Vec<(Range<usize>, Complaint)> {
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
                        .or_else(|| document.opening_of(text, at));
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
            // first character of the value that should have held it does.
            ValidationErrorKind::Required { .. } => {
                said.extend(document.opening_of(text, at).map(|range| {
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

pub fn unreadable_is_one_complaint(unreadable: Unreadable) -> (Range<usize>, Complaint) {
    (
        unreadable.range,
        Complaint {
            message: unreadable.message,
            is_a_fault: true,
        },
    )
}
