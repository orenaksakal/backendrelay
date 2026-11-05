//! Real pooled framework queries: plans may persist, authorization/data may not.
use super::*;
use instant_instaql::{Attribute, AttributeValueType, Cardinality};

struct Authorizer(AttributeCatalog);

impl AdminDataAuthorizer for Authorizer {
    async fn authorize(
        &self,
        _: AdminRequestContext,
        _: AdminDataScope,
    ) -> Result<AttributeCatalog, ApiError> {
        Ok(self.0.clone())
    }
    async fn load_catalog(&self, _: Uuid) -> Result<AttributeCatalog, ApiError> {
        Ok(self.0.clone())
    }
}

struct Planner;

impl QueryPlannerPolicy for Planner {
    fn options<'a>(&'a self, _: &'a Value) -> QueryPlannerPolicyFuture<'a> {
        Box::pin(std::future::ready(Ok(MatchCteOptions {
            pg_hints_enabled: Some(true),
            candidate_entity_scans: Some(true),
            ..Default::default()
        })))
    }
}

fn attribute(label: &str, indexed: bool) -> Attribute {
    Attribute {
        id: Uuid::new_v4(),
        wire: None,
        etype: "posts".into(),
        label: label.into(),
        value_type: AttributeValueType::Blob,
        cardinality: Cardinality::One,
        unique: label == "id",
        setting_unique: false,
        indexed,
        required: false,
        checked_data_type: None,
        indexing: false,
        checking_data_type: false,
        reverse_etype: None,
        reverse_label: None,
        on_delete: None,
        on_delete_reverse: None,
    }
}

