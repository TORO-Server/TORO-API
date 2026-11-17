//! BlueMap の SQL 保存 (TiDB の `bluemap` データベース) の読み取り。**SELECT だけ**を発行する。
//!
//! 鍵の表 (地図、格子、項目、圧縮) は小さく、ほとんど変わらないので、番号を 60 秒覚えておく。
//! データの表は主キー (map, storage, x, z) で 1 行だけ引く。鍵の名前は BlueMap 5.x の `KeyedMapStorage` と同じ。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures_util::future::BoxFuture;
use sqlx::mysql::{MySqlConnectOptions, MySqlPool, MySqlPoolOptions, MySqlSslMode};
use sqlx::{ConnectOptions, Row};

use crate::config::MapDb;
use crate::map::store::{StoreError, StoreResult};

const QUERY_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_CONNECTIONS: u32 = 16;
/// 鍵の表を読み直す間隔
const KEYS_MAX_AGE: Duration = Duration::from_secs(60);
/// 知らない鍵で、続けて読み直さない間隔
const KEYS_MIN_AGE: Duration = Duration::from_secs(10);

/// 読んだデータと、その圧縮 (`bluemap:gzip`、`bluemap:none` など)
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlueData {
    pub data: Vec<u8>,
    pub compression: String,
}

pub trait BluemapStore: Send + Sync {
    /// 格子のデータ (タイル)。`storage` は `bluemap:hires` や `bluemap:lowres/1`
    fn tile<'a>(
        &'a self,
        map: &'a str,
        storage: &'a str,
        x: i64,
        z: i64,
    ) -> BoxFuture<'a, StoreResult<Option<BlueData>>>;
    /// 項目のデータ。`storage` は `bluemap:settings` や `bluemap:asset/playerheads/<UUID>.png`
    fn item<'a>(
        &'a self,
        map: &'a str,
        storage: &'a str,
    ) -> BoxFuture<'a, StoreResult<Option<BlueData>>>;
    /// BlueMap の地図の id として DB にあるか (知らない名前で、BlueMap 本体へ聞きに行かないため)
    fn map_exists<'a>(&'a self, map: &'a str) -> BoxFuture<'a, StoreResult<bool>>;
    /// DB にある地図の id
    fn map_ids<'a>(&'a self) -> BoxFuture<'a, StoreResult<Vec<String>>>;
}

/// 圧縮の名前 → `Content-Encoding`
pub fn content_encoding(compression: &str) -> Option<&'static str> {
    match compression {
        "bluemap:gzip" => Some("gzip"),
        "bluemap:deflate" => Some("deflate"),
        "bluemap:zstd" => Some("zstd"),
        "bluemap:lz4" => Some("lz4"),
        _ => None,
    }
}

/// 圧縮を解く (gzip と deflate だけ。ほかは None)
pub fn decompress(data: &BlueData) -> Option<Vec<u8>> {
    use std::io::Read;
    match data.compression.as_str() {
        "bluemap:gzip" => {
            let mut out = Vec::new();
            flate2::read::GzDecoder::new(&data.data[..])
                .read_to_end(&mut out)
                .ok()?;
            Some(out)
        }
        "bluemap:deflate" => {
            let mut out = Vec::new();
            flate2::read::ZlibDecoder::new(&data.data[..])
                .read_to_end(&mut out)
                .ok()?;
            Some(out)
        }
        "bluemap:none" => Some(data.data.clone()),
        _ => None,
    }
}

#[derive(Default)]
struct Keys {
    maps: HashMap<String, i64>,
    grids: HashMap<String, i64>,
    items: HashMap<String, i64>,
    compressions: HashMap<i64, String>,
    loaded_at: Option<Instant>,
}

// 接頭辞 `bluemap_` は BlueMap の既定 (本番の DB もこれ)。文字列は固定で、値は必ず bind で渡す
const SQL_MAPS: &str = "SELECT CAST(id AS SIGNED) AS id, map_id AS name FROM bluemap_map";
const SQL_GRIDS: &str = "SELECT CAST(id AS SIGNED) AS id, `key` AS name FROM bluemap_grid_storage";
const SQL_ITEMS: &str = "SELECT CAST(id AS SIGNED) AS id, `key` AS name FROM bluemap_item_storage";
const SQL_COMPRESSIONS: &str =
    "SELECT CAST(id AS SIGNED) AS id, `key` AS name FROM bluemap_compression";
