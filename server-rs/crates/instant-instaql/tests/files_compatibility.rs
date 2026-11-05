use std::cell::Cell;
use std::sync::Mutex;

use instant_datalog::{DatalogResult, DatalogTriple, DatalogValue};
use instant_instaql::{
    AsyncFileUrlBatchFuture, AsyncFileUrlSigner, Attribute, AttributeCatalog, AttributeValueType,
    Cardinality, ExecutedEntity, ExecutedNode, FILES_ETYPE, FileUrlSigner, ResultShapeError,
    compile_query, files_attributes, shape_response_with_file_urls,
    shape_response_with_file_urls_async, system_attr_id, transform_file_results_async,
    transform_file_results_async_owned,
};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

#[derive(Deserialize)]
struct FilesFixture {
    source: String,
    cases: Vec<FilesCase>,
}

#[derive(Deserialize)]
struct FilesCase {
    name: String,
    query: Value,
    expected_fetch: Vec<String>,
}

struct RecordingSigner {
    calls: Cell<usize>,
    fail: bool,
}

impl FileUrlSigner for RecordingSigner {
    fn signed_download_url(&self, app_id: Uuid, location_id: &str) -> Result<String, String> {
        self.calls.set(self.calls.get() + 1);
        if self.fail {
            Err("temporary signer failure".to_owned())
        } else {
            Ok(format!(
                "https://s3-redefed-url-for.aws.com/{app_id}/{location_id}"
            ))
        }
    }
}

#[derive(Default)]
struct RecordingBatchSigner {
    batches: Mutex<Vec<Vec<String>>>,
}

impl AsyncFileUrlSigner for RecordingBatchSigner {
    fn signed_download_urls(
        &self,
        app_id: Uuid,
        location_ids: Vec<String>,
    ) -> AsyncFileUrlBatchFuture<'_> {
        self.batches.lock().unwrap().push(location_ids.clone());
        Box::pin(async move {
            Ok(location_ids
                .into_iter()
                .map(|location| format!("https://files.example/{app_id}/{location}"))
                .collect())
        })
    }
}

fn fixture() -> FilesFixture {
    serde_json::from_str(include_str!("fixtures/files.json")).unwrap()
}

fn file_catalog() -> AttributeCatalog {
    AttributeCatalog {
        attributes: files_attributes().unwrap(),
    }
}

fn attr_id(label: &str) -> Uuid {
    system_attr_id(FILES_ETYPE, label).unwrap()
}

fn triple(entity_id: Uuid, label: &str, value: Value, created_at: i64) -> DatalogTriple {
    DatalogTriple {
        entity_id,
        attr_id: attr_id(label),
        value: DatalogValue::Json(value),
        created_at,
    }
}

fn result(entity_id: Uuid, fields: &[&str]) -> ExecutedNode {
    let triples = fields
        .iter()
        .enumerate()
        .map(|(index, field)| {
            let value = match *field {
                "id" => json!(entity_id),
                "path" => json!("profile.png"),
                "size" => json!(8192),
                "content-type" => json!("image/png"),
                "content-disposition" => json!("inline"),
                "location-id" => json!("profile-loc"),
                "key-version" => json!(1),
                other => panic!("unsupported test field {other}"),
            };
            vec![triple(
                entity_id,
                field,
                value,
                100 + i64::try_from(index).unwrap(),
            )]
        })
        .collect();
    ExecutedNode {
        selector: DatalogResult::default(),
        aggregate: None,
        entities: vec![ExecutedEntity {
            fetch: DatalogResult {
                join_rows: triples,
                ..DatalogResult::default()
            },
            children: Vec::new(),
        }],
    }
}

#[test]
fn file_fetch_fields_match_the_jvm_compatibility_cases() {
    let catalog = file_catalog();
    let fixture = fixture();
    assert!(fixture.source.contains("instaql_test.clj:4808-4930"));

    for case in fixture.cases {
        let query = compile_query(&case.query, &catalog, false)
            .unwrap_or_else(|error| panic!("{}: {error}", case.name));
        let labels = query.roots[0]
            .fetch_attr_ids
            .iter()
            .map(|id| catalog.by_id(*id).unwrap().label.clone())
            .collect::<Vec<_>>();
        assert_eq!(labels, case.expected_fetch, "{}", case.name);
    }
}

