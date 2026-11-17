# 看板の共有ロジックの試験ベクタ

3D 地図の看板 (MarumaSign) の URL の確かめ方、再生リストからの URL の取り出し、中身の種類の判別などは、
画面側のリポジトリ (TORO-3D-WebMap の `app/bluemap/marumasign/`) の TypeScript が正本です。
この API は同じ処理を Rust で持っているので、2 つが同じ結果を返すことを、入力と期待する出力の組 (試験ベクタ) で確かめます。

- ベクタ: `tests/vectors/marumasign-shared.json`
- 生成するスクリプト: `tools/gen-sign-vectors.mjs`
- 読んで確かめる試験: `tests/sign_vectors.rs` (`cargo test --test sign_vectors`)

## ベクタの作り直し

**ベクタは凍結済みです。** サーバー側のコード (`server/lib/marumasign/`) は TORO-WebMap から除去され、最後の状態は TORO-WebMap のタグ `pre-server-removal` にあります。サーバー側の項目の再生成は、そのタグをチェックアウトしたリポジトリに対して行います (2 つ目の引数)。

TypeScript を直したときや、ケースを足したいときは、次のとおり作り直し、できた JSON をコミットします。

```sh
# TORO-API のルートで。1 つ目は画面側のリポジトリ、2 つ目 (省略できる) は TORO-WebMap (タグ `pre-server-removal` を取得済みのもの)
node tools/gen-sign-vectors.mjs <TORO-3D-WebMap のパス> <TORO-WebMap のパス>
```

- Node.js 22.18 以降 (TypeScript をそのまま読み込めるバージョン)。古いバージョンでは `node --experimental-strip-types` で実行します。
- 1 つ目の引数のリポジトリの `app/bluemap/marumasign/{core,manifest,xml-head}.ts` から、`extensionOf`、`safeUrl`、`manifestRefs`、`sniff` などの入出力を作ります。
- 2 つ目の引数のリポジトリのタグ `pre-server-removal` の `server/lib/marumasign/{allow,kinds,limits,net}.ts` (git で取り出す。main や作業ツリーは読まない) からは、許可リスト、種類の判別、アドレスの検査、上限の鍵の入出力を作ります。
- 出力先を変えるときは `--out <ファイル>` を付けます。
- ベクタには、元になった TypeScript のファイルのハッシュが入ります。ハッシュが変わったら、ベクタを作り直して Rust の試験を通します。

試験が失敗したときは、Rust の移植が TypeScript と食い違っています。TypeScript を正として Rust を直してください。
