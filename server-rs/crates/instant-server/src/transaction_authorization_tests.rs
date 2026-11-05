//! Live fixtures for the production-only transaction authorization reservation.
use std::sync::Arc;
use std::time::Duration;

use tokio_postgres::types::Json;

use super::*;

struct Authorizer(AttributeCatalog);

impl AdminDataAuthorizer for Authorizer {
    async fn authorize(
        &self,
        _context: AdminRequestContext,
        _scope: AdminDataScope,
    ) -> Result<AttributeCatalog, ApiError> {
        Ok(self.0.clone())
    }

    async fn load_catalog(&self, _app_id: Uuid) -> Result<AttributeCatalog, ApiError> {
        Ok(self.0.clone())
    }
}

struct Fixture {
    setup: Client,
    schema: String,
    url: String,
    app: Uuid,
    user: Uuid,
    token: Uuid,
    token_entity: Uuid,
    email_attr: Uuid,
    catalog: AttributeCatalog,
}

impl Fixture {
    async fn new() -> Option<Self> {
        let url = instant_test_support::test_database_url("transaction_authorization_fixture")?;
        let (setup, connection) = tokio_postgres::connect(&url, tokio_postgres::NoTls)
            .await
            .unwrap();
        tokio::spawn(connection);
        let schema = format!("transaction_auth_{}", Uuid::new_v4().simple());
        setup
            .batch_execute(&format!(
                "CREATE SCHEMA {schema};
                 CREATE TABLE {schema}.rules(app_id uuid PRIMARY KEY,code jsonb);
                 CREATE TABLE {schema}.triples(app_id uuid,entity_id uuid,attr_id uuid,value jsonb,av bool);
                 CREATE FUNCTION {schema}.json_null_to_null(v jsonb) RETURNS jsonb
                 LANGUAGE SQL IMMUTABLE AS 'SELECT NULLIF(v, ''null''::jsonb)';"
            ))
            .await
            .unwrap();
        let catalog = instant_instaql::system_attribute_catalog().unwrap();
        let attr = |etype: &str, label: &str| {
            catalog
                .attributes
                .iter()
                .find(|attr| attr.etype == etype && attr.label == label)
                .unwrap()
                .id
        };
        let app = Uuid::new_v4();
        let user = Uuid::new_v4();
        let token = Uuid::new_v4();
        let token_entity = Uuid::new_v4();
        let email_attr = attr("$users", "email");
        for (entity, attr_id, value) in [
            (user, email_attr, json!("first@example.invalid")),
            (
                token_entity,
                attr("$userRefreshTokens", "hashedToken"),
                json!(instant_auth::hash_refresh_token(token)),
            ),
            (
                token_entity,
                attr("$userRefreshTokens", "$user"),
                json!(user),
            ),
        ] {
            setup
                .execute(
                    &format!("INSERT INTO {schema}.triples VALUES ($1,$2,$3,$4,true)"),
                    &[&app, &entity, &attr_id, &Json(value)],
                )
                .await
                .unwrap();
        }
        setup
            .execute(
                &format!("INSERT INTO {schema}.rules VALUES ($1,$2)"),
                &[&app, &Json(json!({"posts":{"allow":{"create":"true"}}}))],
            )
            .await
            .unwrap();
        let mut scoped = Url::parse(&url).unwrap();
        scoped
            .query_pairs_mut()
            .append_pair("options", &format!("-csearch_path={schema}"));
        Some(Self {
            setup,
            schema,
            url: scoped.to_string(),
            app,
            user,
            token,
            token_entity,
            email_attr,
            catalog,
        })
    }

    fn context(&self) -> AdminRequestContext {
        AdminRequestContext {
            app_id: self.app,
            bearer_token: None,
            actor: AdminActor::RefreshToken(self.token),
            client_ip: None,
        }
    }

    async fn close(self) {
        self.setup
            .batch_execute(&format!("DROP SCHEMA {} CASCADE", self.schema))
            .await
            .unwrap();
    }
}