#[test]
fn default_file_fields_include_a_signed_url_and_location() {
    let catalog = file_catalog();
    let query = compile_query(&json!({"$files": {}}), &catalog, false).unwrap();
    let file_id = Uuid::from_u128(0xf111_1111_1111_1111_1111_1111_1111_1111);
    let results = vec![result(
        file_id,
        &[
            "id",
            "path",
            "size",
            "content-type",
            "content-disposition",
            "location-id",
            "key-version",
        ],
    )];
    let signer = RecordingSigner {
        calls: Cell::new(0),
        fail: false,
    };
    let app_id = Uuid::from_u128(0xa222_2222_2222_2222_2222_2222_2222_2222);
    let response =
        shape_response_with_file_urls(&query, &results, &catalog, app_id, &signer, false).unwrap();
    let file = &response.data["$files"][0];

    assert_eq!(file["location-id"], "profile-loc");
    assert_eq!(
        file["url"],
        format!("https://s3-redefed-url-for.aws.com/{app_id}/profile-loc")
    );
    assert_eq!(file["path"], "profile.png");
    assert_eq!(signer.calls.get(), 1);
}

#[test]
fn url_only_uses_but_redacts_location_id() {
    let catalog = file_catalog();
    let query = compile_query(
        &json!({"$files": {"$": {"fields": ["url"]}}}),
        &catalog,
        false,
    )
    .unwrap();
    let file_id = Uuid::from_u128(0xf222_2222_2222_2222_2222_2222_2222_2222);
    let signer = RecordingSigner {
        calls: Cell::new(0),
        fail: false,
    };
    let response = shape_response_with_file_urls(
        &query,
        &[result(file_id, &["id", "location-id"])],
        &catalog,
        Uuid::nil(),
        &signer,
        false,
    )
    .unwrap();
    let file = response.data["$files"][0].as_object().unwrap();

    assert_eq!(file["id"], file_id.to_string());
    assert!(file.contains_key("url"));
    assert!(!file.contains_key("location-id"));
    assert_eq!(signer.calls.get(), 1);
}

#[test]
fn explicitly_selected_location_id_is_exposed() {
    let catalog = file_catalog();
    let query = compile_query(
        &json!({"$files": {"$": {"fields": ["url", "location-id"]}}}),
        &catalog,
        false,
    )
    .unwrap();
    let signer = RecordingSigner {
        calls: Cell::new(0),
        fail: false,
    };
    let response = shape_response_with_file_urls(
        &query,
        &[result(Uuid::nil(), &["id", "location-id"])],
        &catalog,
        Uuid::nil(),
        &signer,
        false,
    )
    .unwrap();

    assert_eq!(response.data["$files"][0]["location-id"], "profile-loc");
    assert_eq!(signer.calls.get(), 1);
}

#[test]
fn omitting_url_skips_signing_and_hidden_location_fetching() {
    let catalog = file_catalog();
    let query = compile_query(
        &json!({"$files": {"$": {"fields": ["path"]}}}),
        &catalog,
        false,
    )
    .unwrap();
    let signer = RecordingSigner {
        calls: Cell::new(0),
        fail: true,
    };
    let response = shape_response_with_file_urls(
        &query,
        &[result(Uuid::nil(), &["id", "path"])],
        &catalog,
        Uuid::nil(),
        &signer,
        false,
    )
    .unwrap();
    let file = response.data["$files"][0].as_object().unwrap();

    assert_eq!(file["path"], "profile.png");
    assert!(!file.contains_key("url"));
    assert!(!file.contains_key("location-id"));
    assert_eq!(signer.calls.get(), 0);
}

