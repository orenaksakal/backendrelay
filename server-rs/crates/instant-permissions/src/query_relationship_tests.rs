use super::*;
use instant_datalog::{DatalogResult, DatalogTriple, DatalogValue};
use instant_instaql::{ExecutedEntity, compile_query};
use serde_json::json;

#[test]
fn forward_relationship_selectors_keep_only_the_allowed_target() {
    let (catalog, _, _, _) = query_bookkeeping_tests::fixture();
    // comments.post is a forward link. A selector begins with
    // [comment, post-attr, post], not with a triple whose entity is the post.
    let allowed_post = Uuid::from_u128(101);
    let denied_post = Uuid::from_u128(102);
    let comments = [denied_post, Uuid::from_u128(103)];
    let triple = |entity_id, attr_id, value| DatalogTriple {
        entity_id,
        attr_id: Uuid::from_u128(attr_id),
        value,
        created_at: 42,
    };
    for filtered in [false, true] {
        let child_query = if filtered {
            json!({"$": {"where": {"title": "title"}}})
        } else {
            json!({})
        };
        let query =
            compile_query(&json!({"comments": {"post": child_query}}), &catalog, false).unwrap();
        for json_reference in [false, true] {
            let mut entities = Vec::new();
            for (comment, post) in comments.into_iter().zip([allowed_post, denied_post]) {
                let edge = triple(
                    comment,
                    7,
                    if json_reference {
                        DatalogValue::Json(json!(post))
                    } else {
                        DatalogValue::Uuid(post)
                    },
                );
                let id = triple(post, 1, DatalogValue::Json(json!(post)));
                let mut selector_row = vec![edge];
                if filtered {
                    selector_row.push(triple(post, 2, DatalogValue::Json(json!("title"))));
                }
                entities.push(ExecutedEntity {
                    fetch: DatalogResult {
                        join_rows: vec![vec![triple(
                            comment,
                            5,
                            DatalogValue::Json(json!(comment)),
                        )]],
                        ..Default::default()
                    },
                    children: vec![ExecutedNode {
                        selector: DatalogResult {
                            join_rows: vec![selector_row],
                            ..Default::default()
                        },
                        entities: vec![ExecutedEntity {
                            fetch: DatalogResult {
                                join_rows: vec![vec![id]],
                                ..Default::default()
                            },
                            children: Vec::new(),
                        }],
                        ..Default::default()
                    }],
                });
            }
            let input = vec![ExecutedNode {
                selector: DatalogResult {
                    join_rows: entities
                        .iter()
                        .map(|entity| entity.fetch.join_rows[0].clone())
                        .collect(),
                    ..Default::default()
                },
                entities,
                ..Default::default()
            }];
            let original = input.clone();
            let decisions = HashMap::from([
                (
                    DecisionKey::View(PermissionEntityKey {
                        etype: "posts".to_owned(),
                        entity_id: allowed_post,
                    }),
                    true,
                ),
                (
                    DecisionKey::View(PermissionEntityKey {
                        etype: "posts".to_owned(),
                        entity_id: denied_post,
                    }),
                    false,
                ),
            ]);
            let mut owned = input.clone();
            filter_executed_level(&query.roots, &mut owned, &catalog, &decisions).unwrap();
            let borrowed =
                filter_executed_level_borrowed(&query.roots, &input, &catalog, &decisions).unwrap();
            assert_eq!(owned, borrowed);
            assert_eq!(input, original, "shared frames must not be mutated");
            assert_eq!(owned[0].entities.len(), 2);
            let visible = &owned[0].entities[0].children[0];
            assert_eq!(visible.entities.len(), 1);
            assert_eq!(
                visible.selector.join_rows, original[0].entities[0].children[0].selector.join_rows,
                "the allowed forward edge must reach the SDK"
            );
            let hidden = &owned[0].entities[1].children[0];
            assert!(hidden.entities.is_empty());
            assert!(
                hidden.selector.join_rows.is_empty(),
                "denied edges must be removed"
            );
        }
    }
}
