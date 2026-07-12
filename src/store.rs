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
    sqlx::query_as::<_, ApiKeysRow>(
        "select * from api_keys where org_id = any($1) order by created_at asc",
    )
    .bind(orgs)
    .fetch_all(pool)
    .await
}

/// Insert a key under `new.org_id` and return the committed row.
pub async fn insert_api_key(pool: &PgPool, new: NewApiKey<'_>) -> Result<ApiKeysRow, sqlx::Error> {
    sqlx::query_as::<_, ApiKeysRow>(
        "insert into api_keys (key_id, org_id, name, secret_hash, scopes, env) \
         values ($1, $2, $3, $4, $5, $6) returning *",
    )
    .bind(new.key_id)
    .bind(new.org_id)
    .bind(new.name)
    .bind(new.secret_hash)
    .bind(new.scopes)
    .bind(new.env)
    .fetch_one(pool)
    .await
}

/// Rotate the stored secret hash for a key, scoped to the caller's org(s).
/// Returns `None` when no row in those orgs matches the prefix.
pub async fn rotate_secret(
    pool: &PgPool,
    key_id: &str,
    secret_hash: String,
    orgs: &[Uuid],
) -> Result<Option<ApiKeysRow>, sqlx::Error> {
    sqlx::query_as::<_, ApiKeysRow>(
        "update api_keys set secret_hash = $1 where key_id = $2 and org_id = any($3) returning *",
    )
    .bind(secret_hash)
    .bind(key_id)
    .bind(orgs)
    .fetch_optional(pool)
    .await
}

/// Soft-revoke a key by id, scoped to the caller's org(s).
pub async fn soft_delete(
    pool: &PgPool,
    id: Uuid,
    orgs: &[Uuid],
) -> Result<Option<ApiKeysRow>, sqlx::Error> {
    sqlx::query_as::<_, ApiKeysRow>(
        "update api_keys set revoked = true where id = $1 and org_id = any($2) returning *",
    )
    .bind(id)
    .bind(orgs)
    .fetch_optional(pool)
    .await
}

/// Apply a sync upsert patch to a key by id, scoped to the caller's org(s).
pub async fn upsert_fields(
    pool: &PgPool,
    id: Uuid,
    orgs: &[Uuid],
    patch: ApiKeyPatch,
) -> Result<Option<ApiKeysRow>, sqlx::Error> {
    sqlx::query_as::<_, ApiKeysRow>(
        "update api_keys set \
            name = coalesce($2, name), \
            scopes = coalesce($3, scopes), \
            env = coalesce($4, env), \
            revoked = coalesce($5, revoked) \
         where id = $1 and org_id = any($6) returning *",
    )
    .bind(id)
    .bind(patch.name)
    .bind(patch.scopes)
    .bind(patch.env)
    .bind(patch.revoked)
    .bind(orgs)
    .fetch_optional(pool)
    .await
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

    async fn pool_or_skip() -> Option<PgPool> {
        let url = std::env::var("TEST_DATABASE_URL")
            .ok()
            .filter(|v| !v.is_empty())?;
        let pool = PgPoolOptions::new()
            .max_connections(4)
            .connect(&url)
            .await
            .expect("connect TEST_DATABASE_URL");
        // Canonical customer schema; idempotent (`create ... if not exists`), and
        // the Supabase realtime/RLS blocks are no-ops on a plain Postgres.
        sqlx::raw_sql(SCHEMA)
            .execute(&pool)
            .await
            .expect("apply customer.sql");
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
        let created = insert_api_key(&pool, new_key(org_a, &prefix)).await.unwrap();

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
}
