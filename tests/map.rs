//! 地図 API の HTTP (`/map/v1/*`): ヘッダー、ETag、304、異常系。DB は試験用の偽物 (`FakeStore`)。
//! 期待値は、移管前の実装 (toro-webmap) の実測 (ゴールデンと、オリジンの直接の応答) による

mod common;

use axum::http::header;
use common::fake_map::*;
use md5::{Digest, Md5};
use reqwest::StatusCode;
use toro_api::map::store::TileRow;

const BLANK_ETAG: &str = "\"1990bd0b9c4dbb6c8425aeb42207fecc\"";

fn md5_hex(data: &[u8]) -> String {
    Md5::digest(data)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn h(res: &reqwest::Response, name: header::HeaderName) -> &str {
    res.headers()
        .get(name)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
}

fn tile(hash: i64, format: i64, image: Option<&[u8]>) -> TileRow {
    TileRow {
        hash_code: hash,
        last_update: 1_784_805_652_212,
        format,
        image: image.map(<[u8]>::to_vec),
    }
}

// ---- configuration

#[tokio::test]
async fn configuration_returns_the_stored_file_with_md5_etag() {
    let server = start_map(true).await;
    let res = server.get("/map/v1/configuration").await;
    assert_eq!(res.status(), 200);
    assert_eq!(
        h(&res, header::CONTENT_TYPE),
        "application/json; charset=utf-8"
    );
    assert_eq!(h(&res, header::CACHE_CONTROL), "no-cache");
    assert_eq!(h(&res, header::ACCESS_CONTROL_ALLOW_ORIGIN), "*");
    assert_eq!(h(&res, header::X_CONTENT_TYPE_OPTIONS), "nosniff");
    let etag = format!("\"{}\"", md5_hex(CONFIG_JSON.as_bytes()));
    assert_eq!(h(&res, header::ETAG), etag);
    assert_eq!(res.text().await.unwrap(), CONFIG_JSON);
}

#[tokio::test]
async fn configuration_answers_304_only_for_the_exact_etag() {
    let server = start_map(true).await;
    let etag = format!("\"{}\"", md5_hex(CONFIG_JSON.as_bytes()));
    let res = server
        .get_with("/map/v1/configuration", "If-None-Match", &etag)
        .await;
    assert_eq!(res.status(), 304);
    assert_eq!(h(&res, header::ETAG), etag);
    assert_eq!(h(&res, header::CACHE_CONTROL), "no-cache");
    assert_eq!(h(&res, header::ACCESS_CONTROL_ALLOW_ORIGIN), "*");
    assert_eq!(res.bytes().await.unwrap().len(), 0);
    // 旧実装は文字列の完全一致だけ (`W/` や複数の値は解かない)
    for value in [
        format!("W/{etag}"),
        format!("{etag}, \"x\""),
        "\"nope\"".into(),
    ] {
        let res = server
            .get_with("/map/v1/configuration", "If-None-Match", &value)
            .await;
        assert_eq!(res.status(), 200, "{value}");
    }
}

#[tokio::test]
async fn configuration_is_503_until_dynmap_writes_it_and_500_when_the_db_fails() {
    let server = start_map(true).await;
    server.store.remove_file("dynmap_config.json");
    let res = server.get("/map/v1/configuration").await;
    assert_eq!(res.status(), 503);
    assert_eq!(h(&res, header::CACHE_CONTROL), "no-cache");
    server.store.set_failing(true);
    let res = server.get("/map/v1/configuration").await;
    assert_eq!(res.status(), 500);
    assert_eq!(h(&res, header::CACHE_CONTROL), "no-cache");
}

#[tokio::test]
async fn a_trailing_slash_is_ignored_once() {
    let server = start_map(true).await;
    assert_eq!(server.get("/map/v1/configuration/").await.status(), 200);
    assert_eq!(server.get("/map/v1/configuration//").await.status(), 404);
    assert_eq!(server.get("/map/v1/configuration/x").await.status(), 404);
    assert_eq!(server.get("/map/v1/world/main/0/").await.status(), 200);
    assert_eq!(server.get("/map/v1/world/main/0//").await.status(), 404);
}

// ---- world

#[tokio::test]
async fn world_update_ignores_the_timestamp_segment() {
    let server = start_map(true).await;
    let a = server.get("/map/v1/world/main/0").await;
    assert_eq!(a.status(), 200);
    assert_eq!(h(&a, header::CACHE_CONTROL), "no-cache");
    assert_eq!(h(&a, header::ACCESS_CONTROL_ALLOW_ORIGIN), "*");
    assert_eq!(
        h(&a, header::CONTENT_TYPE),
        "application/json; charset=utf-8"
    );
    let etag = h(&a, header::ETAG).to_string();
    let body = a.text().await.unwrap();
    assert_eq!(body, update_json(1, "[]"));
    for ts in ["999999", "abc", "%zz"] {
        let res = server.get(&format!("/map/v1/world/main/{ts}")).await;
        assert_eq!(res.status(), 200, "{ts}");
        assert_eq!(h(&res, header::ETAG), etag);
    }
    // ts が空でも通る (旧実装のルーターは、空の部分も 1 つの値として扱う)
    assert_eq!(server.get("/map/v1/world/main//").await.status(), 200);
}

#[tokio::test]
async fn world_name_with_a_backslash_is_invalid_with_status_200() {
    let server = start_map(true).await;
    // `%5C` は `\`。ルーターが `decodeURI` するので、ワールド名に届く
    let res = server.get("/map/v1/world/a%5Cb/0").await;
    assert_eq!(res.status(), 200);
    assert_eq!(h(&res, header::CACHE_CONTROL), "no-cache");
    assert_eq!(res.text().await.unwrap(), r#"{ "error": "invalid-world" }"#);
    // `%2F` は `decodeURI` で戻らない (予約文字)。名前に `/` は入らず、知らないワールドとして 404
    let res = server.get("/map/v1/world/a%2Fb/0").await;
    assert_eq!(res.status(), 404);
}

#[tokio::test]
async fn unknown_world_is_404_without_reading_its_file() {
    let server = start_map(true).await;
    server.store.set_file("dynmap_nosuch.json", "{}");
    let res = server.get("/map/v1/world/nosuch/0").await;
    assert_eq!(res.status(), 404);
    // 旧実装は CORS のヘッダーを付けず、no-cache。本文だけ HTML から JSON に変わる (許容した差異)
    assert_eq!(h(&res, header::CACHE_CONTROL), "no-cache");
    assert!(
        res.headers()
            .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
            .is_none()
    );
    assert_eq!(h(&res, header::CONTENT_TYPE), "application/json");
    assert_eq!(server.store.read_count("dynmap_nosuch.json"), 0);
}

#[tokio::test]
async fn world_names_are_decoded_like_decode_uri() {
    let server = start_map(true).await;
    assert_eq!(server.get("/map/v1/world/ma%69n/0").await.status(), 200);
    // 壊れたエンコードがあれば、そのまま (ワールド名は `%6dain` のまま。知らないので 404)
    assert_eq!(server.get("/map/v1/world/%6dain/%zz").await.status(), 404);
    // ts は読まないので、壊れていても通る
    assert_eq!(server.get("/map/v1/world/main/%zz").await.status(), 200);
    assert_eq!(server.get("/map/v1/world/%E3%81%82/0").await.status(), 404);
    assert_eq!(server.get("/map/v1/world//0").await.status(), 404);
}

#[tokio::test]
async fn world_is_read_from_the_db_when_the_world_list_is_unreadable() {
    let server = start_map(true).await;
    server.store.remove_file("dynmap_config.json");
    // 一覧が読めないときは、これまでどおり読みに行く: 行があれば 200、無ければ 503
    let res = server.get("/map/v1/world/main/0").await;
    assert_eq!(res.status(), 200);
    server.store.remove_file("dynmap_main.json");
    let res = server.get("/map/v1/world/main/0").await;
    assert_eq!(res.status(), 503);
}

#[tokio::test]
async fn world_is_500_when_the_db_fails() {
    let server = start_map(true).await;
    server.store.set_failing(true);
    assert_eq!(server.get("/map/v1/world/main/0").await.status(), 500);
}

// ---- tiles

#[tokio::test]
async fn tile_uses_format_hash_and_last_update() {
    let server = start_map(true).await;
    let jpeg = b"jpeg-bytes";
    server.store.tiles.lock().unwrap().insert(
        tile_key("main", "flat", "STANDARD", 2622, 119, 0),
        tile(0, 1, Some(jpeg)),
    );
    let res = server
        .get("/map/v1/tiles/main/flat/81_3/2622_119.webp")
        .await;
    assert_eq!(res.status(), 200);
    // 拡張子が .webp でも、中身は Format 列 (1 = jpeg) で決まる
    assert_eq!(h(&res, header::CONTENT_TYPE), "image/jpeg");
    assert_eq!(h(&res, header::CACHE_CONTROL), "public, max-age=300");
    // ETag は HashCode (本文の MD5 ではない。旧実装のとおり "0" のまま)
    assert_eq!(h(&res, header::ETAG), "\"0\"");
    assert_eq!(
        h(&res, header::LAST_MODIFIED),
        "Thu, 23 Jul 2026 11:20:52 GMT"
    );
    assert_eq!(h(&res, header::ACCESS_CONTROL_ALLOW_ORIGIN), "*");
    assert_eq!(res.bytes().await.unwrap().as_ref(), jpeg);

    let res = server
        .get_with(
            "/map/v1/tiles/main/flat/81_3/2622_119.webp",
            "If-None-Match",
            "\"0\"",
        )
        .await;
    assert_eq!(res.status(), 304);
    assert_eq!(
        h(&res, header::LAST_MODIFIED),
        "Thu, 23 Jul 2026 11:20:52 GMT"
    );
}

#[tokio::test]
async fn tile_content_type_follows_the_format_column() {
    let server = start_map(true).await;
    for (format, expected) in [
        (0, "image/png"),
        (1, "image/jpeg"),
        (2, "image/webp"),
        (3, "image/jpeg"),
        (-1, "image/jpeg"),
    ] {
        let world = format!("w{format}");
        server.store.tiles.lock().unwrap().insert(
            tile_key(&world, "t", "STANDARD", 1, 2, 0),
            tile(7, format, Some(b"x")),
        );
        let res = server
            .get(&format!("/map/v1/tiles/{world}/t/0_0/1_2.png"))
            .await;
        assert_eq!(h(&res, header::CONTENT_TYPE), expected, "format {format}");
    }
}

#[tokio::test]
async fn tile_timestamp_query_switches_cache_control_by_presence_only() {
    let server = start_map(true).await;
    server.store.tiles.lock().unwrap().insert(
        tile_key("main", "t", "STANDARD", 1, 2, 0),
        tile(5, 0, Some(b"x")),
    );
    for (query, cache) in [
        ("", "public, max-age=300"),
        (
            "?timestamp=1784805652212",
            "public, max-age=604800, immutable",
        ),
        ("?timestamp=", "public, max-age=604800, immutable"),
        ("?timestamp", "public, max-age=604800, immutable"),
        ("?TIMESTAMP=1", "public, max-age=300"),
        ("?other=1", "public, max-age=300"),
    ] {
        let res = server
            .get(&format!("/map/v1/tiles/main/t/0_0/1_2.png{query}"))
            .await;
        assert_eq!(h(&res, header::CACHE_CONTROL), cache, "{query}");
    }
}

#[tokio::test]
async fn tile_path_is_parsed_into_world_map_variant_zoom_x_y() {
    let server = start_map(true).await;
    let q = &server.store.tile_queries;
    for path in [
        "/map/v1/tiles/main/flat/0_0/10_20.png",
        "/map/v1/tiles/main/flat_day/0_0/10_20.png",
        "/map/v1/tiles/main/flat/0_0/zzz_10_20.png",
        "/map/v1/tiles/main/flat/0_0/-5_20.png",
        "/map/v1/tiles/main/flat/0_0/0x10_1e2.png",
        "/map/v1/tiles/main/flat/0_0/_5_6.png",
        "/map/v1/tiles/main/flat/0_0/10_20",
        "/map/v1/tiles/main%2Fflat/0_0/10_20.png",
        "/map/v1/tiles/main/flat/0_0/10_20.png/",
        "/map/v1/tiles/main/_day/0_0/10_20.png",
    ] {
        server.get(path).await;
    }
    assert_eq!(
        q.lock().unwrap().clone(),
        vec![
            tile_key("main", "flat", "STANDARD", 10, 20, 0),
            tile_key("main", "flat", "DAY", 10, 20, 0),
            tile_key("main", "flat", "STANDARD", 10, 20, 3),
            tile_key("main", "flat", "STANDARD", -5, 20, 0),
            // JavaScript の Number(): 16 進と指数も数になる
            tile_key("main", "flat", "STANDARD", 16, 100, 0),
            // 空の文字列は 0 (先頭が空の 3 つ組の zoom は、その長さの 0)
            tile_key("main", "flat", "STANDARD", 5, 6, 0),
            tile_key("main", "flat", "STANDARD", 10, 20, 0),
            tile_key("main", "flat", "STANDARD", 10, 20, 0),
            tile_key("main", "flat", "STANDARD", 10, 20, 0),
            // `_day` だけの名前は、日の地図ではない
            tile_key("main", "_day", "STANDARD", 10, 20, 0),
        ]
    );
}

#[tokio::test]
async fn bad_tile_paths_return_blank_png_without_querying() {
    let server = start_map(true).await;
    for path in [
        "/map/v1/tiles/main/../flat/0_0/1_1.png",
        "/map/v1/tiles/main/%2e%2e/flat/1_1.png",
        "/map/v1/tiles/%zz",
        "/map/v1/tiles/main/flat/0_0/%E3%81.png",
        "/map/v1/tiles/main/flat/0_0/abc_def.png",
        "/map/v1/tiles/main/flat/0_0/1.5_2.png",
        "/map/v1/tiles/main/flat/0_0/a_b_c_d.png",
        "/map/v1/tiles/main/flat/1_1.png",
        "/map/v1/tiles/main/flat/0_0/0_0/1_1.png",
        "/map/v1/tiles/main/flat/0_0/99999999999999999999_1.png",
    ] {
        let res = server.get(path).await;
        assert_eq!(res.status(), 200, "{path}");
        assert_eq!(h(&res, header::CONTENT_TYPE), "image/png", "{path}");
        assert_eq!(
            h(&res, header::CACHE_CONTROL),
            "public, max-age=60",
            "{path}"
        );
        assert_eq!(h(&res, header::ETAG), BLANK_ETAG, "{path}");
        assert_eq!(h(&res, header::ACCESS_CONTROL_ALLOW_ORIGIN), "*", "{path}");
        assert_eq!(res.bytes().await.unwrap().len(), 167, "{path}");
    }
    assert!(server.store.tile_queries.lock().unwrap().is_empty());
}

#[tokio::test]
async fn missing_tile_and_tile_without_image_are_blank_png() {
    let server = start_map(true).await;
    server
        .store
        .tiles
        .lock()
        .unwrap()
        .insert(tile_key("main", "t", "STANDARD", 1, 1, 0), tile(1, 0, None));
    for path in [
        "/map/v1/tiles/main/t/0_0/1_1.png",
        "/map/v1/tiles/main/t/0_0/999999_999999.png",
    ] {
        let res = server.get(path).await;
        assert_eq!(res.status(), 200);
        assert_eq!(h(&res, header::ETAG), BLANK_ETAG);
        assert_eq!(h(&res, header::CACHE_CONTROL), "public, max-age=60");
    }
    let res = server
        .get_with(
            "/map/v1/tiles/main/t/0_0/999999_999999.png",
            "If-None-Match",
            BLANK_ETAG,
        )
        .await;
    assert_eq!(res.status(), 304);
}

#[tokio::test]
async fn tile_db_failure_is_500() {
    let server = start_map(true).await;
    server.store.set_failing(true);
    let res = server.get("/map/v1/tiles/main/t/0_0/1_1.png").await;
    assert_eq!(res.status(), 500);
}

#[tokio::test]
async fn tiles_without_a_path_are_not_found() {
    let server = start_map(true).await;
    assert_eq!(server.get("/map/v1/tiles/").await.status(), 404);
    assert_eq!(server.get("/map/v1/tiles").await.status(), 404);
}

// ---- markers / icons / faces

#[tokio::test]
async fn marker_json_is_served_or_an_empty_object() {
    let server = start_map(true).await;
    server
        .store
        .markers
        .lock()
        .unwrap()
        .insert("main".into(), r#"{"sets":{}}"#.into());
    let res = server.get("/map/v1/tiles/_markers_/marker_main.json").await;
    assert_eq!(res.status(), 200);
    assert_eq!(h(&res, header::CONTENT_TYPE), "application/json");
    assert_eq!(h(&res, header::CACHE_CONTROL), "public, max-age=30");
    assert_eq!(
        h(&res, header::ETAG),
        format!("\"{}\"", md5_hex(br#"{"sets":{}}"#))
    );
    assert_eq!(res.text().await.unwrap(), r#"{"sets":{}}"#);

    // 無ければ `{ }` (スペースあり)
    let res = server
        .get("/map/v1/tiles/_markers_/marker_nosuch.json")
        .await;
    assert_eq!(res.status(), 200);
    assert_eq!(h(&res, header::CONTENT_TYPE), "application/json");
    assert_eq!(res.text().await.unwrap(), "{ }");

    // エンコードされた名前は 1 度だけ戻す (`%25` は二重に戻さない)
    server.store.marker_queries.lock().unwrap().clear();
    server
        .get("/map/v1/tiles/_markers_/marker_%6Dain.json")
        .await;
    server
        .get("/map/v1/tiles/_markers_/marker_%256Dain.json")
        .await;
    assert_eq!(
        server.store.marker_queries.lock().unwrap().clone(),
        vec!["main".to_string(), "%6Dain".to_string()]
    );
}

#[tokio::test]
async fn marker_icon_is_png_or_blank() {
    let server = start_map(true).await;
    server
        .store
        .icons
        .lock()
        .unwrap()
        .insert("IC".into(), b"icon-bytes".to_vec());
    let res = server.get("/map/v1/tiles/_markers_/IC.png").await;
    assert_eq!(res.status(), 200);
    assert_eq!(h(&res, header::CONTENT_TYPE), "image/png");
    assert_eq!(h(&res, header::CACHE_CONTROL), "public, max-age=3600");
    assert_eq!(res.bytes().await.unwrap().as_ref(), b"icon-bytes");
    // 拡張子は何でもよい (最後の `.` 以降を取る)
    assert_eq!(
        server
            .get("/map/v1/tiles/_markers_/IC.jpg")
            .await
            .bytes()
            .await
            .unwrap()
            .as_ref(),
        b"icon-bytes"
    );

    let res = server.get("/map/v1/tiles/_markers_/nosuch.png").await;
    assert_eq!(res.status(), 200);
    assert_eq!(h(&res, header::ETAG), BLANK_ETAG);
    assert_eq!(h(&res, header::CACHE_CONTROL), "public, max-age=60");
}

#[tokio::test]
async fn face_types_and_name_cutoff() {
    let server = start_map(true).await;
    for (type_id, body) in [(0, "8"), (1, "16"), (2, "32"), (3, "body")] {
        server
            .store
            .faces
            .lock()
            .unwrap()
            .insert(("Steve".into(), type_id), body.as_bytes().to_vec());
    }
    for (dir, body) in [
        ("8x8", "8"),
        ("16x16", "16"),
        ("32x32", "32"),
        ("body", "body"),
    ] {
        let res = server
            .get(&format!("/map/v1/tiles/faces/{dir}/Steve.png"))
            .await;
        assert_eq!(res.status(), 200, "{dir}");
        assert_eq!(h(&res, header::CONTENT_TYPE), "image/png");
        assert_eq!(h(&res, header::CACHE_CONTROL), "public, max-age=3600");
        assert_eq!(res.text().await.unwrap(), body, "{dir}");
    }
    // 名前は最初の `.` まで。種別が未知なら 8x8
    let res = server.get("/map/v1/tiles/faces/zzz/Steve.extra.png").await;
    assert_eq!(res.text().await.unwrap(), "8");
    // 存在しなければ blank.png
    let res = server.get("/map/v1/tiles/faces/8x8/Nobody.png").await;
    assert_eq!(h(&res, header::ETAG), BLANK_ETAG);
    assert_eq!(h(&res, header::CACHE_CONTROL), "public, max-age=60");
    // 段の数が違えば、顔ではなくタイルの形として読む (4 段でないので blank.png)
    let res = server.get("/map/v1/tiles/faces/8x8").await;
    assert_eq!(h(&res, header::ETAG), BLANK_ETAG);
}

// ---- 経路の分離

#[tokio::test]
async fn other_methods_are_refused() {
    let server = start_map(true).await;
    let res = server
        .client
        .post(server.url("/map/v1/configuration"))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(h(&res, header::ALLOW), "GET, HEAD");
}

#[tokio::test]
async fn head_is_served_like_get_without_a_body() {
    let server = start_map(true).await;
    let res = server
        .client
        .head(server.url("/map/v1/configuration"))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    assert_eq!(res.bytes().await.unwrap().len(), 0);
}

#[tokio::test]
async fn map_api_is_off_without_a_database() {
    let server = common::start().await;
    let res = server.get("/map/v1/configuration").await;
    assert_eq!(res.status(), 404);
    assert_eq!(
        res.json::<serde_json::Value>().await.unwrap()["error"],
        "Not Found"
    );
}

#[tokio::test]
async fn live_routes_do_not_exist_on_the_public_listener() {
    let server = start_map(true).await;
    for path in ["/live/plugin", "/live/clients", "/live/"] {
        let res = server
            .client
            .get(server.url(path))
            .header("X-Toro-Token", LIVE_TOKEN)
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 404, "{path}");
    }
    // WebSocket の Upgrade でも、公開の待受は /live/plugin を受けない
    let res = server
        .client
        .get(server.url("/live/plugin"))
        .header("X-Toro-Token", LIVE_TOKEN)
        .header("Connection", "Upgrade")
        .header("Upgrade", "websocket")
        .header("Sec-WebSocket-Version", "13")
        .header("Sec-WebSocket-Key", "dGhlIHNhbXBsZSBub25jZQ==")
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 404);
    let res = server.get("/healthz").await;
    assert_eq!(res.status(), 200);
}

#[tokio::test]
async fn existing_routes_keep_working_with_the_map_api_on() {
    let server = start_map(true).await;
    let res = server.get("/").await;
    assert_eq!(res.status(), 200);
    assert_eq!(h(&res, header::X_CONTENT_TYPE_OPTIONS), "nosniff");
    assert_eq!(server.get("/openapi.json").await.status(), 200);
    assert_eq!(server.get("/no-such-route").await.status(), 404);
}
