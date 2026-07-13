//! SeaORM entity for the `users` table — the thin local mirror of a Supabase
//! auth user (source of truth is Supabase), keyed by `supabase_user_id`. We
//! upsert a row on first authenticated access so preferences/sessions/audit can
//! join against a stable local id. Schema: `fiducia-interfaces/sql/customer.sql`.

use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "users")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: Uuid,
    pub supabase_user_id: Uuid,
    pub email: String,
    pub created_at: DateTimeWithTimeZone,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
