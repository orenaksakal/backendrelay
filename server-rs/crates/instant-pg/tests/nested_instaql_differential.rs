use instant_datalog::{MatchCteOptions, PatternMetadataNode};
use instant_instaql::{
    AttributeCatalog, PlannedNestedGroup, compile_query, plan_nested_query, shape_response,
};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use uuid::Uuid;

mod support;

#[derive(Deserialize)]
struct Fixture {
    oracle: String,
    app_id: Uuid,
    query: Value,
    users: Vec<User>,
    shelves: Vec<Shelf>,
    books: Vec<Book>,
    expected: Value,
}

#[derive(Deserialize)]
struct User {
    id: Uuid,
    handle: String,
    created_at: i64,
}

#[derive(Deserialize)]
struct Shelf {
    id: Uuid,
    owner: Uuid,
    name: String,
    created_at: i64,
}

#[derive(Deserialize)]
struct Book {
    id: Uuid,
    shelf: Uuid,
    title: String,
    created_at: i64,
}

#[test]
fn three_level_nested_query_compiles_regroups_and_shapes_like_clojure() {
    let fixture: Fixture =
        serde_json::from_str(include_str!("fixtures/nested_instaql_differential.json")).unwrap();
    assert!(fixture.oracle.starts_with("4e0a1bd8 "));
    let catalog = catalog();
    let compiled = compile_query(&fixture.query, &catalog, false).unwrap();
    let plan = plan_nested_query(&compiled, fixture.app_id, &MatchCteOptions::default()).unwrap();

    let root_selector = &plan.groups[0];
    let root_fetch = only_group(root_selector);
    let shelf_selector = only_group(root_fetch);
    let shelf_fetch = only_group(shelf_selector);
    let book_selector = only_group(shelf_fetch);
    let book_fetch = only_group(book_selector);
    assert_eq!(plan.sql_plan().result_tables.len(), 6);

    let user_id_attr = attr(&catalog, "users", "id");
    let handle_attr = attr(&catalog, "users", "handle");
    let shelves_attr = attr(&catalog, "users", "bookshelves");
    let shelf_id_attr = attr(&catalog, "bookshelves", "id");
    let shelf_name_attr = attr(&catalog, "bookshelves", "name");
    let books_attr = attr(&catalog, "bookshelves", "books");
    let book_id_attr = attr(&catalog, "books", "id");
    let title_attr = attr(&catalog, "books", "title");

    let mut envelope = Map::new();
    insert_user_rows(
        &mut envelope,
        root_selector,
        root_fetch,
        &fixture.users,
        user_id_attr,
        handle_attr,
    );
    insert_shelf_rows(
        &mut envelope,
        shelf_selector,
        shelf_fetch,
        &fixture.shelves,
        [shelves_attr, shelf_id_attr, shelf_name_attr],
    );
    insert_book_rows(
        &mut envelope,
        book_selector,
        book_fetch,
        &fixture.books,
        [books_attr, book_id_attr, title_attr],
    );

    let executed =
        support::decode_envelope(&plan, &Value::Array(vec![Value::Object(envelope)])).unwrap();
    assert_nested_topology(&executed);

    let response = shape_response(&compiled, &executed, &catalog, false).unwrap();
    assert_eq!(
        serde_json::to_value(response.data).unwrap(),
        fixture.expected
    );
}

fn insert_user_rows(
    envelope: &mut Map<String, Value>,
    root_selector: &PlannedNestedGroup,
    root_fetch: &PlannedNestedGroup,
    users: &[User],
    user_id_attr: Uuid,
    handle_attr: Uuid,
) {
    insert_rows(
        envelope,
        root_selector,
        users.iter().map(|user| {
            row(
                root_selector,
                user.id,
                user_id_attr,
                json!(user.id),
                false,
                user.created_at,
            )
        }),
    );
    insert_rows(
        envelope,
        root_fetch,
        users.iter().flat_map(|user| {
            [
                row(
                    root_fetch,
                    user.id,
                    user_id_attr,
                    json!(user.id),
                    false,
                    user.created_at,
                ),
                row(
                    root_fetch,
                    user.id,
                    handle_attr,
                    json!(user.handle),
                    false,
                    user.created_at + 100,
                ),
            ]
        }),
    );
}

