# seido-data-hub

自治体コードを1つ指定するだけで、国・都道府県・市区町村をまたいで子育ての制度・手続きを引けるデータハブ。

子育ての制度は、市区町村・都道府県・各省庁と見る先が多く、探しきれない。このリポジトリは、
東京都が公開している子育て支援制度レジストリを土台に、各制度の公式ページとの差分を反映したデータを
API として提供する（予定）。

## データの出典

東京デジタル2030ビジョン（こどもDX）子育て支援制度レジストリ（東京都・GovTech東京）を改変して利用しています。
[CC BY 4.0](https://creativecommons.org/licenses/by/4.0/deed.ja)。
`crates/domain/tests/fixtures/registry_sample.json` はこのデータの抜粋です。

各制度の内容は、必ず各自治体の公式サイトで確認してください。

## 構成

| crate | 役割 |
|---|---|
| `crates/domain` | DB も HTTP も知らない純粋ロジック（レジストリの読み取り・月齢の変換など） |
| `crates/entity` | SeaORM のエンティティ（`sea-orm-cli generate entity` で DB から生成） |
| `crates/migration` | スキーマ（SQL を SeaORM の migration で流す） |
| `crates/pipeline` | データの取り込み・更新を行う CLI |

## ローカル開発

```bash
docker compose up -d
cp .env.example .env
cargo run -p migration -- up
cargo run -p pipeline -- import-registry
```

`import-registry` は既定で東京都のサーバーから JSON（約90MB）を取得する。手元のファイルを使うときは
`--source <path>` を渡す。psid で上書きするので何度流してもよい。

テスト（統合テストは `TEST_DATABASE_URL` のデータベースを毎回作り直す）:

```bash
docker compose exec db psql -U postgres -c "create database seido_data_hub_test"
TEST_DATABASE_URL=postgres://postgres:postgres@localhost:55432/seido_data_hub_test cargo test
```

スキーマを変えたら、ローカルに `migration up` したあとエンティティを作り直す:

```bash
sea-orm-cli generate entity -u "$DATABASE_URL" -o crates/entity/src --lib --ignore-tables seaql_migrations
```
