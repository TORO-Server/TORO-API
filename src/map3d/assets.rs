//! (実装中)
use std::sync::Arc;
use crate::config::MapDb;
pub trait AssetStore: Send + Sync {}
pub struct SqlxAssetStore {}
impl SqlxAssetStore { pub fn new(_db: &MapDb, _database: &str) -> Self { Self {} } }
impl AssetStore for SqlxAssetStore {}
pub struct Assets {}
impl Assets { pub fn new(_store: Arc<dyn AssetStore>, _origins: Vec<String>) -> Self { Self {} } }
