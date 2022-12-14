//! `SeaORM` Entity. Generated by sea-orm-codegen 0.10.5

use crate::serialization;
use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Eq, Serialize, Deserialize)]
#[sea_orm(table_name = "user")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: u64,
    #[sea_orm(unique)]
    #[serde(serialize_with = "serialization::vec_as_address")]
    pub address: Vec<u8>,
    pub description: Option<String>,
    pub email: Option<String>,
    pub user_tier_id: u64,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(has_many = "super::login::Entity")]
    Login,
    #[sea_orm(has_many = "super::rpc_key::Entity")]
    RpcKey,
    #[sea_orm(has_many = "super::secondary_user::Entity")]
    SecondaryUser,
    #[sea_orm(
        belongs_to = "super::user_tier::Entity",
        from = "Column::UserTierId",
        to = "super::user_tier::Column::Id",
        on_update = "NoAction",
        on_delete = "NoAction"
    )]
    UserTier,
}

impl Related<super::login::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Login.def()
    }
}

impl Related<super::rpc_key::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::RpcKey.def()
    }
}

impl Related<super::secondary_user::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::SecondaryUser.def()
    }
}

impl Related<super::user_tier::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::UserTier.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}
