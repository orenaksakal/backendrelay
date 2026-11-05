use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use instant_http::{AdminPresenceBackend, AdminPresenceResponse, AdminRequestContext, ApiError};
use instant_realtime::RoomStore;
use serde_json::{Map, Value};
use tokio_postgres::Client;
use uuid::Uuid;

use crate::PostgresPool;
use crate::admin_auth::admin_user;
use crate::admin_data::{AdminDataAuthorizer, AdminDataScope, internal_error, require_admin};
use crate::postgres_pool::PostgresClientSource;
use crate::runtime_auth::auth_error;

/// Authorized live projection of the shared realtime room store.
pub struct AdminPresenceService<A> {
    client: PostgresClientSource,
    authorizer: Arc<A>,
    rooms: Arc<Mutex<RoomStore>>,
}

impl<A> AdminPresenceService<A>
where
    A: AdminDataAuthorizer,
{
    #[must_use]
    pub fn new(client: Client, authorizer: Arc<A>, rooms: Arc<Mutex<RoomStore>>) -> Self {
        Self {
            client: PostgresClientSource::fixed(client),
            authorizer,
            rooms,
        }
    }

    #[must_use]
    pub fn from_pool(pool: PostgresPool, authorizer: Arc<A>, rooms: Arc<Mutex<RoomStore>>) -> Self {
        Self {
            client: PostgresClientSource::pooled(pool),
            authorizer,
            rooms,
        }
    }
}

impl<A> AdminPresenceBackend for AdminPresenceService<A>
where
    A: AdminDataAuthorizer,
{
    async fn presence(
        &self,
        context: AdminRequestContext,
        _room_type: String,
        room_id: String,
    ) -> Result<AdminPresenceResponse, ApiError> {
        require_admin(&context)?;
        let app_id = context.app_id;
        let catalog = self
            .authorizer
            .authorize(context, AdminDataScope::Read)
            .await?;
        let Some(room) = self
            .rooms
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .room_data(app_id, &room_id)
            .cloned()
        else {
            return Ok(AdminPresenceResponse { sessions: None });
        };

        let mut user_ids = room
            .values()
            .filter_map(|peer| peer.user.map(|user| user.id))
            .collect::<Vec<_>>();
        user_ids.sort_unstable();
        user_ids.dedup();
        let client = self.client.acquire().await.map_err(internal_error)?;
        let mut users = HashMap::<Uuid, Value>::new();
        for user_id in user_ids {
            if let Some(user) = instant_auth::get_user(&*client, app_id, &catalog, user_id)
                .await
                .map_err(auth_error)?
            {
                users.insert(
                    user_id,
                    serde_json::to_value(admin_user(user)).map_err(internal_error)?,
                );
            }
        }

        let sessions = room
            .into_iter()
            .map(|(session_id, peer)| {
                let user = peer
                    .user
                    .and_then(|user| users.get(&user.id).cloned())
                    .unwrap_or(Value::Null);
                let mut value = serde_json::to_value(peer)
                    .map_err(internal_error)?
                    .as_object()
                    .cloned()
                    .ok_or_else(|| {
                        internal_error("presence session did not serialize as object")
                    })?;
                value.insert("user".to_owned(), user);
                Ok((session_id.to_string(), Value::Object(value)))
            })
            .collect::<Result<Map<_, _>, ApiError>>()?;
        Ok(AdminPresenceResponse {
            sessions: Some(sessions),
        })
    }
}
