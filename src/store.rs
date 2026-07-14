//! SeaORM persistence for customer profile and session metadata. API-key
//! lifecycle is intentionally absent: `fiducia-auth` is the only credential
//! authority and the browser receives only its sanitized metadata contract.

use fiducia_interfaces_db::customer::{CustomerPreferencesRow, CustomerSessionsRow};
use sea_orm::{
    ActiveModelTrait, ActiveValue::Set, ColumnTrait, DatabaseConnection, DbErr, EntityTrait,
    PaginatorTrait, QueryFilter, QueryOrder, QuerySelect,
};
use uuid::Uuid;

use crate::entity::{
    audit_log, customer_notifications as notif, customer_preferences as prefs,
    customer_sessions as sess, users,
};

/// Ensure a local `users` row exists for the authenticated Supabase user and
/// return its id. Supabase remains the source of truth for identity.
pub async fn ensure_user(
    db: &DatabaseConnection,
    supabase_user_id: Uuid,
    email: &str,
) -> Result<Uuid, DbErr> {
    if let Some(user) = users::Entity::find()
        .filter(users::Column::SupabaseUserId.eq(supabase_user_id))
        .one(db)
        .await?
    {
        return Ok(user.id);
    }

    let model = users::ActiveModel {
        supabase_user_id: Set(supabase_user_id),
        email: Set(email.to_string()),
        ..Default::default()
    };
    match model.insert(db).await {
        Ok(user) => Ok(user.id),
        // A concurrent request may have won the unique-key insert race.
        Err(_) => users::Entity::find()
            .filter(users::Column::SupabaseUserId.eq(supabase_user_id))
            .one(db)
            .await?
            .map(|user| user.id)
            .ok_or_else(|| DbErr::RecordNotFound("users row missing after insert race".into())),
    }
}

pub async fn get_preferences(
    db: &DatabaseConnection,
    user_id: Uuid,
) -> Result<Option<CustomerPreferencesRow>, DbErr> {
    Ok(prefs::Entity::find_by_id(user_id)
        .one(db)
        .await?
        .map(prefs::Model::into_row))
}

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
        let mut model: prefs::ActiveModel = existing.into();
        model.region = Set(region);
        model.timezone = Set(timezone);
        model.density = Set(density);
        model.notify_key_rotation = Set(notify_key_rotation);
        model.notify_lock_contention = Set(notify_lock_contention);
        model.notify_mfa = Set(notify_mfa);
        model.update(db).await.map(prefs::Model::into_row)
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

/// Read a bounded, organization-scoped activity feed. The caller has already
/// established membership from the verified Supabase session; this query adds
/// the database-side org predicate so one tenant can never read another's log.
pub async fn list_audit_events(
    db: &DatabaseConnection,
    org_id: Uuid,
    limit: u64,
) -> Result<Vec<audit_log::Model>, DbErr> {
    audit_log::Entity::find()
        .filter(audit_log::Column::OrgId.eq(org_id))
        .order_by_desc(audit_log::Column::CreatedAt)
        .limit(limit)
        .all(db)
        .await
}

/// Revoke a user's locally observed session record by device label (soft:
/// `status = 'revoked'`, scoped to the caller's user id so one user can never
/// touch another user's rows). Returns `false` when no matching active session
/// exists. This marks the audit record only — provider (Supabase) token
/// revocation is a separate, not-yet-wired concern.
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
    let mut active: sess::ActiveModel = model.into();
    active.status = Set("revoked".to_string());
    active.update(db).await?;
    Ok(true)
}

/// The signed-in user's most recent notifications, newest first. Bounded by
/// `limit` and always scoped to `user_id` at the database, so one user can
/// never read another's feed even if a caller passes a foreign id.
pub async fn list_notifications(
    db: &DatabaseConnection,
    user_id: Uuid,
    limit: u64,
) -> Result<Vec<notif::Model>, DbErr> {
    notif::Entity::find()
        .filter(notif::Column::UserId.eq(user_id))
        .order_by_desc(notif::Column::CreatedAt)
        .limit(limit)
        .all(db)
        .await
}

/// Count the user's unread notifications (for the nav badge).
pub async fn unread_notification_count(
    db: &DatabaseConnection,
    user_id: Uuid,
) -> Result<u64, DbErr> {
    notif::Entity::find()
        .filter(notif::Column::UserId.eq(user_id))
        .filter(notif::Column::ReadAt.is_null())
        .count(db)
        .await
}

/// Mark one notification read, scoped to the owner. Returns `false` when no
/// matching unread row exists (already read, or not this user's). The BEFORE
/// UPDATE trigger bumps `version`/`updated_at`/`sync_sequence`, so the change
/// propagates through the sync catch-up cursor like any other row edit.
pub async fn mark_notification_read(
    db: &DatabaseConnection,
    user_id: Uuid,
    id: Uuid,
) -> Result<bool, DbErr> {
    let Some(model) = notif::Entity::find_by_id(id)
        .filter(notif::Column::UserId.eq(user_id))
        .filter(notif::Column::ReadAt.is_null())
        .one(db)
        .await?
    else {
        return Ok(false);
    };
    let mut active: notif::ActiveModel = model.into();
    active.read_at = Set(Some(sea_orm::prelude::DateTimeWithTimeZone::from(
        chrono_now(),
    )));
    active.update(db).await?;
    Ok(true)
}