const SQL_TILE: &str = "SELECT data, CAST(compression AS SIGNED) AS compression FROM bluemap_grid_storage_data \
     WHERE map = ? AND storage = ? AND x = ? AND z = ?";
const SQL_ITEM: &str = "SELECT data, CAST(compression AS SIGNED) AS compression FROM bluemap_item_storage_data \
     WHERE map = ? AND storage = ?";

pub struct SqlxBluemapStore {
    pool: MySqlPool,
    keys: Mutex<Arc<Keys>>,
    /// 同時に鍵の表を読まない
    reload: tokio::sync::Mutex<()>,
}

impl SqlxBluemapStore {
    /// 接続は最初の問い合わせまで張らない (DB が先に起動していなくても、サービスは立ち上がる)。
    /// `db` の接続先 (host、port、user) を使い、データベースだけを `database` にする
    pub fn new(db: &MapDb, database: &str) -> Self {
        let mut options = MySqlConnectOptions::new()
            .host(&db.host)
            .port(db.port)
            .username(&db.user)
            .database(database)
            .ssl_mode(MySqlSslMode::Disabled)
            .disable_statement_logging();
        if !db.password.is_empty() {
            options = options.password(&db.password);
        }
        let pool = MySqlPoolOptions::new()
            .max_connections(MAX_CONNECTIONS)
            .acquire_timeout(QUERY_TIMEOUT)
            .connect_lazy_with(options);
        Self {
            pool,
            keys: Mutex::new(Arc::new(Keys::default())),
            reload: tokio::sync::Mutex::new(()),
        }
    }

