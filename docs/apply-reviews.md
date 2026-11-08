# 人が判定した移転先を反映する

## 何をするか

主たる URL を開けず、生きた関連リンクも無い市区町村の制度のうち、
移転先を人が確かめたものを、本番の DB に反映する。判定は `data/reviews/*.json` に置き、
GitHub Actions の `Apply reviews`（`.github/workflows/apply-reviews.yml`）を手で流す。

ワークフローは3段で動く。

1. `pipeline apply-reviews register <file>`: `hit`・`index` の候補の URL を `urls` に入れる（既にある行は触らない）
2. `pipeline crawl --kind reviews`: 登録した候補を取得する。定期の巡回と同じ処理なので、取得の時期が来ている他の URL も取得する（`crawl_runs` の `kind` は `reviews`）
3. `pipeline apply-reviews link <file>`: 候補の資源が `active` なら反映する

| 判定 | 反映のしかた | API が返すもの |
|---|---|---|
| `hit`（同じ制度の今のページ） | その制度の `program_urls` の `main` を候補の URL に差し替え、`source = 'discovered'` にする | `url` が候補のページになり、`state` は `active` |
| `index`（制度へのリンクがある一覧ページ） | `program_urls` に `role = 'related'`・`source = 'discovered'` で加える | `state` は元のまま（`deleted`・`deletion_candidate`）で、`related_links` に一覧ページが入る |
| `none`（どれも違う） | 何もしない | 変わらない |

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
      "verdict": "hit", "url": "https://www.city.chofu.lg.jp/050060/p027009.html", "rejected": ["…"] },
    { "program_id": "…", "dead_url": "…", "verdict": "index", "url": "…", "rejected": ["…", "…"] },
    { "program_id": "…", "dead_url": "…", "verdict": "none", "rejected": ["…", "…", "…"] }
  ]
}
```

- `dead_url` は判定したときの主たる URL（`urls.normalized_url`）。流す時点で主たる URL が違えば、その件は反映しない
- `url` は `hit`・`index` に必須で、`none` には付けない。同じ制度は1回だけ
- `rejected` は、見せた候補のうち選ばなかった URL の一覧。`hit`・`index` で選ばなかった候補も、`none` の候補もすべて入れる。省略できる（`2026-10-moves.json` はそのまま読める）。
  `apply-reviews` は URL として読めること・`dead_url` と同じホストであることを確かめるだけで、DB には何もしない。誤りは `url` と同じく、DB に触る前にまとめてエラーにする
- ファイルは回ごとに `data/reviews/<日付>-moves.json` と名付ける。同じ制度が別の回のファイルにまた出てよい（同じファイルの中では1回だけ）
- 入れるのは URL・制度 id・判定だけ（ページの見出しや本文は入れない）
- 誤りがあれば、DB に触る前に全件をまとめてエラーにする

## ホスト移行として差し替える

旧ホストが応答しなくなり、**同じパス・同じクエリのページが新しいホストにある**と人が確かめたときだけ、`hit` に `"host_move": true` を書く。

```json
{ "program_id": "…", "dead_url": "https://www.library.city.kita.tokyo.jp/viewer/info.html?id=8",
  "verdict": "hit", "url": "https://www.library.city.kita.lg.jp/viewer/info.html?id=8", "host_move": true }