#[tokio::test]
async fn structural_plans_keep_fresh_actors_rows_rules_and_revocation_on_the_actual_framework_path()
{
    let Some(url) = instant_test_support::test_database_url(
        "structural_plans_keep_fresh_actors_rows_rules_and_revocation_on_the_actual_framework_path",
    ) else {
        return;
    };
    let (mut setup, driver) = tokio_postgres::connect(&url, tokio_postgres::NoTls)
        .await
        .unwrap();
    let driver = tokio::spawn(driver);
    let creator = Uuid::new_v4();
    let app = Uuid::new_v4();
    setup
        .execute(
            "INSERT INTO instant_users(id,email) VALUES($1,$2)",
            &[&creator, &format!("{creator}@example.invalid")],
        )
        .await
        .unwrap();
    setup
        .execute(
            "INSERT INTO apps(id,creator_id,title) VALUES($1,$2,'structural query fixture')",
            &[&app, &creator],
        )
        .await
        .unwrap();
    let mut catalog = instant_instaql::system_attribute_catalog().unwrap();
    let attrs = [
        attribute("id", true),
        attribute("ownerId", true),
        attribute("title", false),
        attribute("revision", false),
    ];
    catalog.attributes.extend(attrs.clone());
    for attr in &attrs {
        let ident = Uuid::new_v4();
        setup.execute("INSERT INTO attrs(id,app_id,value_type,cardinality,is_unique,is_indexed,forward_ident,etype,label) VALUES($1,$2,'blob','one',$3,$4,$5,'posts',$6)",
            &[&attr.id,&app,&attr.unique,&attr.indexed,&ident,&attr.label]).await.unwrap();
        setup
            .execute(
                "INSERT INTO idents(id,app_id,attr_id,etype,label) VALUES($1,$2,$3,'posts',$4)",
                &[&ident, &app, &attr.id, &attr.label],
            )
            .await
            .unwrap();
    }
    let users = [
        instant_auth::sign_in_guest(&mut setup, app, &catalog)
            .await
            .unwrap(),
        instant_auth::sign_in_guest(&mut setup, app, &catalog)
            .await
            .unwrap(),
    ];
    let entities = [Uuid::new_v4(), Uuid::new_v4()];
    for (index, user) in users.iter().enumerate() {
        for (attr, value) in attrs.iter().zip([
            json!(entities[index]),
            json!(user.user.id),
            json!(format!("private-{index}")),
            json!("fresh"),
        ]) {
            setup.execute("INSERT INTO triples(app_id,entity_id,attr_id,value,value_md5,ea,eav,av,ave,vae) VALUES($1,$2,$3,$4,md5($4::jsonb::text),true,false,$5,$6,false)",
                &[&app,&entities[index],&attr.id,&Json(value),&attr.unique,&attr.indexed]).await.unwrap();
        }
    }
    let rules = json!({"posts":{"allow":{"view":"auth.id != null && auth.id == data.ownerId"}}});
    setup
        .execute(
            "INSERT INTO rules(app_id,code) VALUES($1,$2)",
            &[&app, &Json(&rules)],
        )
        .await
        .unwrap();
    let pool = crate::PostgresPool::connect(&url, 1).await.unwrap();
    let service = AdminDataService::from_pool(pool.clone(), Arc::new(Authorizer(catalog.clone())))
        .with_native_sdk_typed_protocol()
        .with_native_structural_query_plans()
        .with_query_planner_policy(Arc::new(Planner));
    async fn run(
        service: &AdminDataService<Authorizer>,
        app: Uuid,
        token: Uuid,
        nonce: &str,
        native: bool,
    ) -> Result<RuntimeFrameworkResponse, ApiError> {
        DatabaseWorkload::Refresh.scope(service.permissioned_framework_query(AdminRequestContext {
            app_id: app, bearer_token: None, actor: AdminActor::RefreshToken(token), client_ip: None,
        }, json!({"posts":{"$":{"where":{"revision":{"$ne":nonce}},"fields":["id","title"],"limit":10}}}).as_object().unwrap().clone(),native)).await
    }
    for n in 0..14 {
        for (index, user) in users.iter().enumerate() {
            let result = run(
                &service,
                app,
                user.refresh_token,
                &format!("nonce-{n}' unicode-λ"),
                n % 2 == 0,
            )
            .await
            .unwrap();
            let text = result.result.to_string();
            assert!(text.contains(&format!("private-{index}")));
            assert!(!text.contains(&format!("private-{}", 1 - index)));
        }
    }
    let inspect = pool.acquire().await.unwrap();
    let prepared = inspect.query_one("SELECT count(*),coalesce(sum(generic_plans),0)::bigint FROM pg_prepared_statements WHERE statement LIKE 'WITH %' AND statement LIKE '%json_build_array%'",&[]).await.unwrap();
    assert_eq!(
        prepared.get::<_, i64>(0),
        1,
        "fresh nonces and actors must not create SQL keys"
    );
    assert!(
        prepared.get::<_, i64>(1) > 0,
        "automatic plan reuse must actually occur"
    );
    assert_eq!(
        inspect
            .query_one("SHOW plan_cache_mode", &[])
            .await
            .unwrap()
            .get::<_, String>(0),
        "auto"
    );
    drop(inspect);
    setup.execute("UPDATE triples SET value='\"changed-after-plan\"'::jsonb,value_md5=md5('\"changed-after-plan\"') WHERE app_id=$1 AND entity_id=$2 AND attr_id=$3",&[&app,&entities[0],&attrs[2].id]).await.unwrap();
    let changed = run(&service, app, users[0].refresh_token, "new-nonce", true)
        .await
        .unwrap();
    assert!(changed.result.to_string().contains("changed-after-plan"));
    let denied = json!({"posts":{"allow":{"view":"false"}}});
    setup
        .execute(
            "UPDATE rules SET code=$2 WHERE app_id=$1",
            &[&app, &Json(denied)],
        )
        .await
        .unwrap();
    let result = run(&service, app, users[0].refresh_token, "new-nonce", true)
        .await
        .unwrap();
    assert!(!result.result.to_string().contains("changed-after-plan"));
    setup
        .execute(
            "UPDATE rules SET code=$2 WHERE app_id=$1",
            &[&app, &Json(rules)],
        )
        .await
        .unwrap();
    instant_auth::delete_refresh_token(&mut setup, app, &catalog, users[0].refresh_token)
        .await
        .unwrap();
    assert!(
        run(&service, app, users[0].refresh_token, "new-nonce", true)
            .await
            .is_err()
    );
    assert_eq!(pool.status().waiting, 0);
    drop(service);
    drop(pool);
    setup
        .execute("DELETE FROM apps WHERE id=$1", &[&app])
        .await
        .unwrap();
    setup
        .execute("DELETE FROM instant_users WHERE id=$1", &[&creator])
        .await
        .unwrap();
    drop(setup);
    driver.await.unwrap().unwrap();
}