#[tokio::test]
async fn nested_file_relationships_receive_the_same_virtual_url_transform() {
    let mut catalog = file_catalog();
    let user_id_attr = Uuid::from_u128(0x1111_1111_1111_1111_1111_1111_1111_1111);
    catalog.attributes.extend([
        Attribute {
            id: user_id_attr,
            wire: None,
            etype: "users".to_owned(),
            label: "id".to_owned(),
            value_type: AttributeValueType::Blob,
            cardinality: Cardinality::One,
            unique: true,
            setting_unique: false,
            indexed: true,
            required: true,
            checked_data_type: None,
            indexing: false,
            checking_data_type: false,
            reverse_etype: None,
            reverse_label: None,
            on_delete: None,
            on_delete_reverse: None,
        },
        Attribute {
            id: Uuid::from_u128(0x2222_2222_2222_2222_2222_2222_2222_2222),
            wire: None,
            etype: "users".to_owned(),
            label: "avatar".to_owned(),
            value_type: AttributeValueType::Ref,
            cardinality: Cardinality::One,
            unique: false,
            setting_unique: false,
            indexed: false,
            required: false,
            checked_data_type: None,
            indexing: false,
            checking_data_type: false,
            reverse_etype: Some(FILES_ETYPE.to_owned()),
            reverse_label: Some("users".to_owned()),
            on_delete: None,
            on_delete_reverse: None,
        },
    ]);
    let query = compile_query(
        &json!({"users": {"avatar": {"$": {"fields": ["url"]}}}}),
        &catalog,
        false,
    )
    .unwrap();
    let user_id = Uuid::from_u128(0x3333_3333_3333_3333_3333_3333_3333_3333);
    let file_id = Uuid::from_u128(0x4444_4444_4444_4444_4444_4444_4444_4444);
    let mut root = ExecutedNode {
        selector: DatalogResult::default(),
        aggregate: None,
        entities: vec![ExecutedEntity {
            fetch: DatalogResult {
                join_rows: vec![vec![DatalogTriple {
                    entity_id: user_id,
                    attr_id: user_id_attr,
                    value: DatalogValue::Json(json!(user_id)),
                    created_at: 10,
                }]],
                ..DatalogResult::default()
            },
            children: Vec::new(),
        }],
    };
    root.entities[0]
        .children
        .push(result(file_id, &["id", "location-id"]));
    let signer = RecordingSigner {
        calls: Cell::new(0),
        fail: false,
    };
    let results = vec![root];
    let response =
        shape_response_with_file_urls(&query, &results, &catalog, Uuid::nil(), &signer, true)
            .unwrap();
    let avatar = response.data["users"][0]["avatar"].as_object().unwrap();

    assert!(avatar.contains_key("url"));
    assert!(!avatar.contains_key("location-id"));
    assert_eq!(signer.calls.get(), 1);
    let signer = RecordingBatchSigner::default();
    let legacy = transform_file_results_async(&query, &results, &catalog, Uuid::nil(), &signer)
        .await
        .unwrap();
    let pointer = results.as_ptr();
    let owned = transform_file_results_async_owned(&query, results, &catalog, Uuid::nil(), &signer)
        .await
        .unwrap();
    assert_eq!(legacy, owned);
    assert_eq!(owned.as_ptr(), pointer);
}

#[test]
fn signing_failures_do_not_mutate_the_execution_tree() {
    let catalog = file_catalog();
    let query = compile_query(
        &json!({"$files": {"$": {"fields": ["url"]}}}),
        &catalog,
        false,
    )
    .unwrap();
    let results = vec![result(Uuid::nil(), &["id", "location-id"])];
    let snapshot = results.clone();
    let signer = RecordingSigner {
        calls: Cell::new(0),
        fail: true,
    };
    let error =
        shape_response_with_file_urls(&query, &results, &catalog, Uuid::nil(), &signer, false)
            .unwrap_err();

    assert_eq!(
        error,
        ResultShapeError::FileUrlSigning("temporary signer failure".to_owned())
    );
    assert_eq!(results, snapshot);
}

#[tokio::test]
async fn async_signing_batches_all_files_without_mutating_source_results() {
    let catalog = file_catalog();
    let query = compile_query(&json!({"$files": {}}), &catalog, false).unwrap();
    let first_id = Uuid::from_u128(1);
    let second_id = Uuid::from_u128(2);
    let mut root = result(first_id, &["id", "location-id"]);
    root.entities
        .extend(result(second_id, &["id", "location-id"]).entities);
    let results = vec![root];
    let snapshot = results.clone();
    let signer = RecordingBatchSigner::default();
    let app_id = Uuid::from_u128(3);

    let response =
        shape_response_with_file_urls_async(&query, &results, &catalog, app_id, &signer, false)
            .await
            .unwrap();

    assert_eq!(
        signer.batches.lock().unwrap().as_slice(),
        [["profile-loc", "profile-loc"]]
    );
    assert_eq!(
        response.data["$files"][0]["url"],
        format!("https://files.example/{app_id}/profile-loc")
    );
    assert_eq!(
        response.data["$files"][1]["url"],
        format!("https://files.example/{app_id}/profile-loc")
    );
    assert_eq!(results, snapshot);
}