```

- ふだんの判定は、候補のホストが `dead_url` と違うと読む段階で拒む（候補を入れると、そのホストが巡回の許可リストに加わるため）。`host_move` はその例外で、`hit` だけに書ける。ホストが同じ・パスかクエリが違うものは拒む
- `register` は候補の URL と一緒に、その移行を `host_moves` に承認（`approved`）として入れる。転送を観測していなくてよい（旧ホストが消えた後は、もう観測できない）。新しいホストは巡回の許可リストに加わる
- **PR には確かめた根拠を書く**: 新しいホストの同じパスのページが開けること、タイトルが制度と合うこと、分かっていれば旧ホストがそこへ転送していたこと
- `link` は、承認の行があるときだけ差し替える。旧ホストの URL は一度も取れておらず資源が無いことが多いので、元の資源が無くても差し替える（元の資源があれば、今までどおり `deleted`・`deletion_candidate` のときだけ）
- 差し替えでどの制度からも使われなくなった元の URL は、`urls.status` を `retired` にする。`retired` の URL は巡回されず、実行サマリーの待ち行列にも数えない。行と取得の履歴は残る。別の制度がまだ使っている URL は、そのまま巡回する
- 途中の `crawl` で旧ホストの URL も取られ、ホストごと見送りの警告でその段が失敗と表示されることがある。`link` はそのまま流れ、流し終えた後の巡回では旧ホストの URL は取られない

## 候補を取る

定期の巡回の最後の段 `suggest-moves` が、主たる URL が死んだ制度（`deleted`・`deletion_candidate`）の移転先の候補を、
同じホストの生きた一覧ページのリンクから作り、`move-candidates.json` を成果物 `move-candidates` に上げる（90日残る）。
月の最初の定期実行の後に、実行サマリーの「まだ確かめていない候補がある制度」を見て、1件以上なら取る。

```bash
gh run download <run id> -n move-candidates
```

中身は、候補のある制度ごとに、制度 id・自治体・制度名（`canonical_name`・`short_name`）・死んだ主たる URL（`dead_url`）と、
候補（上位3件。URL・`dedup_key`・リンク文字列・近さ・どの一覧ページにあったか）、4件目以降を切った数（`more`）。
候補は制度名とリンク文字列の近さの順で、ページの本文は入らない。人が候補のページを開いて、同じ制度の今のページか、制度へのリンクがある一覧ページかを判定する。

- 確かめ済みの（制度・URL）の組は出ない。`data/reviews/*.json` すべての `url` と `rejected` を読み、`dedup_key` で比べる。マージ済みの判定が次の実行から効く
- 「後で」にした候補は記録しないので、次の回にまた出る
- `rejected` の無い `none`（`2026-10-moves.json` の `none`）は、どの候補が違ったかの記録が無いので、候補が一度だけまた出る。そのとき `rejected` 付きで判定し直す
- `hit` を反映できなかった制度（候補がまだ取れていない、など）は、同じ判定のファイルで `Apply reviews` を流し直す。その候補は確かめ済みなので、候補のファイルには出ない

判定したら、`data/reviews/<日付>-moves.json` を書く（見せた候補のうち選ばなかった URL を `rejected` に入れる）。PR にしてマージし、下の手順で `Apply reviews` を流す。

手元で試すときは、本番の DB を読むだけで書かない（`--reviews` の既定は `data/reviews`、`--out` の既定は `move-candidates.json`）。

```bash
cargo run -p pipeline -- suggest-moves --out move-candidates.json
```

## 流し方

1. 判定のファイルを PR にしてマージする
2. `Apply reviews` を `dry_run: true`（既定）で流す。DB には書かず、登録と反映の件数だけを表示する。
   巡回は流さないので、`link` は新しい候補がすべて「候補をまだ取っていない」になる
3. 件数を見て、`dry_run: false` で流す

**月曜（定期の巡回の日）には流さない**。巡回と同じ `concurrency` のグループなので同時には走らないが、
巡回の後ろに待たされ、`crawl` が巡回の残りまで取ることになる。

何度流してもよい（登録・反映は、既にあれば何もしない。反映済みの件は「反映済み」に数える）。

巡回で警告（robots.txt が読めないホストなど）が出るとジョブは失敗と表示されるが、最後の `link` は実行される。反映の件数は `link` の出力で確認する。

## 報告の読み方

`register` は候補の URL の件数（新規・既存）・none・ホスト移行の承認の件数を出す。
`link` は件数（移転・関連リンク・反映済み・none・巡回から外した元の URL・反映しなかった・書けなかった）と、反映しなかった判定を理由ごとに出す。
「巡回から外した元の URL」は、ホスト移行で差し替えて `retired` にした URL の数。
Actions では実行サマリーにも足す。書けなかった判定（DB のエラー）が1件でもあれば、終了コード1で終える。

| 理由 | 意味 | どうするか |
|---|---|---|
| 候補をまだ取っていない | 候補の URL に資源が無い | 試し流しなら正常。本番なら、`crawl` で取れなかった（robots・通信エラー）。次の週の巡回の後に流し直す |
| 候補が active でない | 候補のページが 404 などになった | 判定し直す |
| 主たる URL が判定のときと違う | レジストリを取り込み直して主たる URL が変わった、または前回の実行で別の候補に差し替え済み | 判定し直す。差し替え済みの制度は、下の「誤った判定を戻す」のあとで流し直す |
| 元の資源が死んでいない | 主たる URL が生き返った、または巡回が転送で移した | 何もしない（判定の前提が崩れている） |
| 候補の資源が moved | 候補のページ自体が、さらに転送されている | 転送先で判定し直す |
| 候補の資源が元の資源と同じ | 候補が元の URL と同じページ | 判定の誤り。`none` にする |
| ホスト移行が承認されていない | `host_move` の判定で、`host_moves` に承認の行が無い | 試し流しなら正常（`register` の書き込みを戻すため）。本番なら `register` が流れたかを確かめる |

## 誤った判定を戻す

`hit` で差し替えた制度は、`import-registry` を流しても元に戻らない。戻すときは、その制度の行をレジストリ由来に戻してから `Import registry` を流す。主たる URL がレジストリの値に戻る。

```sql
UPDATE program_urls SET source = 'registry'
WHERE program_id = '<制度の id>' AND role = 'main';
```

ホスト移行で差し替えた制度は、上のあとで、巡回から外した元の URL も戻す（`dead_url` の値で引く）。新しいホストの承認（`host_moves` の行）は残しても害は無い。

```sql
UPDATE urls SET status = 'failed_final', next_crawl_at = now()
WHERE normalized_url = '<dead_url>' AND status = 'retired';
```

`index` で足した関連リンクは、その行を消す（`DELETE FROM program_urls WHERE program_id = '<制度の id>' AND url_id = '<URL の id>'`）。

レジストリがあとで正しい URL に変わっても、差し替えた制度には入らず、取り込みの出力にも出ない（レジストリは更新が止まっている）。

## 流した後の確かめ方

主たる URL を開けず、生きた関連リンクも無い市区町村の制度を数え直す。流す前より、`hit`・`index` で反映した件数だけ減る。

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
