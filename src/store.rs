//! Customer `api_keys` data access — the single seam between the HTTP handlers
//! and Postgres. Every operation is **org-scoped**: reads and mutations only ever
//! touch rows in the caller's org(s), so a customer can never see or change
//! another tenant's keys.
//!
//! This module is the abstraction boundary the storage backend lives behind. It
//! returns the shared [`ApiKeysRow`] contract type regardless of implementation,
//! so handlers, broadcast, and tests are decoupled from the query engine. The
//! DB-behavior tests in `tests/api_keys_store.rs` pin this seam's semantics so an
//! engine swap (raw SQL → ORM) is provably behaviour-preserving.

use fiducia_interfaces_db::customer::ApiKeysRow;
use sqlx::PgPool;
use uuid::Uuid;

use crate::entity::api_keys::{ActiveModel, Column, Entity as ApiKeys, Model};
use sea_orm::{
    ActiveModelTrait, ActiveValue::Set, ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter,
    QueryOrder, SqlxPostgresConnector,
};

/// Wrap the shared sqlx pool as a SeaORM connection. The api_keys access below is
/// expressed through the ORM; the pool (and its lifecycle) stays owned by the app.
fn orm(pool: &PgPool) -> DatabaseConnection {
    SqlxPostgresConnector::from_sqlx_postgres_pool(pool.clone())
}

/// The seam's error type is stable across engines: ORM errors are surfaced as an
/// opaque `sqlx::Error` so handlers/tests need no changes when the engine swaps.
fn map_err(e: sea_orm::DbErr) -> sqlx::Error {
    sqlx::Error::Protocol(e.to_string())
}

/// Fields for a new api_keys row. The secret is never stored — only its hash.
pub struct NewApiKey<'a> {
    pub key_id: &'a str,
    pub org_id: Uuid,
    pub name: &'a str,
    pub secret_hash: String,
    pub scopes: serde_json::Value,
    pub env: &'a str,
}

/// Patch for a sync upsert. `None` leaves a column untouched (COALESCE).
#[derive(Default)]
pub struct ApiKeyPatch {
    pub name: Option<String>,
    pub scopes: Option<serde_json::Value>,
    pub env: Option<String>,
    pub revoked: Option<bool>,
}

/// List the caller's api keys (org-scoped), newest first.
pub async fn list_api_keys(pool: &PgPool, orgs: &[Uuid]) -> Result<Vec<ApiKeysRow>, sqlx::Error> {
    let rows = ApiKeys::find()
        .filter(Column::OrgId.is_in(orgs.iter().copied()))
        .order_by_asc(Column::CreatedAt)
        .all(&orm(pool))
        .await
        .map_err(map_err)?;
    Ok(rows.into_iter().map(Model::into_row).collect())
}

/// Insert a key under `new.org_id` and return the committed row. The primary key,
/// version, and timestamps are left unset so the DB defaults/trigger populate them.
pub async fn insert_api_key(pool: &PgPool, new: NewApiKey<'_>) -> Result<ApiKeysRow, sqlx::Error> {
    let model = ActiveModel {
        key_id: Set(new.key_id.to_string()),
        org_id: Set(new.org_id),
        name: Set(new.name.to_string()),
        secret_hash: Set(new.secret_hash),
        scopes: Set(new.scopes),
        env: Set(new.env.to_string()),
        ..Default::default()
    }
    .insert(&orm(pool))
    .await
    .map_err(map_err)?;
    Ok(model.into_row())
}

/// Load one key scoped to the caller's org(s) — the org filter is what makes every
/// mutation below tenant-safe (a row in another org is simply never found).
async fn find_owned(
    conn: &DatabaseConnection,
    filter: sea_orm::Select<ApiKeys>,
    orgs: &[Uuid],
) -> Result<Option<Model>, sqlx::Error> {
    filter
        .filter(Column::OrgId.is_in(orgs.iter().copied()))
        .one(conn)
        .await
        .map_err(map_err)
}

/// Rotate the stored secret hash for a key, scoped to the caller's org(s).
/// Returns `None` when no row in those orgs matches the prefix.
pub async fn rotate_secret(
    pool: &PgPool,
    key_id: &str,
    secret_hash: String,
    orgs: &[Uuid],
) -> Result<Option<ApiKeysRow>, sqlx::Error> {
    let conn = orm(pool);
    let Some(model) = find_owned(
        &conn,
        ApiKeys::find().filter(Column::KeyId.eq(key_id)),
        orgs,
    )
    .await?
    else {
        return Ok(None);
    };
    let mut active: ActiveModel = model.into();
    active.secret_hash = Set(secret_hash);
    // Only secret_hash is dirty; the BEFORE UPDATE trigger bumps version + updated_at.
    let updated = active.update(&conn).await.map_err(map_err)?;
    Ok(Some(updated.into_row()))
}