/// Deliver a notification to a user. Server-authoritative: callers are trusted
/// internal code paths (key-rotation reminders, contention alerts), never the
/// browser. `sync_sequence` is assigned by the trigger.
#[allow(clippy::too_many_arguments)]
pub async fn create_notification(
    db: &DatabaseConnection,
    user_id: Uuid,
    org_id: Option<Uuid>,
    kind: &str,
    severity: &str,
    title: &str,
    body: &str,
    link: Option<&str>,
) -> Result<notif::Model, DbErr> {
    notif::ActiveModel {
        user_id: Set(user_id),
        org_id: Set(org_id),
        kind: Set(kind.to_string()),
        severity: Set(severity.to_string()),
        title: Set(title.to_string()),
        body: Set(body.to_string()),
        link: Set(link.map(str::to_string)),
        ..Default::default()
    }
    .insert(db)
    .await
}

/// `now()` as a fixed-offset timestamp. Kept in one place so tests and prod
/// agree on the type SeaORM expects for `timestamptz` columns.
fn chrono_now() -> sea_orm::prelude::DateTimeWithTimeZone {
    sea_orm::prelude::DateTimeWithTimeZone::from(std::time::SystemTime::now())
}

#[cfg(test)]
mod tests {
    use super::*;
    use sea_orm::{ConnectionTrait, Database};

    const SCHEMA: &str = include_str!("../../fiducia-interfaces/sql/customer.sql");
    static SCHEMA_READY: tokio::sync::Mutex<bool> = tokio::sync::Mutex::const_new(false);

    async fn db_or_skip() -> Option<DatabaseConnection> {
        let url = std::env::var("TEST_DATABASE_URL")
            .ok()
            .filter(|value| !value.is_empty())?;
        let db = Database::connect(&url)
            .await
            .expect("connect TEST_DATABASE_URL with SeaORM");
        {
            let mut ready = SCHEMA_READY.lock().await;
            if !*ready {
                db.execute_unprepared(SCHEMA)
                    .await
                    .expect("apply customer.sql with SeaORM");
                *ready = true;
            }
        }
        Some(db)
    }

    fn unique_device() -> String {
        format!("MacBook-{}", Uuid::new_v4().simple())
    }

    #[tokio::test]
    async fn ensure_user_is_idempotent() {
        let Some(db) = db_or_skip().await else {
            eprintln!("skip ensure_user_is_idempotent: TEST_DATABASE_URL unset");
            return;
        };
        let subject = Uuid::new_v4();
        let first = ensure_user(&db, subject, "a@example.com").await.unwrap();
        let second = ensure_user(&db, subject, "a@example.com").await.unwrap();
        assert_eq!(first, second);
    }

    #[tokio::test]
    async fn preferences_persist_and_bump_version() {
        let Some(db) = db_or_skip().await else {
            eprintln!("skip preferences_persist_and_bump_version: TEST_DATABASE_URL unset");
            return;
        };
        let user_id = ensure_user(&db, Uuid::new_v4(), "p@example.com")
            .await
            .unwrap();
        assert!(get_preferences(&db, user_id).await.unwrap().is_none());

        let saved = upsert_preferences(
            &db,
            user_id,
            "iad".into(),
            "UTC".into(),
            "compact".into(),
            false,
            true,
            true,
        )
        .await
        .unwrap();
        let updated = upsert_preferences(
            &db,
            user_id,
            "sfo".into(),
            "UTC".into(),
            "comfortable".into(),
            true,
            true,
            true,
        )
        .await
        .unwrap();
        assert_eq!(updated.region, "sfo");
        assert_eq!(updated.version, saved.version + 1);
    }

    #[tokio::test]
    async fn sessions_are_user_scoped() {
        let Some(db) = db_or_skip().await else {
            eprintln!("skip sessions_are_user_scoped: TEST_DATABASE_URL unset");
            return;
        };
        let mine = ensure_user(&db, Uuid::new_v4(), "me@example.com")
            .await
            .unwrap();
        let other = ensure_user(&db, Uuid::new_v4(), "other@example.com")
            .await
            .unwrap();
        let device = unique_device();
        for user_id in [mine, other] {
            sess::ActiveModel {
                user_id: Set(user_id),
                device: Set(device.clone()),
                ..Default::default()
            }
            .insert(&db)
            .await
            .unwrap();
        }

        let mine_rows = list_sessions(&db, mine).await.unwrap();
        assert_eq!(mine_rows.len(), 1);
        assert_eq!(mine_rows[0].user_id, mine);
        assert_eq!(list_sessions(&db, other).await.unwrap()[0].user_id, other);

        assert!(revoke_session(&db, mine, &device).await.unwrap());
        assert!(!revoke_session(&db, mine, &device).await.unwrap());
        assert_eq!(list_sessions(&db, mine).await.unwrap()[0].status, "revoked");
        assert_eq!(list_sessions(&db, other).await.unwrap()[0].status, "active");
    }
}
