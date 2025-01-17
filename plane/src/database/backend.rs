use super::{subscribe::emit_with_key, util::MapSqlxError, PlaneDatabase};
use crate::{
    log_types::{BackendAddr, LoggableTime},
    names::{BackendActionName, BackendName},
    protocol::{BackendAction, RouteInfo},
    types::{BackendStatus, BearerToken, NodeId, SecretToken, TimestampedBackendStatus},
};
use chrono::{DateTime, Utc};
use futures_util::Stream;
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;

pub struct BackendDatabase<'a> {
    db: &'a PlaneDatabase,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackendActionMessage {
    pub action_id: BackendActionName,
    pub backend_id: BackendName,
    pub drone_id: NodeId,
    pub action: BackendAction,
}

impl super::subscribe::NotificationPayload for BackendActionMessage {
    fn kind() -> &'static str {
        "backend_action"
    }
}

impl super::subscribe::NotificationPayload for BackendStatus {
    fn kind() -> &'static str {
        "backend_state"
    }
}

impl<'a> BackendDatabase<'a> {
    pub fn new(db: &'a PlaneDatabase) -> Self {
        Self { db }
    }

    pub async fn status_stream(
        &self,
        backend: &BackendName,
    ) -> sqlx::Result<impl Stream<Item = TimestampedBackendStatus>> {
        let mut sub = self
            .db
            .subscribe_with_key::<BackendStatus>(&backend.to_string());

        let result = sqlx::query!(
            r#"
            select
                id,
                created_at,
                status
            from backend_status
            where backend_id = $1
            order by id asc
            "#,
            backend.to_string(),
        )
        .fetch_all(&self.db.pool)
        .await?;

        let stream = async_stream::stream! {
            let mut last_status = None;
            for row in result {
                let status = BackendStatus::try_from(row.status);
                match status {
                    Ok(status) => {
                        yield TimestampedBackendStatus {
                            time: LoggableTime(row.created_at),
                            status,
                        };
                        last_status = Some(status);
                    }
                    Err(e) => {
                        tracing::warn!(?e, "Invalid backend status");
                    }
                }
            }

            while let Some(item) = sub.next().await {
                // In order to missing events that occur when we read the DB and when we subscribe to updates,
                // we subscribe to updates before we read from the DB. But this means we might get duplicate
                // events, so we keep track of the last status we saw and ignore events that have a status
                // less than or equal to it.
                if let Some(last_status) = last_status {
                    if item.payload <= last_status {
                        continue;
                    }
                }

                let status = item.payload;
                let time = item.timestamp;

                let item = TimestampedBackendStatus {
                    status,
                    time: LoggableTime(time),
                };

                yield item;
            }
        };

        Ok(stream)
    }

    pub async fn backend(&self, backend_id: &BackendName) -> sqlx::Result<Option<BackendRow>> {
        let result = sqlx::query!(
            r#"
            select
                id,
                cluster,
                last_status,
                last_status_time,
                drone_id,
                expiration_time,
                allowed_idle_seconds,
                last_keepalive,
                now() as "as_of!"
            from backend
            where id = $1
            "#,
            backend_id.to_string(),
        )
        .fetch_optional(&self.db.pool)
        .await?;

        let Some(result) = result else {
            return Ok(None);
        };

        Ok(Some(BackendRow {
            id: BackendName::try_from(result.id)
                .map_err(|_| sqlx::Error::Decode("Failed to decode backend name.".into()))?,
            cluster: result.cluster,
            last_status: BackendStatus::try_from(result.last_status).map_sqlx_error()?,
            last_status_time: result.last_status_time,
            last_keepalive: result.last_keepalive,
            drone_id: NodeId::from(result.drone_id),
            expiration_time: result.expiration_time,
            allowed_idle_seconds: result.allowed_idle_seconds,
            as_of: result.as_of,
        }))
    }

    pub async fn update_status(
        &self,
        backend: &BackendName,
        status: BackendStatus,
        address: Option<BackendAddr>,
        exit_code: Option<i32>,
    ) -> sqlx::Result<()> {
        let mut txn = self.db.pool.begin().await?;

        emit_with_key(&mut *txn, &backend.to_string(), &status).await?;

        sqlx::query!(
            r#"
            update backend
            set
                last_status = $2,
                last_status_time = now(),
                cluster_address = $3,
                exit_code = $4
            where id = $1
            "#,
            backend.to_string(),
            status.to_string(),
            address.map(|a| a.0.to_string()),
            exit_code,
        )
        .execute(&mut *txn)
        .await?;

        sqlx::query!(
            r#"
            insert into backend_status (backend_id, status)
            values ($1, $2)
            "#,
            backend.to_string(),
            status.to_string(),
        )
        .execute(&mut *txn)
        .await?;

        // If the backend is terminated, we can delete its associated key.
        if status == BackendStatus::Terminated {
            sqlx::query!(
                r#"
                delete from backend_key
                where id = $1
                "#,
                backend.to_string(),
            )
            .execute(&mut *txn)
            .await?;
        }

        txn.commit().await?;

        Ok(())
    }

