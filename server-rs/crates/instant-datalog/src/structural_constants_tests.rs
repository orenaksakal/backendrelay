use super::*;
use crate::{IndexKey, WhereColumnRef};
use serde_json::json;

fn compare(column: WhereColumn, value: WhereExpression, structural: bool) -> RenderedQuery {
    let mut renderer = Renderer {
        structural_constants: structural,
        ..Renderer::default()
    };
    let mut sql = String::new();
    renderer
        .render_predicate(
            &mut sql,
            &WherePredicate::equal(
                WhereExpression::Column(WhereColumnRef::qualified("t-0", column)),
                value,
            ),
        )
        .unwrap();
    RenderedQuery {
        sql,
        parameters: renderer.parameters,
    }
}

#[test]
fn physical_index_flags_are_constants_but_data_booleans_remain_parameters() {
    for index in [
        IndexKey::Ea,
        IndexKey::Eav,
        IndexKey::Av,
        IndexKey::Ave,
        IndexKey::Vae,
    ] {
        for value in [false, true] {
            let expression = WhereExpression::Literal(json!(value));
            let old = compare(WhereColumn::Index(index), expression.clone(), false);
            let new = compare(WhereColumn::Index(index), expression.clone(), true);
            assert_eq!(old.parameters, [SqlParameter::Boolean(value)]);
            assert!(new.parameters.is_empty());
            assert!(
                new.sql
                    .ends_with(if value { " = TRUE)" } else { " = FALSE)" })
            );
            let data = compare(WhereColumn::Value, expression, true);
            assert!(data.sql.contains('$'));
            assert!(!data.parameters.is_empty());
        }
    }
}

#[test]
fn schema_uuids_are_constants_but_entities_cursors_and_json_uuid_values_stay_bound() {
    let id = Uuid::from_u128(99);
    for column in [WhereColumn::AppId, WhereColumn::AttrId] {
        let new = compare(column, WhereExpression::Uuid(id), true);
        assert!(new.sql.contains(&format!("'{id}'::uuid")));
        assert!(new.parameters.is_empty());
        let new = compare(
            column,
            WhereExpression::Cast {
                expression: Box::new(WhereExpression::Literal(json!(id))),
                pg_type: PgType::Uuid,
            },
            true,
        );
        assert!(new.parameters.is_empty());
    }
    let entity = compare(WhereColumn::EntityId, WhereExpression::Uuid(id), true);
    assert_eq!(entity.parameters, [SqlParameter::Uuid(id)]);
    for value in [
        json!(id),
        json!(true),
        json!("' OR TRUE; --"),
        json!([id]),
        Value::Null,
    ] {
        let expression = WhereExpression::Cast {
            expression: Box::new(WhereExpression::Literal(value.clone())),
            pg_type: PgType::Jsonb,
        };
        let new = compare(WhereColumn::Value, expression, true);
        assert_eq!(new.parameters, [SqlParameter::Json(value)]);
        assert!(!new.sql.contains("OR TRUE"));
    }
}

#[test]
fn only_valid_attribute_uuid_arrays_are_constants_including_empty_arrays() {
    for values in [
        Vec::new(),
        vec![json!(Uuid::from_u128(1)), json!(Uuid::from_u128(2))],
    ] {
        let expression = WhereExpression::Any {
            values: values.clone(),
            element_type: PgType::Uuid,
        };
        let new = compare(WhereColumn::AttrId, expression.clone(), true);
        assert!(new.sql.contains("ANY(ARRAY["));
        assert!(new.parameters.is_empty());
        let entity = compare(WhereColumn::EntityId, expression, true);
        assert_eq!(
            entity.parameters,
            [SqlParameter::Array {
                values,
                element_type: PgType::Uuid
            }]
        );
    }
    let mut sql = String::new();
    assert!(!render_structural_constant(
        &mut sql,
        &WhereExpression::column(WhereColumn::AttrId),
        &WhereExpression::Any {
            values: vec![json!("not-a-uuid' OR TRUE")],
            element_type: PgType::Uuid
        }
    ));
    assert!(sql.is_empty());
}
