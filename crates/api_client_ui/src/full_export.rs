use std::io::{Cursor, Read, Write};

use anyhow::{Context as _, Result};
use api_client::{Collection, Environment, Folder, Request};
use uuid::Uuid;

use crate::export::{export_postman_collection, export_postman_environment};
use crate::import::{ImportedCollection, parse_postman_collection, parse_postman_environment};

/// One file inside the archive that failed to parse -- collected rather than
/// aborting the whole import, since Postman's own "Export Data" bundles can
/// contain collections from very different points in time and one malformed
/// file should not block importing the other 58.
pub struct FailedImport {
    pub file_name: String,
    pub error: String,
}

/// The result of importing a Postman "Full Data Export" ZIP: every
/// collection and environment that parsed successfully, plus a report of
/// whichever files did not.
#[derive(Default)]
pub struct FullExportImport {
    pub collections: Vec<ImportedCollection>,
    pub environments: Vec<Environment>,
    pub failed: Vec<FailedImport>,
}

/// Imports a Postman "Full Data Export" ZIP: a workspace-UUID-named
/// directory containing `collection/<uuid>.json` and
/// `environment/<uuid>.json` files, plus an `archive.json` manifest this
/// function does not need to consult -- every `.json` file directly under a
/// `collection/` or `environment/` path segment is read and parsed on its
/// own, so a missing or stale manifest entry can never hide a file that is
/// actually present in the archive.
pub fn import_full_export(zip_bytes: &[u8]) -> Result<FullExportImport> {
    let mut archive = zip::ZipArchive::new(Cursor::new(zip_bytes))
        .context("not a valid Postman \"Full Data Export\" zip file")?;

    let mut result = FullExportImport::default();
    for index in 0..archive.len() {
        let mut entry = archive
            .by_index(index)
            .context("failed to read a zip entry")?;
        if !entry.is_file() {
            continue;
        }
        let name = entry.name().to_string();
        let is_collection = name.contains("/collection/") && name.ends_with(".json");
        let is_environment = name.contains("/environment/") && name.ends_with(".json");
        if !is_collection && !is_environment {
            continue;
        }

        let mut contents = String::new();
        if let Err(error) = entry.read_to_string(&mut contents) {
            result.failed.push(FailedImport {
                file_name: name,
                error: error.to_string(),
            });
            continue;
        }

        if is_collection {
            match parse_postman_collection(&contents) {
                Ok(imported) => result.collections.push(imported),
                Err(error) => result.failed.push(FailedImport {
                    file_name: name,
                    error: error.to_string(),
                }),
            }
        } else {
            match parse_postman_environment(&contents) {
                Ok(environment) => result.environments.push(environment),
                Err(error) => result.failed.push(FailedImport {
                    file_name: name,
                    error: error.to_string(),
                }),
            }
        }
    }
    Ok(result)
}

/// One collection worth of exportable data: the collection itself plus every
/// folder/request that belongs to it.
pub struct CollectionExport<'a> {
    pub collection: &'a Collection,
    pub folders: &'a [Folder],
    pub requests: &'a [Request],
}