    fn current(&self) -> Arc<Keys> {
        self.keys.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    async fn all(&self, sql: &'static str) -> StoreResult<Vec<(i64, String)>> {
        let rows = match tokio::time::timeout(QUERY_TIMEOUT, sqlx::query(sql).fetch_all(&self.pool))
            .await
        {
            Ok(Ok(rows)) => rows,
            Ok(Err(e)) => return Err(StoreError(e.to_string())),
            Err(_) => return Err(StoreError("query timeout".into())),
        };
        rows.iter()
            .map(|row| {
                let id = row
                    .try_get_unchecked::<i64, _>(0)
                    .map_err(|e| StoreError(e.to_string()))?;
                let name = row
                    .try_get_unchecked::<Vec<u8>, _>(1)
                    .map_err(|e| StoreError(e.to_string()))?;
                Ok((id, String::from_utf8_lossy(&name).into_owned()))
            })
            .collect()
    }

    /// 鍵の表。`fresh` なら、読んだばかりでなければ読み直す
    async fn keys(&self, fresh: bool) -> StoreResult<Arc<Keys>> {
        let max_age = if fresh { KEYS_MIN_AGE } else { KEYS_MAX_AGE };
        let stale = |k: &Keys| k.loaded_at.is_none_or(|at| at.elapsed() >= max_age);
        let current = self.current();
        if !stale(&current) {
            return Ok(current);
        }
        let _guard = self.reload.lock().await;
        let current = self.current();
        if !stale(&current) {
            return Ok(current);
        }
        let (maps, grids, items, compressions) = tokio::try_join!(
            self.all(SQL_MAPS),
            self.all(SQL_GRIDS),
            self.all(SQL_ITEMS),
            self.all(SQL_COMPRESSIONS)
        )?;
        let invert = |rows: Vec<(i64, String)>| -> HashMap<String, i64> {
            rows.into_iter().map(|(id, name)| (name, id)).collect()
        };
        let keys = Arc::new(Keys {
            maps: invert(maps),
            grids: invert(grids),
            items: invert(items),
            compressions: compressions.into_iter().collect(),
            loaded_at: Some(Instant::now()),
        });
        *self.keys.lock().unwrap_or_else(|e| e.into_inner()) = keys.clone();
        Ok(keys)
    }

    async fn fetch(
        &self,
        query: sqlx::query::Query<'_, sqlx::MySql, sqlx::mysql::MySqlArguments>,
        keys: &Keys,
    ) -> StoreResult<Option<BlueData>> {
        let row = match tokio::time::timeout(QUERY_TIMEOUT, query.fetch_optional(&self.pool)).await
        {
            Ok(Ok(row)) => row,
            Ok(Err(e)) => return Err(StoreError(e.to_string())),
            Err(_) => return Err(StoreError("query timeout".into())),
        };
        let Some(row) = row else { return Ok(None) };
        let data = row
            .try_get_unchecked::<Vec<u8>, _>(0)
            .map_err(|e| StoreError(e.to_string()))?;
        let compression_id = row
            .try_get_unchecked::<i64, _>(1)
            .map_err(|e| StoreError(e.to_string()))?;
        Ok(Some(BlueData {
            data,
            compression: keys
                .compressions
                .get(&compression_id)
                .cloned()
                .unwrap_or_else(|| "bluemap:none".to_string()),
        }))
    }
}

impl BluemapStore for SqlxBluemapStore {
    fn tile<'a>(
        &'a self,
        map: &'a str,
        storage: &'a str,
        x: i64,
        z: i64,
    ) -> BoxFuture<'a, StoreResult<Option<BlueData>>> {
        Box::pin(async move {
            // 見つからない鍵があったときは、新しく増えたかもしれないので読み直す
            for fresh in [false, true] {
                let keys = self.keys(fresh).await?;
                let (Some(&map_id), Some(&grid_id)) = (keys.maps.get(map), keys.grids.get(storage))
                else {
                    continue;
                };
                let query = sqlx::query(SQL_TILE)
                    .bind(map_id)
                    .bind(grid_id)
                    .bind(x)
                    .bind(z);
                return self.fetch(query, &keys).await;
            }
            Ok(None)
        })
    }

    fn item<'a>(
        &'a self,
        map: &'a str,
        storage: &'a str,
    ) -> BoxFuture<'a, StoreResult<Option<BlueData>>> {
        Box::pin(async move {
            for fresh in [false, true] {
                let keys = self.keys(fresh).await?;
                let (Some(&map_id), Some(&item_id)) = (keys.maps.get(map), keys.items.get(storage))
                else {
                    continue;
                };
                let query = sqlx::query(SQL_ITEM).bind(map_id).bind(item_id);
                return self.fetch(query, &keys).await;
            }
            Ok(None)
        })
    }

    fn map_exists<'a>(&'a self, map: &'a str) -> BoxFuture<'a, StoreResult<bool>> {
        Box::pin(async move {
            if self.keys(false).await?.maps.contains_key(map) {
                return Ok(true);
            }
            // 新しい地図かもしれない。ただし、読んだばかりのときは読み直さない
            Ok(self.keys(true).await?.maps.contains_key(map))
        })
    }

    fn map_ids<'a>(&'a self) -> BoxFuture<'a, StoreResult<Vec<String>>> {
        Box::pin(async move {
            let mut ids: Vec<String> = self.keys(false).await?.maps.keys().cloned().collect();
            ids.sort();
            Ok(ids)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 発行する文は SELECT だけ (DB の書き込みをこのサービスから行わない)
    #[test]
    fn every_statement_is_a_select() {
        for statement in [
            SQL_MAPS,
            SQL_GRIDS,
            SQL_ITEMS,
            SQL_COMPRESSIONS,
            SQL_TILE,
            SQL_ITEM,
        ] {
            assert!(statement.starts_with("SELECT "), "{statement}");
            for word in ["INSERT", "UPDATE", "DELETE", "REPLACE", "DROP", "ALTER"] {
                assert!(!statement.contains(word), "{statement}");
            }
        }
    }

    #[test]
    fn encodings() {
        assert_eq!(content_encoding("bluemap:gzip"), Some("gzip"));
        assert_eq!(content_encoding("bluemap:none"), None);
    }
}