    pub async fn list_backends(&self) -> sqlx::Result<Vec<BackendRow>> {
        let query_result = sqlx::query!(
            r#"
            select
                id,
                cluster,
                last_status,
                last_status_time,
                drone_id,
                expiration_time,
                allowed_idle_seconds,
                last_keepalive,
                now() as "as_of!"
            from backend
            "#
        )
        .fetch_all(&self.db.pool)
        .await?;

        let mut result = Vec::new();

        for row in query_result {
            result.push(BackendRow {
                id: BackendName::try_from(row.id)
                    .map_err(|_| sqlx::Error::Decode("Failed to decode backend name.".into()))?,
                cluster: row.cluster,
                last_status: BackendStatus::try_from(row.last_status).map_sqlx_error()?,
                last_status_time: row.last_status_time,
                last_keepalive: row.last_keepalive,
                drone_id: NodeId::from(row.drone_id),
                expiration_time: row.expiration_time,
                allowed_idle_seconds: row.allowed_idle_seconds,
                as_of: row.as_of,
            });
        }

        Ok(result)
    }

    pub async fn route_info_for_token(
        &self,
        token: &BearerToken,
    ) -> sqlx::Result<Option<RouteInfo>> {
        let result = sqlx::query!(
            r#"
            select
                backend_id,
                username,
                auth,
                last_status,
                cluster_address,
                secret_token
            from token
            left join backend
            on backend.id = token.backend_id
            where token = $1
            limit 1
            "#,
            token.to_string(),
        )
        .fetch_optional(&self.db.pool)
        .await?;

        let Some(result) = result else {
            return Ok(None);
        };

        let Some(address) = result.cluster_address else {
            return Ok(None);
        };

        let Ok(address) = address.parse::<SocketAddr>() else {
            tracing::warn!("Invalid cluster address: {}", address);
            return Ok(None);
        };

        Ok(Some(RouteInfo {
            backend_id: BackendName::try_from(result.backend_id)
                .map_err(|_| sqlx::Error::Decode("Failed to decode backend name.".into()))?,
            address: BackendAddr(address),
            secret_token: SecretToken::from(result.secret_token),
            user: result.username,
            user_data: Some(result.auth),
        }))
    }

    pub async fn update_keepalive(&self, backend_id: &BackendName) -> sqlx::Result<()> {
        let result = sqlx::query!(
            r#"
            update backend
            set
                last_keepalive = now()
            where id = $1
            "#,
            backend_id.to_string(),
        )
        .execute(&self.db.pool)
        .await?;

        if result.rows_affected() == 0 {
            return Err(sqlx::Error::RowNotFound);
        }

        Ok(())
    }

    pub async fn termination_candidates(
        &self,
        drone_id: NodeId,
    ) -> sqlx::Result<Vec<TerminationCandidate>> {
        let result = sqlx::query!(
            r#"
            select
                id as backend_id,
                expiration_time,
                allowed_idle_seconds,
                last_keepalive,
                now() as "as_of!"
            from backend
            where
                drone_id = $1
                and last_status != $2
                and (
                    now() - last_keepalive > make_interval(secs => allowed_idle_seconds)
                    or now() > expiration_time
                )
            "#,
            drone_id.as_i32(),
            BackendStatus::Terminated.to_string(),
        )
        .fetch_all(&self.db.pool)
        .await?;

        let mut candidates = Vec::new();
        for row in result {
            candidates.push(TerminationCandidate {
                backend_id: BackendName::try_from(row.backend_id)
                    .map_err(|_| sqlx::Error::Decode("Failed to decode backend name.".into()))?,
                expiration_time: row.expiration_time,
                last_keepalive: row.last_keepalive,
                allowed_idle_seconds: row.allowed_idle_seconds,
                as_of: row.as_of,
            });
        }

        Ok(candidates)
    }
}

#[derive(Debug, Clone)]
pub struct TerminationCandidate {
    pub backend_id: BackendName,
    pub expiration_time: Option<DateTime<Utc>>,
    pub last_keepalive: DateTime<Utc>,
    pub allowed_idle_seconds: Option<i32>,
    pub as_of: DateTime<Utc>,
}

pub struct BackendRow {
    pub id: BackendName,
    pub cluster: String,
    pub last_status: BackendStatus,
    pub last_status_time: DateTime<Utc>,
    pub last_keepalive: DateTime<Utc>,
    pub drone_id: NodeId,
    pub expiration_time: Option<DateTime<Utc>>,
    pub allowed_idle_seconds: Option<i32>,
    pub as_of: DateTime<Utc>,
}

impl BackendRow {
    /// The duration since the heartbeat, as of the time of the query.
    pub fn status_age(&self) -> chrono::Duration {
        self.as_of - self.last_status_time
    }
}
