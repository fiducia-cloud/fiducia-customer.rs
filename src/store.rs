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

use fiducia_interfaces_db::customer::{ApiKeysRow, CustomerPreferencesRow, CustomerSessionsRow};
use uuid::Uuid;

use crate::entity::api_keys::{ActiveModel, Column, Entity as ApiKeys, Model};
use crate::entity::sync_idempotency_keys as idem;
use sea_orm::{
    sea_query::Expr, ActiveModelTrait, ActiveValue::Set, ColumnTrait, ConnectionTrait,
    DatabaseBackend, DatabaseConnection, DbErr, EntityTrait, QueryFilter, QueryOrder, QuerySelect,
    Statement,
};

/// Fields for a new api_keys row. The secret is never stored — only its hash.
pub struct NewApiKey<'a> {
    pub key_id: &'a str,
    pub org_id: Uuid,
    pub name: &'a str,
    pub secret_hash: String,
    pub scopes: serde_json::Value,
    pub env: &'a str,
    pub require_idempotency: bool,
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
pub async fn list_api_keys(
    db: &DatabaseConnection,
    orgs: &[Uuid],
) -> Result<Vec<ApiKeysRow>, DbErr> {
    let rows = ApiKeys::find()
        .filter(Column::OrgId.is_in(orgs.iter().copied()))
        .order_by_desc(Column::CreatedAt)
        .all(db)
        .await?;
    Ok(rows.into_iter().map(Model::into_row).collect())
}

/// Insert a key under `new.org_id` and return the committed row. The primary key,
/// version, and timestamps are left unset so the DB defaults/trigger populate them.
pub async fn insert_api_key(
    db: &DatabaseConnection,
    new: NewApiKey<'_>,
) -> Result<ApiKeysRow, DbErr> {
    let model = ActiveModel {
        key_id: Set(new.key_id.to_string()),
        org_id: Set(new.org_id),
        name: Set(new.name.to_string()),
        secret_hash: Set(new.secret_hash),
        scopes: Set(new.scopes),
        env: Set(new.env.to_string()),
        require_idempotency: Set(new.require_idempotency),
        ..Default::default()
    }
    .insert(db)
    .await?;
    Ok(model.into_row())
}

/// Load one key scoped to the caller's org(s) — the org filter is what makes every
/// mutation below tenant-safe (a row in another org is simply never found).
async fn find_owned(
    conn: &DatabaseConnection,
    filter: sea_orm::Select<ApiKeys>,
    orgs: &[Uuid],
) -> Result<Option<Model>, DbErr> {
    filter
        .filter(Column::OrgId.is_in(orgs.iter().copied()))
        .one(conn)
        .await
}

/// Rotate the stored secret hash for a key, scoped to the caller's org(s).
/// Returns `None` when no row in those orgs matches the prefix.
pub async fn rotate_secret(
    db: &DatabaseConnection,
    key_id: &str,
    secret_hash: String,
    orgs: &[Uuid],
) -> Result<Option<ApiKeysRow>, DbErr> {
    let Some(model) =
        find_owned(db, ApiKeys::find().filter(Column::KeyId.eq(key_id)), orgs).await?
    else {
        return Ok(None);
    };
    let mut active: ActiveModel = model.into();
    active.secret_hash = Set(secret_hash);
    // Only secret_hash is dirty; the BEFORE UPDATE trigger bumps version + updated_at.
    let updated = active.update(db).await?;
    Ok(Some(updated.into_row()))
}

/// Soft-revoke a key by id, scoped to the caller's org(s).
pub async fn soft_delete(
    db: &DatabaseConnection,
    id: Uuid,
    orgs: &[Uuid],
) -> Result<Option<ApiKeysRow>, DbErr> {
    let Some(model) = find_owned(db, ApiKeys::find_by_id(id), orgs).await? else {
        return Ok(None);
    };
    let mut active: ActiveModel = model.into();
    active.revoked = Set(true);
    let updated = active.update(db).await?;
    Ok(Some(updated.into_row()))
}