fn request() -> AdminTransactRequest {
    AdminTransactRequest {
        steps: Vec::new(),
        throw_on_missing_attrs: false,
    }
}

async fn prepare(
    service: &AdminDataService<Authorizer>,
    context: &AdminRequestContext,
) -> Result<PreparedAdminTransaction, ApiError> {
    tokio::time::timeout(
        Duration::from_secs(2),
        service.prepare_transact(context, request()),
    )
    .await
    .expect("short transaction authorization queued behind saturated interactive work")
}

#[tokio::test]
async fn reserved_transaction_auth_keeps_live_scoped_identity_and_rules_while_data_is_full() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let data = PostgresPool::connect(&fixture.url, 1).await.unwrap();
    let control = PostgresPool::connect(&fixture.url, 1).await.unwrap();
    let service =
        AdminDataService::from_pool(data.clone(), Arc::new(Authorizer(fixture.catalog.clone())))
            .with_transaction_authorization_pool(control.clone());
    let auth_lease = service
        .transaction_authorization_client
        .as_ref()
        .unwrap()
        .acquire()
        .await
        .unwrap();
    assert_eq!(
        auth_lease
            .query_one("SHOW default_transaction_read_only", &[])
            .await
            .unwrap()
            .get::<_, String>(0),
        "on"
    );
    let write_error = auth_lease
        .execute(
            "INSERT INTO rules VALUES ($1, '{}'::jsonb)",
            &[&Uuid::new_v4()],
        )
        .await
        .unwrap_err();
    assert_eq!(
        write_error.code(),
        Some(&tokio_postgres::error::SqlState::READ_ONLY_SQL_TRANSACTION)
    );
    drop(auth_lease);
    let held = data.acquire().await.unwrap();
    let context = fixture.context();
    let prepared = prepare(&service, &context).await.unwrap();
    let permission = prepared.permission.unwrap();
    assert_eq!(permission.auth["id"], json!(fixture.user));
    assert_eq!(permission.auth["email"], "first@example.invalid");
    assert_eq!(permission.rules.code()["posts"]["allow"]["create"], "true");
    assert_eq!(data.status().waiting, 0);
    fixture
        .setup
        .execute(
            &format!(
                "UPDATE {}.triples SET value=$1 WHERE app_id=$2 AND attr_id=$3",
                fixture.schema
            ),
            &[
                &Json(json!("fresh@example.invalid")),
                &fixture.app,
                &fixture.email_attr,
            ],
        )
        .await
        .unwrap();
    fixture
        .setup
        .execute(
            &format!(
                "UPDATE {}.rules SET code=$1 WHERE app_id=$2",
                fixture.schema
            ),
            &[
                &Json(json!({"posts":{"allow":{"create":"false"}}})),
                &fixture.app,
            ],
        )
        .await
        .unwrap();
    let permission = prepare(&service, &context)
        .await
        .unwrap()
        .permission
        .unwrap();
    assert_eq!(permission.auth["email"], "fresh@example.invalid");
    assert_eq!(permission.rules.code()["posts"]["allow"]["create"], "false");
    let mut other = context.clone();
    other.app_id = Uuid::new_v4();
    let error = prepare(&service, &other).await.err().unwrap();
    assert_eq!(error.body().kind, ApiErrorType::RecordNotFound);
    fixture
        .setup
        .execute(
            &format!("DELETE FROM {}.triples WHERE entity_id=$1", fixture.schema),
            &[&fixture.token_entity],
        )
        .await
        .unwrap();
    let error = prepare(&service, &context).await.err().unwrap();
    assert_eq!(error.body().kind, ApiErrorType::RecordNotFound);
    // A new admin request must keep bypassing impersonation/rule reads.
    let control_held = control.acquire().await.unwrap();
    let mut admin = context;
    admin.actor = AdminActor::Admin;
    assert!(
        prepare(&service, &admin)
            .await
            .unwrap()
            .permission
            .is_none()
    );
    drop(control_held);
    drop(held);
    fixture.close().await;
}

