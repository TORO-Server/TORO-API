//! Dynmap の DB (TiDB) の読み取り。**SELECT だけ**を発行する (書き込みの文はこのファイルに無い)。
//!
//! 試験では `MapStore` を差し替える (DB なしで、ハンドラーとハブを確かめるため)。

use std::time::Duration;

use futures_util::future::BoxFuture;
use sqlx::mysql::{MySqlConnectOptions, MySqlPool, MySqlPoolOptions, MySqlSslMode};
use sqlx::{ConnectOptions, Row};

use crate::config::MapDb;

/// 1 つの問い合わせの時間 (入り口のプロキシが返事を待つ長さと同じ)
const QUERY_TIMEOUT: Duration = Duration::from_secs(30);
/// 接続の上限 (TiDB は 1 台なので、地図の配信には 16 本あれば足りる)
const MAX_CONNECTIONS: u32 = 16;

#[derive(Debug)]
pub struct StoreError(pub String);

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

pub type StoreResult<T> = Result<T, StoreError>;

/// タイルの 1 行
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TileRow {
    pub hash_code: i64,
    pub last_update: i64,
    pub format: i64,
    /// `IFNULL(NewImage, Image)`。無ければ None
    pub image: Option<Vec<u8>>,
}

/// タイルの探し方 (`dynmap_Maps` と `dynmap_Tiles` の結合)
#[derive(Clone, Copy, Debug)]
pub struct TileKey<'a> {
    pub world: &'a str,
    pub map: &'a str,
    /// `DAY` か `STANDARD`
    pub variant: &'a str,
    pub x: i64,
    pub y: i64,
    pub zoom: i64,
}

pub trait MapStore: Send + Sync {
    /// `StandaloneFiles` (ServerID = 0) の本文。`dynmap_config.json` と `dynmap_<world>.json`
    fn standalone_file<'a>(&'a self, name: &'a str) -> BoxFuture<'a, StoreResult<Option<String>>>;
    fn tile<'a>(&'a self, key: TileKey<'a>) -> BoxFuture<'a, StoreResult<Option<TileRow>>>;
    fn marker_file<'a>(&'a self, name: &'a str) -> BoxFuture<'a, StoreResult<Option<String>>>;
    fn marker_icon<'a>(&'a self, name: &'a str) -> BoxFuture<'a, StoreResult<Option<Vec<u8>>>>;
    /// `type_id`: 8x8=0、16x16=1、32x32=2、body=3
    fn face<'a>(
        &'a self,
        player: &'a str,
        type_id: i32,
    ) -> BoxFuture<'a, StoreResult<Option<Vec<u8>>>>;
}

// 発行する SELECT。接頭辞 `dynmap_` は Dynmap の既定 (本番の DB もこれ)。文字列は固定で、値は必ず bind で渡す
const SQL_STANDALONE: &str =
    "SELECT Content FROM dynmap_StandaloneFiles WHERE FileName = ? AND ServerID = 0";
// CAST は、列の型 (INT/BIGINT) に関わらず i64 で読むため
const SQL_TILE: &str = "SELECT CAST(t.HashCode AS SIGNED) AS HashCode, CAST(t.LastUpdate AS SIGNED) AS LastUpdate, \
     CAST(t.Format AS SIGNED) AS Format, IFNULL(t.NewImage, t.Image) AS Image \
     FROM dynmap_Maps m JOIN dynmap_Tiles t ON m.ID = t.MapID \
     WHERE m.WorldID = ? AND m.MapID = ? AND m.Variant = ? AND t.x = ? AND t.y = ? AND t.zoom = ?";
const SQL_MARKER_FILE: &str = "SELECT Content FROM dynmap_MarkerFiles WHERE FileName = ?";
const SQL_MARKER_ICON: &str = "SELECT Image FROM dynmap_MarkerIcons WHERE IconName = ?";
const SQL_FACE: &str = "SELECT Image FROM dynmap_Faces WHERE PlayerName = ? AND TypeID = ?";
pub struct SqlxStore {
    pool: MySqlPool,
}

impl SqlxStore {
    /// 接続は最初の問い合わせまで張らない (DB が先に起動していなくても、サービスは立ち上がる)
    pub fn new(db: &MapDb) -> Self {
        let mut options = MySqlConnectOptions::new()
            .host(&db.host)
            .port(db.port)
            .username(&db.user)
            .database(&db.database)
            // 内部ネットワークだけで使う。TLS の機能は入れていない
            .ssl_mode(MySqlSslMode::Disabled)
            .disable_statement_logging();
        // 空のパスワードは「なし」として送る (空の値を付けると、パスワードなしの利用者が断られる)
        if !db.password.is_empty() {
            options = options.password(&db.password);
        }
        let pool = MySqlPoolOptions::new()
            .max_connections(MAX_CONNECTIONS)
            .acquire_timeout(QUERY_TIMEOUT)
            .connect_lazy_with(options);
        Self { pool }
    }

