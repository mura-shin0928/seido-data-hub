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
| `crates/pipeline` | データの取り込み・更新を行う CLI。取得の結果は `pipeline::persist::record` が履歴・資源の状態・ジョブの完了とともに1つのトランザクションで保存する。`crawl` で巡回を回す |
| `crates/api` | 読み取り専用の HTTP API（axum。Lambda でもローカルでも同じ Router） |

## ローカル開発

```bash
docker compose up -d
cp .env.example .env
cargo run -p migration -- up
cargo run -p pipeline -- import-registry
```

API をローカルで動かす（`http://127.0.0.1:3000`）:

```bash
cargo run -p api
curl 'http://127.0.0.1:3000/v1/areas/132101/programs?age_months=6&category=003'
```

`import-registry` は既定で東京都のサーバーから JSON（約90MB）を取得する。手元のファイルを使うときは
`--source <path>` を渡す。psid で上書きするので何度流してもよい。
タグの値は3桁のコードにそろえて入れ（`"002，003"` → `002` と `003`、`"86"` → `086`）、
タグの一覧に無いコードは警告としてログに出す（取り込みは止めない）。

URL を1件ずつ取得して結果を見る（DB は使わない。robots.txt と同一ホスト2秒の間隔を守る）:

```bash
cargo run -p pipeline -- fetch https://www.city.koganei.lg.jp/kenkofukuhsi/431/kyujitusinryokyukyu/kyuujitu.html
```

転送の各段・status・文字コードの判定元・`ETag` を表示する。`--etag` / `--last-modified` を渡すと条件付きで取得する。
HTML なら本文を取り出し、本文コンテナを決めた規則・タイトル・ページに書かれた更新日・`rel=canonical`・リンク数と、
5種のハッシュ（`raw_hash` / `page_hash` / `title_hash` / `body_hash` / `links_hash`）も表示する。
同じ URL を2回渡すと、`body_hash` が再取得で変わらないかを確かめられる。
代表 URL（`canonical_url`）とその根拠（恒久転送の先・検証を通った `rel=canonical`・取りに行った URL）、
採らなかった転送や canonical の理由も表示する（DB を使わないので、ホスト単位の canonical の判定は当てない）。
許可リストは渡した URL のホストだけなので、それ以外のホストへの転送は追わずに転送先を表示する。

時刻の来た URL を取得して DB に記録する（`import-registry` 済みの DB に対して流す）:

```bash
cargo run --release -p pipeline -- crawl --kind sweep
```

同時16件・同一ホストは1件ずつ2秒間隔で、robots.txt を守る。実行は `crawl_runs` に `--kind`（既定 `manual`）で残り、
正常に終えたときだけ `finished_at` が入る。取得の履歴は `fetch_history` に URL ごとに1行（応答の status・Content-Type・
文字コード・資源への観測・エラーの理由）。次に取るまでの間隔は、内容が変わったら半分・変わらなければ1.5倍にし、HTML は3〜14日・PDF は14〜90日・見つからないページは3〜7日・取れない（403・robots）ページは7〜30日に収める（初めは HTML 7日・PDF 30日）。時刻が来るまでは取らないので、
続けて流しても取り直さない。途中で止めたら、10分（lease の長さ）後に流し直せば続きから進む。

終わりに実行サマリー（ホスト別の status・304 の割合・変更率・404 など）を出し、`crawl_runs` の `stats`・`alerts` にも残す。
Actions では `GITHUB_STEP_SUMMARY` に追記する。429・5xx の急増・全件失敗・変更率50%超・lease 切れの多発に当たったら、
記録したうえで終了コード1で終える。過去の実行のサマリーは `run-report` で見る（DB には書かない）:

```bash
cargo run -p pipeline -- run-report            # 最新の実行
cargo run -p pipeline -- run-report --run <id> # 実行を指定
```

テスト（統合テストは `TEST_DATABASE_URL` のデータベースを毎回作り直す）:

```bash
docker compose exec db psql -U postgres -c "create database seido_data_hub_test"
TEST_DATABASE_URL=postgres://postgres:postgres@localhost:55432/seido_data_hub_test cargo test
```

スキーマを変えたら、ローカルに `migration up` したあとエンティティを作り直す:

```bash
sea-orm-cli generate entity -u "$DATABASE_URL" -o crates/entity/src --lib --ignore-tables seaql_migrations
```

## API

| エンドポイント | 返すもの |
|---|---|
| `GET /v1/areas` | 自治体の一覧（code / name / parent_code） |
| `GET /v1/areas/{code}/programs?age_months=&category=` | その自治体と都道府県の制度（市区町村が先、UM 順）。月齢の上下限が無い制度はどの月齢でも返す |
| `GET /v1/programs/{id}` | 1件の詳細（レジストリの行 `registry` 込み） |
| `GET /v1/tags` | タグのコードと名前（`categories` / `targets` / `contents`。レジストリ README §3） |

データは `{"data": ..., "attribution": {...}}` で返し、`attribution` に出典表記（CC BY 4.0）を入れる。
エラーは `{"error": {"message": ...}}`。

main に入ると CI が migration の後に Lambda（`seido-data-hub-api`、ap-northeast-1）へデプロイする。
初回だけ要る AWS・Supabase・GitHub の設定は [docs/deploy-api.md](docs/deploy-api.md)。

## ライセンス

コードは [MIT](LICENSE-MIT) または [Apache-2.0](LICENSE-APACHE) のどちらかを選んで利用できます。
データは上記「データの出典」のとおり CC BY 4.0 です。
