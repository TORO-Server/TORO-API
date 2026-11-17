//! メモリを使うキャッシュの上限を、ここで一元管理する。
//!
//! 移管前の toro-webmap は、看板中継の本文のキャッシュに 160 MB、`/_assets/` の中身のキャッシュに 256 MiB を持っていた。
//! このサービスでは、その合計を既定で 256 MiB 以下にする (コンテナのメモリの上限の見直しは、配備のときに行う)。
//! キャッシュを足すときは、ここに定数を足して、下の試験の合計に入れる。

const MIB: usize = 1024 * 1024;

/// 看板中継 (`/map3d/v1/marumasign/proxy`) の本文のキャッシュの合計
pub const SIGN_BODY_CACHE_BYTES: usize = 96 * MIB;
/// 看板中継で、1 つの本文をキャッシュしてよい大きさ
pub const SIGN_BODY_CACHE_ENTRY_BYTES: usize = 4 * MIB;
/// `/_assets/` の中身 (1 MiB ずつのかたまり) のキャッシュの合計
pub const ASSET_CHUNK_CACHE_BYTES: usize = 160 * MIB;
/// 2 つのキャッシュの合計の上限
pub const TOTAL_CACHE_BYTES: usize = 256 * MIB;

// 2 つのキャッシュの合計が上限に収まること (足したときに、ここで気づく)
const _: () = assert!(SIGN_BODY_CACHE_BYTES + ASSET_CHUNK_CACHE_BYTES <= TOTAL_CACHE_BYTES);
const _: () = assert!(SIGN_BODY_CACHE_ENTRY_BYTES <= SIGN_BODY_CACHE_BYTES);