    async fn fetch(
        &self,
        query: sqlx::query::Query<'_, sqlx::MySql, sqlx::mysql::MySqlArguments>,
    ) -> StoreResult<Option<sqlx::mysql::MySqlRow>> {
        match tokio::time::timeout(QUERY_TIMEOUT, query.fetch_optional(&self.pool)).await {
            Ok(Ok(row)) => Ok(row),
            Ok(Err(e)) => Err(StoreError(e.to_string())),
            Err(_) => Err(StoreError("query timeout".into())),
        }
    }
}

fn bytes(row: &sqlx::mysql::MySqlRow, index: usize) -> StoreResult<Vec<u8>> {
    // 列の型 (TEXT / BLOB) の違いで失敗しないよう、型の照合なしで読む
    row.try_get_unchecked::<Vec<u8>, _>(index)
        .map_err(|e| StoreError(e.to_string()))
}

fn optional_bytes(row: &sqlx::mysql::MySqlRow, index: usize) -> StoreResult<Option<Vec<u8>>> {
    row.try_get_unchecked::<Option<Vec<u8>>, _>(index)
        .map_err(|e| StoreError(e.to_string()))
}

fn text(raw: Vec<u8>) -> String {
    String::from_utf8_lossy(&raw).into_owned()
}

impl MapStore for SqlxStore {
    fn standalone_file<'a>(&'a self, name: &'a str) -> BoxFuture<'a, StoreResult<Option<String>>> {
        Box::pin(async move {
            let row = self.fetch(sqlx::query(SQL_STANDALONE).bind(name)).await?;
            row.map(|r| bytes(&r, 0).map(text)).transpose()
        })
    }

    fn tile<'a>(&'a self, key: TileKey<'a>) -> BoxFuture<'a, StoreResult<Option<TileRow>>> {
        Box::pin(async move {
            let query = sqlx::query(SQL_TILE)
                .bind(key.world)
                .bind(key.map)
                .bind(key.variant)
                .bind(key.x)
                .bind(key.y)
                .bind(key.zoom);
            let Some(row) = self.fetch(query).await? else {
                return Ok(None);
            };
            let int = |i: usize| -> StoreResult<i64> {
                row.try_get_unchecked::<Option<i64>, _>(i)
                    .map(|v| v.unwrap_or(0))
                    .map_err(|e| StoreError(e.to_string()))
            };
            Ok(Some(TileRow {
                hash_code: int(0)?,
                last_update: int(1)?,
                format: int(2)?,
                image: optional_bytes(&row, 3)?,
            }))
        })
    }

    fn marker_file<'a>(&'a self, name: &'a str) -> BoxFuture<'a, StoreResult<Option<String>>> {
        Box::pin(async move {
            let row = self.fetch(sqlx::query(SQL_MARKER_FILE).bind(name)).await?;
            row.map(|r| bytes(&r, 0).map(text)).transpose()
        })
    }

    fn marker_icon<'a>(&'a self, name: &'a str) -> BoxFuture<'a, StoreResult<Option<Vec<u8>>>> {
        Box::pin(async move {
            let row = self.fetch(sqlx::query(SQL_MARKER_ICON).bind(name)).await?;
            row.map(|r| bytes(&r, 0)).transpose()
        })
    }

    fn face<'a>(
        &'a self,
        player: &'a str,
        type_id: i32,
    ) -> BoxFuture<'a, StoreResult<Option<Vec<u8>>>> {
        Box::pin(async move {
            let row = self
                .fetch(sqlx::query(SQL_FACE).bind(player).bind(type_id))
                .await?;
            row.map(|r| bytes(&r, 0)).transpose()
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
            SQL_STANDALONE,
            SQL_TILE,
            SQL_MARKER_FILE,
            SQL_MARKER_ICON,
            SQL_FACE,
        ] {
            assert!(statement.starts_with("SELECT "), "{statement}");
            for word in ["INSERT", "UPDATE", "DELETE", "REPLACE", "DROP", "ALTER"] {
                assert!(!statement.contains(word), "{statement}");
            }
        }
    }
}
