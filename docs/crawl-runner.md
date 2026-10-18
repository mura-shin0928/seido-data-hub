# 巡回のランナー（初回の設定）

週1の巡回（`.github/workflows/crawl.yml`）は、GitHub のランナーではなく、東京（`ap-northeast-1`）の AWS CodeBuild を
GitHub Actions の自前のランナーにして動かす。DB（Supabase）と同じ東京で動かし、1件ごとの DB との往復を短くするため
（米国のランナーでは往復に百数十ミリ秒かかり、毎分100件前後で頭打ちになっていた）。

ジョブが待ちに入ると、GitHub が CodeBuild に webhook を送り、CodeBuild が使い捨てのコンテナを起動する。コンテナはこのジョブ専用の
ランナーとして GitHub に登録され、steps を流して消える。steps・secret・ログ・実行サマリー・成否と通知は GitHub のまま。
GitHub から AWS への認証は使わない（アクセスキーも OIDC のロールも置かない）。接続文字列も AWS に置かない。

以下は最初に一度だけ手で行う。リージョンはすべて **アジアパシフィック（東京）`ap-northeast-1`**。

## 1. GitHub との接続

CodeBuild がランナーを GitHub に登録するために使う。個人のトークンを AWS に置かないよう、GitHub App で繋ぐ。

1. コンソール → Developer Tools → 設定 → 接続 → 接続を作成。プロバイダーは GitHub、名前の例 `seido-data-hub-github`
2. 「新しいアプリをインストールする」で AWS Connector for GitHub を入れる。入れる先のリポジトリは
   **`mura-shin0928/seido-data-hub` だけ**を選ぶ（Only select repositories）
3. 接続の状態が「利用可能」になるまで進める。接続の ARN を控える
4. GitHub の Settings → Applications → Installed GitHub Apps に **AWS Connector for GitHub** が出ていることを確かめる。
   GitHub での認可だけで済ませると、接続は「利用可能」になるのにアプリはリポジトリに入らず、4 の webhook を作るときに
   `OAuthProviderException`（Failed to create webhook）で失敗する。出ていなければ
   `https://github.com/apps/aws-connector-for-github/installations/new` から、このリポジトリだけに入れる

## 2. CodeBuild のロール

- 信頼されたエンティティ: AWS のサービス → CodeBuild
- 名前の例: `seido-data-hub-crawl-codebuild`
- 許可ポリシー（このプロジェクトのログを書くことと、1 の接続を使うことだけ）:

```json
{
  "Version": "2012-10-17",
  "Statement": [
    {
      "Effect": "Allow",
      "Action": ["logs:CreateLogGroup", "logs:CreateLogStream", "logs:PutLogEvents"],
      "Resource": [
        "arn:aws:logs:ap-northeast-1:<account-id>:log-group:/aws/codebuild/seido-data-hub-crawl",
        "arn:aws:logs:ap-northeast-1:<account-id>:log-group:/aws/codebuild/seido-data-hub-crawl:*"
      ]
    },
    {
      "Effect": "Allow",
      "Action": ["codeconnections:GetConnection", "codeconnections:GetConnectionToken"],
      "Resource": "<1 の接続の ARN>"
    }
  ]
}
```

コンソールでプロジェクトを作るときに「新しいサービスロール」を選ぶと、S3 などへの許可も付いたロールができる。
それは使わず、このロールを「既存のサービスロール」で選ぶ（コンソールにロールの変更を任せるチェックは外す）。

## 3. CodeBuild のプロジェクト

CodeBuild → ビルドプロジェクト → ビルドプロジェクトを作成。

| 項目 | 値 | 補足 |
|---|---|---|
| プロジェクト名 | `seido-data-hub-crawl` | `crawl.yml` の `runs-on:`（`codebuild-seido-data-hub-crawl-…`）と合わせる。違うとジョブが待ったままになる |
| プロジェクトのタイプ | ランナープロジェクト（Runner project） | |
| ランナー | プロバイダー GitHub、場所はリポジトリ、`https://github.com/mura-shin0928/seido-data-hub` | 認証情報はカスタムのソース認証情報 → GitHub App → 1 の接続 |
| 環境 | マネージド型イメージ、EC2、Ubuntu、`aws/codebuild/standard:8.0`（Ubuntu 24.04） | いまの `ubuntu-latest`（Ubuntu 24.04）と同じ版。rust-cache のキーには Ubuntu の版が入らず、GitHub のランナーで作ったキャッシュがそのまま戻るので、版を揃えないとビルドの成果物が合わずに落ちうる |
| コンピューティング | `general1.small`（2 vCPU・3GB）、x86_64 | 仮置き（下を参照） |
| ホストカーネル（作った後の「編集」→ 環境 → 追加設定） | Amazon Linux 2023（項目が無ければ既定のまま） | コンテナを載せる側の OS。AWS が Amazon Linux 2 からの移行を勧めている。ジョブが動くのは上のイメージ（Ubuntu）の中なので、巡回には影響しない |
| サービスロール | 既存のサービスロール → 2 のロール | |
| タイムアウト | 180分 | 仮置き（下を参照） |
| 同時ビルドの上限 | 1（「このプロジェクトが開始できる同時ビルドの数を制限」） | 巡回が同時に2つ走らないことを AWS 側でも守る。悪用されても1台分にとどめる |
| VPC | 付けない | 既定で外のインターネットに出られる。NAT Gateway は要らない |
| ログ | CloudWatch Logs を有効にする（グループ `/aws/codebuild/seido-data-hub-crawl`） | |
| buildspec | 何も書かない | ランナープロジェクトでは使われない |