#[tokio::test]
async fn transaction_auth_custom_source_and_auth_before_rule_error_priority_are_unchanged() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let data = PostgresPool::connect(&fixture.url, 1).await.unwrap();
    let control = PostgresPool::connect(&fixture.url, 1).await.unwrap();
    let service =
        AdminDataService::from_pool(data.clone(), Arc::new(Authorizer(fixture.catalog.clone())));
    let held = data.acquire().await.unwrap();
    let before_control = control.acquisition_count();
    assert!(
        tokio::time::timeout(
            Duration::from_millis(50),
            service.prepare_transact(&fixture.context(), request()),
        )
        .await
        .is_err()
    );
    assert_eq!(control.acquisition_count(), before_control);
    let service = service.with_transaction_authorization_pool(control);
    fixture
        .setup
        .batch_execute(&format!("DROP TABLE {}.rules", fixture.schema))
        .await
        .unwrap();
    let mut invalid = fixture.context();
    invalid.actor = AdminActor::RefreshToken(Uuid::new_v4());
    let error = prepare(&service, &invalid).await.err().unwrap();
    assert_eq!(error.body().kind, ApiErrorType::RecordNotFound);
    let error = prepare(&service, &fixture.context()).await.err().unwrap();
    assert_eq!(error.body().kind, ApiErrorType::Unknown);
    assert_eq!(data.status().waiting, 0);
    drop(held);
    fixture.close().await;
}

struct RejectBeforeWrite(PostgresPool);

impl TransactionPoolPressurePolicy for RejectBeforeWrite {
    fn check(&self, _app: Uuid) -> crate::TransactionPoolPressureFuture<'_> {
        Box::pin(async move {
            // This can complete only if the auth lease was returned before the
            // original pressure-policy boundary. No write lease is yet held.
            let _lease = self.0.acquire().await.map_err(internal_error)?;
            Err(ApiError::new(
                ApiErrorType::RateLimited,
                "live pressure check",
            ))
        })
    }
}

#[tokio::test]
async fn reserved_transaction_auth_cancellation_releases_waiters_and_keeps_pressure_before_write() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let data = PostgresPool::connect(&fixture.url, 1).await.unwrap();
    let control = PostgresPool::connect(&fixture.url, 1).await.unwrap();
    let service = Arc::new(
        AdminDataService::from_pool(data.clone(), Arc::new(Authorizer(fixture.catalog.clone())))
            .with_transaction_authorization_pool(control.clone())
            .with_transaction_pool_pressure_policy(Arc::new(RejectBeforeWrite(control.clone()))),
    );
    let held = data.acquire().await.unwrap();
    let control_held = control.acquire().await.unwrap();
    let pending_service = Arc::clone(&service);
    let context = fixture.context();
    let pending_context = context.clone();
    let pending = tokio::spawn(async move {
        pending_service
            .prepare_transact(&pending_context, request())
            .await
            .is_ok()
    });
    tokio::time::timeout(Duration::from_secs(2), async {
        while control.status().waiting == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    pending.abort();
    assert!(pending.await.unwrap_err().is_cancelled());
    drop(control_held);
    let prepared = prepare(&service, &context).await.unwrap();
    let error = tokio::time::timeout(
        Duration::from_secs(2),
        service.execute_prepared_transact(fixture.app, prepared),
    )
    .await
    .unwrap()
    .unwrap_err();
    assert_eq!(error.body().kind, ApiErrorType::RateLimited);
    assert_eq!(error.body().message, "live pressure check");
    assert_eq!(data.status().waiting, 0);
    assert_eq!(control.status().waiting, 0);
    let control_lease = control.acquire().await.unwrap();
    assert_eq!(
        control_lease
            .query_one("SHOW default_transaction_read_only", &[])
            .await
            .unwrap()
            .get::<_, String>(0),
        "off"
    );
    drop(control_lease);
    drop(held);
    fixture.close().await;
}
