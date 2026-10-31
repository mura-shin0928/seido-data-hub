# 人が判定した移転先を反映する

## 何をするか

family-app の一覧から外している制度（主たる URL を開けず、生きた関連リンクも無い市区町村の制度）のうち、
移転先を人が確かめたものを、本番の DB に反映して一覧に戻す。判定は `data/reviews/*.json` に置き、
GitHub Actions の `Apply reviews`（`.github/workflows/apply-reviews.yml`）を手で流す。

ワークフローは3段で動く。

1. `pipeline apply-reviews register <file>`: `hit`・`index` の候補の URL を `urls` に入れる（既にある行は触らない）
2. `pipeline crawl --kind reviews`: 登録したばかりの候補を取る（`crawl_runs` には `reviews` で残る）
3. `pipeline apply-reviews link <file>`: 候補の資源が `active` なら反映する

| 判定 | 反映のしかた | family-app での見え方 |
|---|---|---|
| `hit`（同じ制度の今のページ） | その制度の `program_urls` の `main` を候補の URL に差し替え、`source = 'discovered'` にする | 候補のページが公式ページとして、注記なしで出る |
| `index`（制度へのリンクがある一覧ページ） | `program_urls` に `role = 'related'`・`source = 'discovered'` で加える | 「公式ページを開けませんでした」と、一覧ページへのリンク |
| `none`（どれも違う） | 何もしない | 一覧から外したまま |

差し替えは制度ごとの処理で、資源の状態には触らない。元の URL を共有する制度があっても、判定した制度だけが変わる。
1つの古いページに複数の制度が載り、移転先が制度ごとに分かれていることがあるため。

`link-related` は死んだ制度の関連リンクを毎週作り直すが、`source = 'discovered'` の行は消さない。
`import-registry` も、`source = 'discovered'` の `main` がある制度は作り直さない。

## 判定のファイル

```json
{
  "reviewed_at": "2026-10-28",
  "reviews": [
    { "program_id": "…", "dead_url": "https://www.city.chofu.lg.jp/050010/p027009.html",
      "verdict": "hit", "url": "https://www.city.chofu.lg.jp/050060/p027009.html" },
    { "program_id": "…", "dead_url": "…", "verdict": "index", "url": "…" },
    { "program_id": "…", "dead_url": "…", "verdict": "none" }
  ]
}
```

- `dead_url` は判定したときの主たる URL（`urls.normalized_url`）。流す時点で主たる URL が違えば、その件は反映しない
- `url` は `hit`・`index` に必須で、`none` には付けない。同じ制度は1回だけ
- 入れるのは URL・制度 id・判定だけ（ページの見出しや本文は入れない）
- 誤りがあれば、DB に触る前に全件をまとめてエラーにする

## 流し方

1. 判定のファイルを PR にしてマージする
2. `Apply reviews` を `dry_run: true`（既定）で流す。DB には書かず、登録と反映の件数だけを表示する。
   巡回は流さないので、`link` は新しい候補がすべて「候補をまだ取っていない」になる
3. 件数を見て、`dry_run: false` で流す

**月曜（定期の巡回の日）には流さない**。巡回と同じ `concurrency` のグループなので同時には走らないが、
巡回の後ろに待たされ、`crawl` が巡回の残りまで取ることになる。

何度流してもよい（登録・反映は、既にあれば何もしない。反映済みの件は「反映済み」に数える）。

## 報告の読み方

`link` は件数（移転・関連リンク・反映済み・none・反映しなかった・書けなかった）と、反映しなかった判定を理由ごとに出す。
Actions では実行サマリーにも足す。書けなかった判定（DB のエラー）が1件でもあれば、終了コード1で終える。

| 理由 | 意味 | どうするか |
|---|---|---|
| 候補をまだ取っていない | 候補の URL に資源が無い | 試し流しなら正常。本番なら、`crawl` で取れなかった（robots・通信エラー）。次の週の巡回の後に流し直す |
| 候補が active でない | 候補のページが 404 などになった | 判定し直す |
| 主たる URL が判定のときと違う | レジストリを取り込み直して主たる URL が変わった | 判定し直す |
| 元の資源が死んでいない | 主たる URL が生き返った、または既に別の移転先へ移った | 何もしない（判定の前提が崩れている） |
| 候補の資源が moved | 候補のページ自体が、さらに転送されている | 転送先で判定し直す |
| 候補の資源が元の資源と同じ | 候補が元の URL と同じページ | 判定の誤り。`none` にする |

## 流した後の確かめ方

一覧から外している市区町村の制度を数え直す。流す前より、`hit`・`index` で反映した件数だけ減る。

```sql
WITH linked AS (
    SELECT pu.program_id, pu.role, pu.rank, r.state
    FROM program_urls pu
    LEFT JOIN LATERAL (
        SELECT ur.resource_id FROM url_resources ur
        WHERE ur.url_id = pu.url_id ORDER BY ur.observed_at DESC LIMIT 1
    ) cur ON TRUE
    LEFT JOIN resources r ON r.id = cur.resource_id
)
SELECT count(*)
FROM programs p
JOIN areas a ON a.code = p.area_code
JOIN linked m ON m.program_id = p.id AND m.role = 'main'
WHERE a.parent_code IS NOT NULL
  AND m.state IN ('deleted', 'deletion_candidate')
  AND NOT EXISTS (
      SELECT 1 FROM linked x
      WHERE x.program_id = p.id AND x.role = 'related' AND x.state = 'active'
  );
```