ロググループは最初のビルドで初めてできるので、**プロジェクトを作る前に** CloudWatch Logs でロググループ `/aws/codebuild/seido-data-hub-crawl` を
作り、保持期間を **30日** にしておく（仮置き）。先に作らないと、保持は無期限のまま残る。

仮置きの根拠と、確定の時期:

- `general1.small`・x86_64: いまの GitHub のランナーと同じ x86 にして、差を場所だけに絞る。arm と無料枠（月100分）は同じ。
  毎回のビルドが遅すぎれば見直す。CodeBuild での数回の定期実行を見てから決める
- タイムアウト180分: ワークフローの `timeout-minutes: 120` より長くし、打ち切りは GitHub 側で起こす（失敗の形をいまと変えない）。同上
- ログの保持30日: API の Lambda と同じ。同上

## 4. webhook の条件

プロジェクトの「プライマリソースの Webhook イベント」で、条件の組（フィルターグループ）を **1つだけ** 作り、次の3つを入れる。
同じ組に入れた条件は、すべて満たしたときだけビルドを起動する。

| 種類 | パターン |
|---|---|
| イベント（`EVENT`） | `WORKFLOW_JOB_QUEUED` だけ |
| ワークフロー名（`WORKFLOW_NAME`） | `^Crawl$` |
| アクターの ID（`ACTOR_ACCOUNT_ID`。コンソールの表示は `ACTOR_ID`） | `^122416071$` |

`122416071` は GitHub アカウント `mura-shin0928` の ID（`curl -s https://api.github.com/users/mura-shin0928` の `id`）。

公開リポジトリなので、他人がフォークからの PR でワークフローを書き換え、`runs-on:` にこのプロジェクトの名前を書いてくることがありうる。
起動した人が自分でなければ条件に合わず、CodeBuild は起動しない。

作ると、GitHub のリポジトリの Settings → Webhooks に、Workflow jobs のイベントを送る webhook が1つ増える。

## 5. GitHub のリポジトリ設定

Settings → Actions → General → 「Approval for running fork pull request workflows from contributors」を
**Require approval for all external contributors** にする。外部の人のフォークからの PR では、ワークフローを動かす前に毎回承認を求める。

この承認は `pull_request_target` で起きるワークフローには効かない。このリポジトリのワークフローに `pull_request_target` を足さない。

守りは、4 の webhook の条件と、この承認と、ランナーの性質（1ジョブごとに使い捨て・ロールに権限が無い・同時に1台まで）の3つを重ねる。
抜けられた場合に使われるのは、計算時間と、自分の AWS アカウントの東京の IP から外へ出る通信（第三者への不正なアクセスの報告は、自分のアカウントに届く）。

## 6. 作った後に確かめる

CloudShell（東京）で:

```bash
aws codebuild batch-get-projects --names seido-data-hub-crawl --query 'projects[0].{src:source,env:environment,timeout:timeoutInMinutes,limit:concurrentBuildLimit,webhook:webhook,logs:logsConfig,role:serviceRole,vpc:vpcConfig}'
aws iam list-role-policies --role-name seido-data-hub-crawl-codebuild
aws iam list-attached-role-policies --role-name seido-data-hub-crawl-codebuild
```

- `timeout` が `180`、`limit` が `1`、`vpc` が無い
- `env` が `LINUX_CONTAINER`・`aws/codebuild/standard:8.0`・`BUILD_GENERAL1_SMALL`
- `aws logs describe-log-groups --log-group-name-prefix /aws/codebuild/seido-data-hub-crawl` の `retentionInDays` が `30`
- `webhook.filterGroups` が1組で、中身が 4 の3つ
- ロールに、2 の許可ポリシー以外（`AdministratorAccess` や `*` の許可など）が付いていない

初めて流したとき（`gh workflow run crawl.yml`）:

- Actions のジョブのログの「Set up job」に、ランナーの名前として `codebuild-seido-data-hub-crawl` が出る
- CodeBuild のビルド履歴に、同じ時刻のビルドが1つ増え、ジョブが終わると「成功」で終わる

## 7. 定期実行でランナーが来ないとき

`ACTOR_ACCOUNT_ID` は、webhook の送り手（`sender`）の ID で照らす。手での起動では自分になる。
`schedule` で起きたジョブの送り手は、cron を最後に変えた人になる見込みだが、確かめていない。
合わないと、定期実行のジョブが待ったままになる（24時間後に失敗して通知が届く）。

そのときは:

1. 待ったままの実行を取り消す（`gh run cancel <id>`）。条件を直しても、送り済みの webhook は送り直されないので、そのジョブにはランナーが来ない。
   残しておくと `concurrency` で後の実行が待たされる
2. 4 の条件の組から `ACTOR_ACCOUNT_ID` を外し、`EVENT` と `WORKFLOW_NAME` だけにする
3. `gh workflow run crawl.yml` で流し直す

この形では、フォークの PR でもワークフロー名を `Crawl` にすれば条件に合うので、守りはほぼ 5 の承認だけになる
（承認しない限りフォークの PR のワークフローは動かない）。承認するときは、PR がワークフローを変えていないかを見る。