fn insert_shelf_rows(
    envelope: &mut Map<String, Value>,
    shelf_selector: &PlannedNestedGroup,
    shelf_fetch: &PlannedNestedGroup,
    shelves: &[Shelf],
    [shelves_attr, shelf_id_attr, shelf_name_attr]: [Uuid; 3],
) {
    insert_rows(
        envelope,
        shelf_selector,
        shelves.iter().map(|shelf| {
            row(
                shelf_selector,
                shelf.owner,
                shelves_attr,
                json!(shelf.id),
                true,
                shelf.created_at,
            )
        }),
    );
    insert_rows(
        envelope,
        shelf_fetch,
        shelves.iter().flat_map(|shelf| {
            [
                row(
                    shelf_fetch,
                    shelf.id,
                    shelf_id_attr,
                    json!(shelf.id),
                    false,
                    shelf.created_at,
                ),
                row(
                    shelf_fetch,
                    shelf.id,
                    shelf_name_attr,
                    json!(shelf.name),
                    false,
                    shelf.created_at + 100,
                ),
            ]
        }),
    );
}

fn insert_book_rows(
    envelope: &mut Map<String, Value>,
    book_selector: &PlannedNestedGroup,
    book_fetch: &PlannedNestedGroup,
    books: &[Book],
    [books_attr, book_id_attr, title_attr]: [Uuid; 3],
) {
    insert_rows(
        envelope,
        book_selector,
        books.iter().map(|book| {
            row(
                book_selector,
                book.shelf,
                books_attr,
                json!(book.id),
                true,
                book.created_at,
            )
        }),
    );
    insert_rows(
        envelope,
        book_fetch,
        books.iter().flat_map(|book| {
            [
                row(
                    book_fetch,
                    book.id,
                    book_id_attr,
                    json!(book.id),
                    false,
                    book.created_at,
                ),
                row(
                    book_fetch,
                    book.id,
                    title_attr,
                    json!(book.title),
                    false,
                    book.created_at + 100,
                ),
            ]
        }),
    );
}

fn assert_nested_topology(executed: &[instant_instaql::ExecutedNode]) {
    assert_eq!(executed[0].entities.len(), 2);
    assert_eq!(executed[0].entities[0].children[0].entities.len(), 2);
    assert!(executed[0].entities[1].children[0].entities.is_empty());
    assert_eq!(
        executed[0].entities[0].children[0].entities[0].children[0]
            .entities
            .len(),
        2
    );
    assert!(
        executed[0].entities[0].children[0].entities[1].children[0]
            .entities
            .is_empty()
    );
}

fn catalog() -> AttributeCatalog {
    serde_json::from_value(
        serde_json::from_str::<Value>(include_str!(
            "../../instant-instaql/tests/fixtures/where_compilation.json"
        ))
        .unwrap()["catalog"]
            .clone(),
    )
    .unwrap()
}

fn attr(catalog: &AttributeCatalog, etype: &str, label: &str) -> Uuid {
    catalog
        .attributes
        .iter()
        .find(|attribute| attribute.etype == etype && attribute.label == label)
        .unwrap()
        .id
}

fn only_group(group: &PlannedNestedGroup) -> &PlannedNestedGroup {
    let children = group.children.as_ref().unwrap();
    assert_eq!(children.groups.len(), 1);
    &children.groups[0]
}

fn insert_rows(
    envelope: &mut Map<String, Value>,
    group: &PlannedNestedGroup,
    rows: impl IntoIterator<Item = Value>,
) {
    envelope.insert(
        group.table.clone().unwrap(),
        Value::Array(rows.into_iter().collect()),
    );
}

fn row(
    group: &PlannedNestedGroup,
    entity_id: Uuid,
    attr_id: Uuid,
    value: Value,
    reference: bool,
    created_at: i64,
) -> Value {
    let PatternMetadataNode::Pattern(metadata) = &group.metadata[0] else {
        panic!("fixture groups use one direct pattern");
    };
    let [
        entity,
        attribute,
        value_column,
        reference_column,
        created_column,
    ] = metadata.cte_columns.as_slice()
    else {
        panic!("pattern metadata should expose five columns");
    };
    Value::Object(Map::from_iter([
        (entity.clone(), json!(entity_id)),
        (attribute.clone(), json!(attr_id)),
        (value_column.clone(), value),
        (reference_column.clone(), json!(reference)),
        (created_column.clone(), json!(created_at)),
    ]))
}
