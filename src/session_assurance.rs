//! Local assurance binding for customer Shared Auth browser sessions.
//!
//! Shared Auth proves the JWT and customer role. Supabase proves the upstream
//! identity and MFA factor. This module records the application-specific join:
//! a Shared Auth `sid` is accepted as a browser session only after this customer
//! application completed the provider MFA flow and bound that exact SID to the
//! local customer user with a bounded expiry.
//!
//! The SQL migration lives in `fiducia-interfaces`; this module deliberately
//! uses a narrow raw query instead of extending the generated public row type.
//! The assurance columns are security metadata, not part of the client sync
//! contract or the Security-page session DTO.

use sea_orm::prelude::DateTimeWithTimeZone;
use sea_orm::{
    ConnectionTrait, DatabaseBackend, DatabaseConnection, DbErr, QueryResult, Statement,
};
use uuid::Uuid;

pub const CUSTOMER_PROVIDER_PROJECT: &str = "fiducia-customer";
const MAX_DEVICE_CHARS: usize = 200;
const MAX_LOCATION_CHARS: usize = 200;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerifiedCustomerSession {
    pub id: Uuid,
    pub user_id: Uuid,
    pub shared_auth_session_id: Uuid,
    pub assurance_verified_at: DateTimeWithTimeZone,
    pub expires_at: DateTimeWithTimeZone,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct NormalizedBinding {
    device: String,
    location: Option<String>,
}

fn invalid(message: impl Into<String>) -> DbErr {
    DbErr::Custom(message.into())
}

fn bounded_label(value: &str, maximum: usize, field: &str) -> Result<String, DbErr> {
    let value = value.trim();
    if value.is_empty() || value.chars().count() > maximum || value.chars().any(char::is_control) {
        return Err(invalid(format!("invalid {field}")));
    }
    Ok(value.to_string())
}

fn normalize_binding(
    device: &str,
    location: Option<&str>,
    assurance_verified_at: DateTimeWithTimeZone,
    expires_at: DateTimeWithTimeZone,
) -> Result<NormalizedBinding, DbErr> {
    if expires_at <= assurance_verified_at {
        return Err(invalid(
            "customer Shared Auth session expiry must follow MFA verification",
        ));
    }
    let device = bounded_label(device, MAX_DEVICE_CHARS, "session device")?;
    let location = match location.map(str::trim).filter(|value| !value.is_empty()) {
        Some(value) => Some(bounded_label(
            value,
            MAX_LOCATION_CHARS,
            "session location",
        )?),
        None => None,
    };
    Ok(NormalizedBinding { device, location })
}

fn row_to_verified(row: &QueryResult) -> Result<VerifiedCustomerSession, DbErr> {
    Ok(VerifiedCustomerSession {
        id: row.try_get("", "id")?,
        user_id: row.try_get("", "user_id")?,
        shared_auth_session_id: row.try_get("", "shared_auth_session_id")?,
        assurance_verified_at: row.try_get("", "assurance_verified_at")?,
        expires_at: row.try_get("", "expires_at")?,
    })
}

/// Bind one newly issued Shared Auth SID to the local customer user after the
/// provider MFA flow completed. The database's sync/version trigger supplies
/// `sync_sequence`, `version`, and `updated_at`; callers cannot forge them.
///
/// A duplicate SID fails through the migration's unique index. It is never
/// silently reassigned to another local user.
pub async fn bind_verified_session(
    db: &DatabaseConnection,
    user_id: Uuid,
    shared_auth_session_id: Uuid,
    device: &str,
    location: Option<&str>,
    assurance_verified_at: DateTimeWithTimeZone,
    expires_at: DateTimeWithTimeZone,
) -> Result<VerifiedCustomerSession, DbErr> {
    let normalized = normalize_binding(
        device,
        location,
        assurance_verified_at,
        expires_at,
    )?;
    let row = db
        .query_one(Statement::from_sql_and_values(
            DatabaseBackend::Postgres,
            r#"
            insert into customer_sessions (
              user_id,
              device,
              location,
              status,
              shared_auth_session_id,
              provider_project,
              assurance_level,
              assurance_verified_at,
              expires_at
            ) values ($1, $2, $3, 'verified', $4, $5, 'aal2', $6, $7)
            returning
              id,
              user_id,
              shared_auth_session_id,
              assurance_verified_at,
              expires_at
            "#,
            vec![
                user_id.into(),
                normalized.device.into(),
                normalized.location.into(),
                shared_auth_session_id.into(),
                CUSTOMER_PROVIDER_PROJECT.into(),
                assurance_verified_at.into(),
                expires_at.into(),
            ],
        ))
        .await?
        .ok_or_else(|| invalid("customer Shared Auth session insert returned no row"))?;
    row_to_verified(&row)
}

/// Resolve a browser Shared Auth SID for the exact Supabase subject at request
/// time. This is the application MFA gate: wrong user, wrong project, aal1,
/// future/unverified, expired, active-but-unverified, and revoked rows all return
/// `None` rather than a partial identity.
pub async fn verified_session_for_provider(
    db: &DatabaseConnection,
    supabase_user_id: Uuid,
    shared_auth_session_id: Uuid,
    now: DateTimeWithTimeZone,
) -> Result<Option<VerifiedCustomerSession>, DbErr> {
    db.query_one(Statement::from_sql_and_values(
        DatabaseBackend::Postgres,
        r#"
        select
          session.id,
          session.user_id,
          session.shared_auth_session_id,
          session.assurance_verified_at,
          session.expires_at
        from customer_sessions as session
        join users as local_user on local_user.id = session.user_id
        where local_user.supabase_user_id = $1
          and session.shared_auth_session_id = $2
          and session.provider_project = $3
          and session.assurance_level = 'aal2'
          and session.status = 'verified'
          and session.assurance_verified_at is not null
          and session.assurance_verified_at <= $4
          and session.expires_at is not null
          and session.expires_at > $4
        limit 1
        "#,
        vec![
            supabase_user_id.into(),
            shared_auth_session_id.into(),
            CUSTOMER_PROVIDER_PROJECT.into(),
            now.into(),
        ],
    ))
    .await?
    .as_ref()
    .map(row_to_verified)
    .transpose()
}

/// Revoke one verified SID, scoped through the local user's Supabase subject.
/// A foreign user cannot revoke another customer's row even if it learns the
/// opaque SID. Repeating revocation returns `false`.
pub async fn revoke_verified_session_for_provider(
    db: &DatabaseConnection,
    supabase_user_id: Uuid,
    shared_auth_session_id: Uuid,
) -> Result<bool, DbErr> {
    Ok(db
        .query_one(Statement::from_sql_and_values(
            DatabaseBackend::Postgres,
            r#"
            update customer_sessions as session
               set status = 'revoked'
              from users as local_user
             where local_user.id = session.user_id
               and local_user.supabase_user_id = $1
               and session.shared_auth_session_id = $2
               and session.provider_project = $3
               and session.assurance_level = 'aal2'
               and session.status = 'verified'
            returning session.id
            "#,
            vec![
                supabase_user_id.into(),
                shared_auth_session_id.into(),
                CUSTOMER_PROVIDER_PROJECT.into(),
            ],
        ))
        .await?
        .is_some())
}