/// Apply a sync upsert patch to a key by id, scoped to the caller's org(s). Only
/// the fields present in the patch are written (the COALESCE-equivalent); the
/// trigger bumps version on any write.
pub async fn upsert_fields(
    db: &DatabaseConnection,
    id: Uuid,
    orgs: &[Uuid],
    patch: ApiKeyPatch,
) -> Result<Option<ApiKeysRow>, DbErr> {
    let Some(model) = find_owned(db, ApiKeys::find_by_id(id), orgs).await? else {
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
    let updated = active.update(db).await?;
    Ok(Some(updated.into_row()))
}

/// Catch-up hydration: keys strictly newer than `since` (org-scoped), ordered by
/// the monotonic `version`. Backed by the `api_keys (org_id, version)` index, so
/// this is an index range scan, not a table scan. `limit` bounds one page.
pub async fn catchup_api_keys(
    db: &DatabaseConnection,
    orgs: &[Uuid],
    since: i64,
    limit: i64,
) -> Result<Vec<ApiKeysRow>, DbErr> {
    let rows = ApiKeys::find()
        .filter(Column::OrgId.is_in(orgs.iter().copied()))
        .filter(Column::Version.gt(since))
        .order_by_asc(Column::Version)
        .limit(limit.max(1) as u64)
        .all(db)
        .await?;
    Ok(rows.into_iter().map(Model::into_row).collect())
}

// ─── Durable idempotency ledger ─────────────────────────────────────────────
// The write path records the committed version it returned for each client
// Idempotency-Key in `sync_idempotency_keys`, so a retried write replays the same
// ack ACROSS RESTARTS instead of re-running the UPDATE (which would re-bump
// version). Claim-first so only the first request runs the mutation.

/// Try to claim `key`. `Ok(true)` => we own it (run the mutation); `Ok(false)` =>
/// it already existed (replay via [`idem_committed`]).
pub async fn idem_claim(db: &DatabaseConnection, key: &str) -> Result<bool, DbErr> {
    let claimed = db
        .query_one(Statement::from_sql_and_values(
            DatabaseBackend::Postgres,
            "insert into sync_idempotency_keys (key) values ($1) \
         on conflict (key) do update set created_at = excluded.created_at \
         where sync_idempotency_keys.committed_version is null \
           and sync_idempotency_keys.created_at < now() - interval '5 minutes' \
         returning 1",
            [key.to_owned().into()],
        ))
        .await?;
    Ok(claimed.is_some())
}

/// Release an unsuccessful in-flight claim. A committed replay record is never
/// deleted by this path.
pub async fn idem_release(db: &DatabaseConnection, key: &str) -> Result<(), DbErr> {
    idem::Entity::delete_many()
        .filter(idem::Column::Key.eq(key))
        .filter(idem::Column::CommittedVersion.is_null())
        .exec(db)
        .await?;
    Ok(())
}

/// The recorded outcome for `key`: `None` => no such key; `Some(None)` => claimed
/// but still in-flight; `Some(Some(v))` => committed at version `v` (replay it).
pub async fn idem_committed(
    db: &DatabaseConnection,
    key: &str,
) -> Result<Option<Option<i64>>, DbErr> {
    Ok(idem::Entity::find_by_id(key)
        .one(db)
        .await?
        .map(|row| row.committed_version))
}

/// Record the committed version for a claimed key.
pub async fn idem_record(db: &DatabaseConnection, key: &str, version: i64) -> Result<(), DbErr> {
    idem::Entity::update_many()
        .col_expr(idem::Column::CommittedVersion, Expr::value(version))
        .filter(idem::Column::Key.eq(key))
        .exec(db)
        .await?;
    Ok(())
}

// ─── users / preferences / sessions (real, DB-backed customer data) ─────────
// These back the customer Settings + Security pages. The Supabase user id (JWT
// subject) is mirrored into a local `users` row on first access so per-user
// preferences and trusted sessions join against a stable id.

use crate::entity::{customer_preferences as prefs, customer_sessions as sess, users};

/// Ensure a local `users` row exists for the authenticated Supabase user and
/// return its id (upsert on the unique `supabase_user_id`).
pub async fn ensure_user(
    db: &DatabaseConnection,
    supabase_user_id: Uuid,
    email: &str,
) -> Result<Uuid, DbErr> {
    if let Some(u) = users::Entity::find()
        .filter(users::Column::SupabaseUserId.eq(supabase_user_id))
        .one(db)
        .await?
    {
        return Ok(u.id);
    }
    let am = users::ActiveModel {
        supabase_user_id: Set(supabase_user_id),
        email: Set(email.to_string()),
        ..Default::default()
    };
    match am.insert(db).await {
        Ok(u) => Ok(u.id),
        // Lost a concurrent insert race → the unique index rejected us; re-read.
        Err(_) => users::Entity::find()
            .filter(users::Column::SupabaseUserId.eq(supabase_user_id))
            .one(db)
            .await?
            .map(|u| u.id)
            .ok_or_else(|| DbErr::RecordNotFound("users upsert lost its row".to_string())),
    }
}

/// The user's stored preferences, or `None` if they've never saved any.
pub async fn get_preferences(
    db: &DatabaseConnection,
    user_id: Uuid,
) -> Result<Option<CustomerPreferencesRow>, DbErr> {
    Ok(prefs::Entity::find_by_id(user_id)
        .one(db)
        .await?
        .map(prefs::Model::into_row))
}

/// Upsert the user's preferences and return the committed row (trigger bumps
/// version/updated_at on update).
#[allow(clippy::too_many_arguments)]
pub async fn upsert_preferences(
    db: &DatabaseConnection,
    user_id: Uuid,
    region: String,
    timezone: String,
    density: String,
    notify_key_rotation: bool,
    notify_lock_contention: bool,
    notify_mfa: bool,
) -> Result<CustomerPreferencesRow, DbErr> {
    if let Some(existing) = prefs::Entity::find_by_id(user_id).one(db).await? {
        let mut am: prefs::ActiveModel = existing.into();
        am.region = Set(region);
        am.timezone = Set(timezone);
        am.density = Set(density);
        am.notify_key_rotation = Set(notify_key_rotation);
        am.notify_lock_contention = Set(notify_lock_contention);
        am.notify_mfa = Set(notify_mfa);
        am.update(db).await.map(prefs::Model::into_row)
    } else {
        prefs::ActiveModel {
            user_id: Set(user_id),
            region: Set(region),
            timezone: Set(timezone),
            density: Set(density),
            notify_key_rotation: Set(notify_key_rotation),
            notify_lock_contention: Set(notify_lock_contention),
            notify_mfa: Set(notify_mfa),
            ..Default::default()
        }
        .insert(db)
        .await
        .map(prefs::Model::into_row)
    }
}

/// The user's trusted sessions, most-recently-seen first.
pub async fn list_sessions(
    db: &DatabaseConnection,
    user_id: Uuid,
) -> Result<Vec<CustomerSessionsRow>, DbErr> {
    Ok(sess::Entity::find()
        .filter(sess::Column::UserId.eq(user_id))
        .order_by_desc(sess::Column::LastSeen)
        .all(db)
        .await?
        .into_iter()
        .map(sess::Model::into_row)
        .collect())
}

/// Revoke a user's session by device label (soft: `status = 'revoked'`, scoped to
/// the caller). Returns `false` when no matching active session exists.
pub async fn revoke_session(
    db: &DatabaseConnection,
    user_id: Uuid,
    device: &str,
) -> Result<bool, DbErr> {
    let existing = sess::Entity::find()
        .filter(sess::Column::UserId.eq(user_id))
        .filter(sess::Column::Device.eq(device))
        .filter(sess::Column::Status.ne("revoked"))
        .one(db)
        .await?;
    let Some(model) = existing else {
        return Ok(false);
    };
    let mut am: sess::ActiveModel = model.into();
    am.status = Set("revoked".to_string());
    am.update(db).await?;
    Ok(true)
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
    use sea_orm::SqlxPostgresConnector;
    use sqlx::{postgres::PgPoolOptions, PgPool};
    use std::ops::Deref;

    const SCHEMA: &str = include_str!("../../fiducia-interfaces/sql/customer.sql");

    // Each test owns its pool (a sqlx pool is bound to the runtime that created
    // it, and every `#[tokio::test]` spins a fresh runtime — a shared pool would
    // dangle). The schema apply, however, is serialized + done once: concurrent
    // `create or replace function` from two sessions races on the pg_proc unique
    // index, so we guard it behind an async mutex + a done-flag.
    static SCHEMA_READY: tokio::sync::Mutex<bool> = tokio::sync::Mutex::const_new(false);

    struct TestDb {
        sqlx: PgPool,
        orm: DatabaseConnection,
    }

    impl Deref for TestDb {
        type Target = DatabaseConnection;

        fn deref(&self) -> &Self::Target {
            &self.orm
        }
    }

    async fn pool_or_skip() -> Option<TestDb> {
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
        let orm = SqlxPostgresConnector::from_sqlx_postgres_pool(pool.clone());
        Some(TestDb { sqlx: pool, orm })
    }

    fn uniq(prefix: &str) -> String {
        format!("{prefix}-{}", Uuid::new_v4().simple())
    }

    async fn make_org(pool: &TestDb) -> Uuid {
        let slug = uniq("org");
        sqlx::query_scalar::<_, Uuid>("insert into orgs (slug, name) values ($1, $2) returning id")
            .bind(&slug)
            .bind(&slug)
            .fetch_one(&pool.sqlx)
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
            require_idempotency: true,
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
        assert!(only_a[0].require_idempotency);
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

    #[tokio::test]
    async fn ensure_user_upserts_and_is_idempotent() {
        let Some(pool) = pool_or_skip().await else {
            eprintln!("skip ensure_user_upserts_and_is_idempotent: no TEST_DATABASE_URL");
            return;
        };
        let sub = Uuid::new_v4();
        let a = ensure_user(&pool, sub, "a@example.com").await.unwrap();
        // Same Supabase subject → same local id (no duplicate row).
        let b = ensure_user(&pool, sub, "a@example.com").await.unwrap();
        assert_eq!(a, b);
    }

    #[tokio::test]
    async fn preferences_default_then_persist_and_bump() {
        let Some(pool) = pool_or_skip().await else {
            eprintln!("skip preferences_default_then_persist_and_bump: no TEST_DATABASE_URL");
            return;
        };
        let uid = ensure_user(&pool, Uuid::new_v4(), "p@example.com")
            .await
            .unwrap();
        assert!(
            get_preferences(&pool, uid).await.unwrap().is_none(),
            "none until saved"
        );

        let saved = upsert_preferences(
            &pool,
            uid,
            "iad".into(),
            "UTC".into(),
            "compact".into(),
            false,
            true,
            true,
        )
        .await
        .unwrap();
        assert_eq!(saved.region, "iad");
        assert_eq!(saved.density, "compact");
        assert!(!saved.notify_key_rotation);

        // Second upsert updates in place and the trigger bumps version.
        let again = upsert_preferences(
            &pool,
            uid,
            "sfo".into(),
            "UTC".into(),
            "comfortable".into(),
            true,
            true,
            true,
        )
        .await
        .unwrap();
        assert_eq!(again.region, "sfo");
        assert_eq!(again.version, saved.version + 1);
        assert_eq!(
            get_preferences(&pool, uid).await.unwrap().unwrap().region,
            "sfo"
        );
    }

    #[tokio::test]
    async fn sessions_list_and_revoke_are_user_scoped() {
        let Some(pool) = pool_or_skip().await else {
            eprintln!("skip sessions_list_and_revoke_are_user_scoped: no TEST_DATABASE_URL");
            return;
        };
        let mine = ensure_user(&pool, Uuid::new_v4(), "me@example.com")
            .await
            .unwrap();
        let other = ensure_user(&pool, Uuid::new_v4(), "other@example.com")
            .await
            .unwrap();
        let device = uniq("MacBook");
        // Seed a session for each user (login flow creates these elsewhere).
        for uid in [mine, other] {
            sqlx::query("insert into customer_sessions (user_id, device) values ($1, $2)")
                .bind(uid)
                .bind(&device)
                .execute(&pool.sqlx)
                .await
                .unwrap();
        }

        // Listing is scoped to the caller.
        assert_eq!(list_sessions(&pool, mine).await.unwrap().len(), 1);

        // Revoking my session works; a repeat is a no-op (already revoked).
        assert!(revoke_session(&pool, mine, &device).await.unwrap());
        assert!(!revoke_session(&pool, mine, &device).await.unwrap());
        let after = list_sessions(&pool, mine).await.unwrap();
        assert_eq!(after[0].status, "revoked");

        // The other user's identically-named session is untouched.
        assert_eq!(
            list_sessions(&pool, other).await.unwrap()[0].status,
            "active"
        );
    }
}