/// Soft-revoke a key by id, scoped to the caller's org(s).
pub async fn soft_delete(
    pool: &PgPool,
    id: Uuid,
    orgs: &[Uuid],
) -> Result<Option<ApiKeysRow>, sqlx::Error> {
    let conn = orm(pool);
    let Some(model) = find_owned(&conn, ApiKeys::find_by_id(id), orgs).await? else {
        return Ok(None);
    };
    let mut active: ActiveModel = model.into();
    active.revoked = Set(true);
    let updated = active.update(&conn).await.map_err(map_err)?;
    Ok(Some(updated.into_row()))
}

/// Apply a sync upsert patch to a key by id, scoped to the caller's org(s). Only
/// the fields present in the patch are written (the COALESCE-equivalent); the
/// trigger bumps version on any write.
pub async fn upsert_fields(
    pool: &PgPool,
    id: Uuid,
    orgs: &[Uuid],
    patch: ApiKeyPatch,
) -> Result<Option<ApiKeysRow>, sqlx::Error> {
    let conn = orm(pool);
    let Some(model) = find_owned(&conn, ApiKeys::find_by_id(id), orgs).await? else {
        return Ok(None);
    };
    // An empty patch is a no-op (SeaORM would reject an update with no dirty
    // columns); return the current row unchanged rather than error.
    if patch.name.is_none()
        && patch.scopes.is_none()
        && patch.env.is_none()
        && patch.revoked.is_none()
    {
        return Ok(Some(model.into_row()));
    }
    let mut active: ActiveModel = model.into();
    if let Some(name) = patch.name {
        active.name = Set(name);
    }
    if let Some(scopes) = patch.scopes {
        active.scopes = Set(scopes);
    }
    if let Some(env) = patch.env {
        active.env = Set(env);
    }
    if let Some(revoked) = patch.revoked {
        active.revoked = Set(revoked);
    }
    let updated = active.update(&conn).await.map_err(map_err)?;
    Ok(Some(updated.into_row()))
}

/// Catch-up hydration: keys strictly newer than `since` (org-scoped), ordered by
/// the monotonic `version`. Backed by the `api_keys (org_id, version)` index, so
/// this is an index range scan, not a table scan. `limit` bounds one page.
pub async fn catchup_api_keys(
    pool: &PgPool,
    orgs: &[Uuid],
    since: i64,
    limit: i64,
) -> Result<Vec<ApiKeysRow>, sqlx::Error> {
    sqlx::query_as::<_, ApiKeysRow>(
        "select * from api_keys \
         where org_id = any($1) and version > $2 \
         order by version asc limit $3",
    )
    .bind(orgs)
    .bind(since)
    .bind(limit)
    .fetch_all(pool)
    .await
}

// ─── Durable idempotency ledger ─────────────────────────────────────────────
// The write path records the committed version it returned for each client
// Idempotency-Key in `sync_idempotency_keys`, so a retried write replays the same
// ack ACROSS RESTARTS instead of re-running the UPDATE (which would re-bump
// version). Claim-first so only the first request runs the mutation.

/// Try to claim `key`. `Ok(true)` => we own it (run the mutation); `Ok(false)` =>
/// it already existed (replay via [`idem_committed`]).
pub async fn idem_claim(pool: &PgPool, key: &str) -> Result<bool, sqlx::Error> {
    let claimed = sqlx::query_scalar::<_, i32>(
        "insert into sync_idempotency_keys (key) values ($1) \
         on conflict (key) do nothing returning 1",
    )
    .bind(key)
    .fetch_optional(pool)
    .await?;
    Ok(claimed.is_some())
}

/// The recorded outcome for `key`: `None` => no such key; `Some(None)` => claimed
/// but still in-flight; `Some(Some(v))` => committed at version `v` (replay it).
pub async fn idem_committed(pool: &PgPool, key: &str) -> Result<Option<Option<i64>>, sqlx::Error> {
    sqlx::query_scalar::<_, Option<i64>>(
        "select committed_version from sync_idempotency_keys where key = $1",
    )
    .bind(key)
    .fetch_optional(pool)
    .await
}

/// Record the committed version for a claimed key.
pub async fn idem_record(pool: &PgPool, key: &str, version: i64) -> Result<(), sqlx::Error> {
    sqlx::query("update sync_idempotency_keys set committed_version = $2 where key = $1")
        .bind(key)
        .bind(version)
        .execute(pool)
        .await?;
    Ok(())
}

