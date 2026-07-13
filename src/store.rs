//! SeaORM persistence for customer profile and session metadata. API-key
//! lifecycle is intentionally absent: `fiducia-auth` is the only credential
//! authority and the browser receives only its sanitized metadata contract.

use fiducia_interfaces_db::customer::{CustomerPreferencesRow, CustomerSessionsRow};
use sea_orm::{
    ActiveModelTrait, ActiveValue::Set, ColumnTrait, DatabaseConnection, DbErr, EntityTrait,
    QueryFilter, QueryOrder,
};
use uuid::Uuid;

use crate::entity::{customer_preferences as prefs, customer_sessions as sess, users};

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