#[tokio::test]
async fn owned_file_transforms_match_all_legacy_field_cases_and_reuse_the_tree() {
    let catalog = file_catalog();
    let app = Uuid::from_u128(99);
    for case in fixture().cases {
        let query = compile_query(&case.query, &catalog, false).unwrap();
        let fields = case
            .expected_fetch
            .iter()
            .map(String::as_str)
            .filter(|field| *field != "url")
            .collect::<Vec<_>>();
        let input = vec![result(Uuid::from_u128(1), &fields)];
        let snapshot = input.clone();
        let signer = RecordingBatchSigner::default();
        let legacy = transform_file_results_async(&query, &input, &catalog, app, &signer)
            .await
            .unwrap();
        let pointer = input.as_ptr();
        let owned = transform_file_results_async_owned(&query, input, &catalog, app, &signer)
            .await
            .unwrap();
        assert_eq!(owned, legacy, "{}", case.name);
        assert_eq!(
            owned.as_ptr(),
            pointer,
            "the owned root vector must not be copied"
        );
        assert_eq!(
            snapshot[0].entities[0]
                .fetch
                .join_rows
                .iter()
                .flatten()
                .filter(|t| t.attr_id == attr_id("url"))
                .count(),
            0
        );
    }
}

#[tokio::test]
async fn owned_file_transform_keeps_validation_errors_and_signer_boundaries() {
    let catalog = file_catalog();
    let query = compile_query(&json!({"$files": {}}), &catalog, false).unwrap();
    let mut invalid_location = result(Uuid::from_u128(1), &["id", "location-id"]);
    invalid_location.entities[0].fetch.join_rows[1][0].value = DatalogValue::Json(json!(17));
    let mut invalid_children = result(Uuid::from_u128(1), &["id"]);
    invalid_children.entities[0]
        .children
        .push(result(Uuid::from_u128(2), &["id"]));
    for input in [vec![], vec![invalid_location], vec![invalid_children]] {
        let signer = RecordingBatchSigner::default();
        let legacy = transform_file_results_async(&query, &input, &catalog, Uuid::nil(), &signer)
            .await
            .unwrap_err();
        let owned =
            transform_file_results_async_owned(&query, input, &catalog, Uuid::nil(), &signer)
                .await
                .unwrap_err();
        assert_eq!(owned, legacy);
        assert!(
            signer.batches.lock().unwrap().is_empty(),
            "validate the complete tree before signing"
        );
    }
}

#[tokio::test]
async fn owned_file_transform_preserves_signing_failures_and_count_errors() {
    struct FixedSigner(Result<Vec<String>, String>);
    impl AsyncFileUrlSigner for FixedSigner {
        fn signed_download_urls(&self, _: Uuid, _: Vec<String>) -> AsyncFileUrlBatchFuture<'_> {
            Box::pin(async { self.0.clone() })
        }
    }
    let catalog = file_catalog();
    let query = compile_query(&json!({"$files": {}}), &catalog, false).unwrap();
    let input = vec![result(Uuid::from_u128(1), &["id", "location-id"])];
    for outcome in [
        Err("temporary failure".into()),
        Ok(vec![]),
        Ok(vec!["one".into(), "two".into()]),
    ] {
        let signer = FixedSigner(outcome);
        let legacy = transform_file_results_async(&query, &input, &catalog, Uuid::nil(), &signer)
            .await
            .unwrap_err();
        let owned = transform_file_results_async_owned(
            &query,
            input.clone(),
            &catalog,
            Uuid::nil(),
            &signer,
        )
        .await
        .unwrap_err();
        assert_eq!(owned, legacy);
    }
}