// ─── DB behavior tests ──────────────────────────────────────────────────────
//
// These pin the org-isolation + versioning semantics of the seam against a REAL
// Postgres, so the storage-engine migration (raw SQL → ORM) is provably
// behaviour-preserving: the identical suite runs before and after the swap.
//
// Gated on `TEST_DATABASE_URL` (a Postgres the harness may create/drop freely);
// unset → the tests skip with a note, so `cargo test` stays green with no DB.
#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::postgres::PgPoolOptions;

    const SCHEMA: &str = include_str!("../../fiducia-interfaces/sql/customer.sql");

    // Each test owns its pool (a sqlx pool is bound to the runtime that created
    // it, and every `#[tokio::test]` spins a fresh runtime — a shared pool would
    // dangle). The schema apply, however, is serialized + done once: concurrent
    // `create or replace function` from two sessions races on the pg_proc unique
    // index, so we guard it behind an async mutex + a done-flag.
    static SCHEMA_READY: tokio::sync::Mutex<bool> = tokio::sync::Mutex::const_new(false);

    async fn pool_or_skip() -> Option<PgPool> {
        let url = std::env::var("TEST_DATABASE_URL")
            .ok()
            .filter(|v| !v.is_empty())?;
        let pool = PgPoolOptions::new()
            .max_connections(4)
            .connect(&url)
            .await
            .expect("connect TEST_DATABASE_URL");
        {
            let mut ready = SCHEMA_READY.lock().await;
            if !*ready {
                // Canonical customer schema; idempotent, and the Supabase
                // realtime/RLS blocks are no-ops on a plain Postgres.
                sqlx::raw_sql(SCHEMA)
                    .execute(&pool)
                    .await
                    .expect("apply customer.sql");
                *ready = true;
            }
        }
        Some(pool)
    }

    fn uniq(prefix: &str) -> String {
        format!("{prefix}-{}", Uuid::new_v4().simple())
    }

    async fn make_org(pool: &PgPool) -> Uuid {
        let slug = uniq("org");
        sqlx::query_scalar::<_, Uuid>("insert into orgs (slug, name) values ($1, $2) returning id")
            .bind(&slug)
            .bind(&slug)
            .fetch_one(pool)
            .await
            .expect("insert org")
    }

    fn new_key(org: Uuid, key_id: &str) -> NewApiKey<'_> {
        NewApiKey {
            key_id,
            org_id: org,
            name: "test key",
            secret_hash: "sha256:deadbeef".to_string(),
            scopes: serde_json::json!(["requests:write"]),
            env: "live",
        }
    }

    #[tokio::test]
    async fn insert_and_list_are_org_scoped() {
        let Some(pool) = pool_or_skip().await else {
            eprintln!("skip insert_and_list_are_org_scoped: TEST_DATABASE_URL unset");
            return;
        };
        let (org_a, org_b) = (make_org(&pool).await, make_org(&pool).await);
        let ka = uniq("fid_live");
        let kb = uniq("fid_live");
        insert_api_key(&pool, new_key(org_a, &ka)).await.unwrap();
        insert_api_key(&pool, new_key(org_b, &kb)).await.unwrap();

        let only_a = list_api_keys(&pool, &[org_a]).await.unwrap();
        assert_eq!(only_a.len(), 1, "org A sees exactly its own key");
        assert_eq!(only_a[0].key_id, ka);
        assert!(only_a.iter().all(|r| r.org_id == org_a));

        let only_b = list_api_keys(&pool, &[org_b]).await.unwrap();
        assert_eq!(only_b.len(), 1);
        assert_eq!(only_b[0].key_id, kb);

        let both = list_api_keys(&pool, &[org_a, org_b]).await.unwrap();
        assert_eq!(both.len(), 2);
    }

    #[tokio::test]
    async fn rotate_is_org_scoped() {
        let Some(pool) = pool_or_skip().await else {
            eprintln!("skip rotate_is_org_scoped: TEST_DATABASE_URL unset");
            return;
        };
        let (org_a, org_b) = (make_org(&pool).await, make_org(&pool).await);
        let prefix = uniq("fid_live");
        let created = insert_api_key(&pool, new_key(org_a, &prefix))
            .await
            .unwrap();

        // Another org cannot rotate org A's key.
        let cross = rotate_secret(&pool, &prefix, "sha256:new".into(), &[org_b])
            .await
            .unwrap();
        assert!(cross.is_none(), "cross-org rotate must not match any row");

        // The owning org can, and the row version bumps.
        let rotated = rotate_secret(&pool, &prefix, "sha256:new".into(), &[org_a])
            .await
            .unwrap()
            .expect("owner rotate returns the row");
        assert_eq!(rotated.secret_hash, "sha256:new");
        assert_eq!(rotated.version, created.version + 1);
    }

    #[tokio::test]
    async fn soft_delete_and_upsert_are_org_scoped() {
        let Some(pool) = pool_or_skip().await else {
            eprintln!("skip soft_delete_and_upsert_are_org_scoped: TEST_DATABASE_URL unset");
            return;
        };
        let (org_a, org_b) = (make_org(&pool).await, make_org(&pool).await);
        let created = insert_api_key(&pool, new_key(org_a, &uniq("fid_live")))
            .await
            .unwrap();

        // Cross-org upsert is a no-op.
        let cross = upsert_fields(
            &pool,
            created.id,
            &[org_b],
            ApiKeyPatch {
                name: Some("hijacked".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert!(cross.is_none(), "cross-org upsert must not match");

        // Owner upsert applies the patch, coalesces omitted fields, bumps version.
        let patched = upsert_fields(
            &pool,
            created.id,
            &[org_a],
            ApiKeyPatch {
                name: Some("renamed".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap()
        .expect("owner upsert returns the row");
        assert_eq!(patched.name, "renamed");
        assert_eq!(patched.env, created.env, "omitted field preserved");
        assert_eq!(patched.version, created.version + 1);

        // Cross-org soft delete is a no-op; owner soft delete revokes.
        assert!(soft_delete(&pool, created.id, &[org_b])
            .await
            .unwrap()
            .is_none());
        let deleted = soft_delete(&pool, created.id, &[org_a])
            .await
            .unwrap()
            .expect("owner soft delete returns the row");
        assert!(deleted.revoked);
    }

    #[tokio::test]
    async fn catchup_returns_only_rows_newer_than_the_cursor() {
        let Some(pool) = pool_or_skip().await else {
            eprintln!(
                "skip catchup_returns_only_rows_newer_than_the_cursor: TEST_DATABASE_URL unset"
            );
            return;
        };
        let org = make_org(&pool).await;
        // Two keys at version 1; bump one to v2 so the cursor can separate them.
        let a = insert_api_key(&pool, new_key(org, &uniq("fid_live")))
            .await
            .unwrap();
        let _b = insert_api_key(&pool, new_key(org, &uniq("fid_live")))
            .await
            .unwrap();
        upsert_fields(
            &pool,
            a.id,
            &[org],
            ApiKeyPatch {
                name: Some("bumped".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        // since=0 sees both; since=1 sees only the bumped (v2) row.
        let all = catchup_api_keys(&pool, &[org], 0, 500).await.unwrap();
        assert_eq!(all.len(), 2);
        assert!(
            all.windows(2).all(|w| w[0].version <= w[1].version),
            "ordered by version"
        );
        let newer = catchup_api_keys(&pool, &[org], 1, 500).await.unwrap();
        assert_eq!(newer.len(), 1);
        assert_eq!(newer[0].id, a.id);
        assert!(newer[0].version > 1);

        // Org-scoped: another org's cursor sees nothing here.
        let other = make_org(&pool).await;
        assert!(catchup_api_keys(&pool, &[other], 0, 500)
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn durable_idempotency_claim_then_replay() {
        let Some(pool) = pool_or_skip().await else {
            eprintln!("skip durable_idempotency_claim_then_replay: TEST_DATABASE_URL unset");
            return;
        };
        let key = uniq("api_keys:k1:upsert:7");

        // First claim wins; nothing committed yet (in-flight).
        assert!(
            idem_claim(&pool, &key).await.unwrap(),
            "first claim owns the key"
        );
        assert_eq!(
            idem_committed(&pool, &key).await.unwrap(),
            Some(None),
            "claimed, in-flight"
        );

        // A concurrent claim loses.
        assert!(
            !idem_claim(&pool, &key).await.unwrap(),
            "second claim is refused"
        );

        // Record the committed version; now every future lookup replays it —
        // this is what survives a process restart (unlike the in-process cache).
        idem_record(&pool, &key, 8).await.unwrap();
        assert_eq!(idem_committed(&pool, &key).await.unwrap(), Some(Some(8)));
        assert!(
            !idem_claim(&pool, &key).await.unwrap(),
            "committed key never re-claims"
        );

        // An unknown key has no record.
        assert_eq!(idem_committed(&pool, "never-seen").await.unwrap(), None);
    }
}