/// Exports every given collection and environment into a Postman "Full Data
/// Export" ZIP: a fresh workspace-UUID directory containing
/// `collection/<id>.json`, `environment/<id>.json`, and an `archive.json`
/// manifest -- the structural inverse of `import_full_export`.
pub fn export_full_export(
    collections: &[CollectionExport],
    environments: &[Environment],
) -> Result<Vec<u8>> {
    let workspace_id = Uuid::new_v4();
    let buffer = Cursor::new(Vec::new());
    let mut zip = zip::ZipWriter::new(buffer);
    let options =
        zip::write::FileOptions::default().compression_method(zip::CompressionMethod::Deflated);

    let mut collection_manifest = serde_json::Map::new();
    for export in collections {
        let json = export_postman_collection(export.collection, export.folders, export.requests);
        zip.start_file(
            format!("{workspace_id}/collection/{}.json", export.collection.id),
            options,
        )?;
        zip.write_all(json.as_bytes())?;
        collection_manifest.insert(
            export.collection.id.to_string(),
            serde_json::Value::Bool(true),
        );
    }

    let mut environment_manifest = serde_json::Map::new();
    for environment in environments {
        let json = export_postman_environment(environment);
        zip.start_file(
            format!("{workspace_id}/environment/{}.json", environment.id),
            options,
        )?;
        zip.write_all(json.as_bytes())?;
        environment_manifest.insert(environment.id.to_string(), serde_json::Value::Bool(true));
    }

    let manifest = serde_json::json!({
        "collection": serde_json::Value::Object(collection_manifest),
        "environment": serde_json::Value::Object(environment_manifest),
    });
    zip.start_file(format!("{workspace_id}/archive.json"), options)?;
    zip.write_all(serde_json::to_string_pretty(&manifest)?.as_bytes())?;

    Ok(zip.finish()?.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use api_client::{Environment, HttpMethod};

    fn sample_collection(name: &str) -> (Collection, Vec<Folder>, Vec<Request>) {
        let collection = Collection::new(name.to_string());
        let mut request = Request::new(collection.id, "Get users".to_string());
        request.method = HttpMethod::Get;
        request.url = "https://api.example.com/users".to_string();
        (collection, Vec::new(), vec![request])
    }

    #[test]
    fn exporting_two_collections_and_an_environment_then_reimporting_recovers_all_of_them() {
        let (collection_a, folders_a, requests_a) = sample_collection("API A");
        let (collection_b, folders_b, requests_b) = sample_collection("API B");
        let environment = Environment::new("Staging".to_string());

        let exports = [
            CollectionExport {
                collection: &collection_a,
                folders: &folders_a,
                requests: &requests_a,
            },
            CollectionExport {
                collection: &collection_b,
                folders: &folders_b,
                requests: &requests_b,
            },
        ];
        let zip_bytes = export_full_export(&exports, &[environment]).unwrap();

        let imported = import_full_export(&zip_bytes).unwrap();
        assert!(imported.failed.is_empty());
        assert_eq!(imported.collections.len(), 2);
        assert_eq!(imported.environments.len(), 1);
        let names: Vec<&str> = imported
            .collections
            .iter()
            .map(|imported| imported.collection.name.as_str())
            .collect();
        assert!(names.contains(&"API A"));
        assert!(names.contains(&"API B"));
        assert_eq!(imported.environments[0].name, "Staging");
    }

    #[test]
    fn a_malformed_collection_file_is_reported_without_failing_the_whole_import() {
        let (collection, folders, requests) = sample_collection("API A");
        let exports = [CollectionExport {
            collection: &collection,
            folders: &folders,
            requests: &requests,
        }];
        let mut zip_bytes = export_full_export(&exports, &[]).unwrap();

        // Append a second, deliberately malformed collection file alongside
        // the valid one, to prove one bad file doesn't sink the good ones.
        let buffer = Cursor::new(std::mem::take(&mut zip_bytes));
        let mut archive = zip::ZipArchive::new(buffer).unwrap();
        let mut rebuilt = Cursor::new(Vec::new());
        {
            let mut writer = zip::ZipWriter::new(&mut rebuilt);
            let options = zip::write::FileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated);
            for index in 0..archive.len() {
                let mut entry = archive.by_index(index).unwrap();
                let name = entry.name().to_string();
                let mut contents = Vec::new();
                entry.read_to_end(&mut contents).unwrap();
                writer.start_file(name, options).unwrap();
                writer.write_all(&contents).unwrap();
            }
            writer
                .start_file("workspace/collection/broken.json", options)
                .unwrap();
            writer.write_all(b"{not json").unwrap();
            writer.finish().unwrap();
        }

        let imported = import_full_export(rebuilt.get_ref()).unwrap();
        assert_eq!(imported.collections.len(), 1);
        assert_eq!(imported.failed.len(), 1);
        assert_eq!(
            imported.failed[0].file_name,
            "workspace/collection/broken.json"
        );
    }

    #[test]
    fn a_non_zip_byte_stream_is_rejected_rather_than_panicking() {
        assert!(import_full_export(b"not a zip file").is_err());
    }
}
