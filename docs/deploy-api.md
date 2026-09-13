# API のデプロイ（初回の設定）

CI の `deploy-api` ジョブ（main への push で、`migrate` の後）が `cargo lambda build --release --arm64` →
`cargo lambda deploy` を流す。AWS の認証は GitHub Actions の OIDC で IAM ロールを借りるので、アクセスキーは置かない。
以下は最初に一度だけ手で行う。

## 1. Supabase: API 用の読み取り専用ロール

API は読むだけなので、migration・取り込みで使う `DATABASE_URL` とは別に、select しかできないロールで繋ぐ。
SQL Editor で:

```sql
create role api_reader with login password '<十分に長いランダムな文字列>';
grant usage on schema public to api_reader;
grant select on all tables in schema public to api_reader;
-- 今後 migration で足すテーブルにも select を付ける
alter default privileges for role postgres in schema public grant select on tables to api_reader;
```

接続文字列はプーラーの **Session モード（ポート5432）**。ユーザー名は `api_reader.<project-ref>` になる:

```
postgres://api_reader.<project-ref>:<password>@aws-0-ap-northeast-1.pooler.supabase.com:5432/postgres?sslmode=require
```

## 2. AWS IAM

### Lambda の実行ロール

- 信頼されたエンティティ: AWS のサービス → Lambda
- 許可ポリシー: `AWSLambdaBasicExecutionRole`（CloudWatch Logs に書くだけ）
- 名前の例: `seido-data-hub-api-execution`

### GitHub Actions 用の OIDC プロバイダー

IAM → ID プロバイダ → 追加: OpenID Connect、URL `https://token.actions.githubusercontent.com`、対象者 `sts.amazonaws.com`。

### デプロイ用ロール

信頼ポリシー（main ブランチの workflow だけが借りられる）:

```json
{
  "Version": "2012-10-17",
  "Statement": [
    {
      "Effect": "Allow",
      "Principal": { "Federated": "arn:aws:iam::<account-id>:oidc-provider/token.actions.githubusercontent.com" },
      "Action": "sts:AssumeRoleWithWebIdentity",
      "Condition": {
        "StringEquals": {
          "token.actions.githubusercontent.com:aud": "sts.amazonaws.com",
          "token.actions.githubusercontent.com:sub": "repo:mura-shin0928/seido-data-hub:ref:refs/heads/main"
        }
      }
    }
  ]
}
```

許可ポリシー（この関数と実行ロールだけに絞る）:

```json
{
  "Version": "2012-10-17",
  "Statement": [
    {
      "Effect": "Allow",
      "Action": [
        "lambda:GetFunction",
        "lambda:GetFunctionConfiguration",
        "lambda:CreateFunction",
        "lambda:UpdateFunctionCode",
        "lambda:UpdateFunctionConfiguration",
        "lambda:PublishVersion",
        "lambda:TagResource",
        "lambda:GetFunctionUrlConfig",
        "lambda:CreateFunctionUrlConfig",
        "lambda:AddPermission"
      ],
      "Resource": "arn:aws:lambda:ap-northeast-1:<account-id>:function:seido-data-hub-api"
    },
    {
      "Effect": "Allow",
      "Action": "iam:PassRole",
      "Resource": "arn:aws:iam::<account-id>:role/seido-data-hub-api-execution"
    },
    {
      "Effect": "Allow",
      "Action": ["logs:CreateLogGroup", "logs:PutRetentionPolicy"],
      "Resource": "arn:aws:logs:ap-northeast-1:<account-id>:log-group:/aws/lambda/seido-data-hub-api:*"
    }
  ]
}
```

cargo-lambda が呼ぶ API は版によって変わりうる。足りなければ Actions のログに `AccessDenied` と操作名が出るので、それを足す。

## 3. GitHub のリポジトリ設定

Settings → Secrets and variables → Actions:

| 種類 | 名前 | 値 |
|---|---|---|
| Variables | `AWS_DEPLOY_ROLE_ARN` | デプロイ用ロールの ARN |
| Variables | `LAMBDA_EXECUTION_ROLE_ARN` | Lambda の実行ロールの ARN |
| Secrets | `API_DATABASE_URL` | 1 の接続文字列 |

## 4. 初回デプロイの後

関数URL（認証なし）は、2025年10月以降に作ったものだと `lambda:InvokeFunctionUrl` に加えて
`lambda:InvokeFunction` の許可も要る。`--enable-function-url` が前者しか付けず 403 が返るときは、一度だけ足す:

```bash
aws lambda add-permission --function-name seido-data-hub-api --region ap-northeast-1 --statement-id FunctionURLInvoke --action lambda:InvokeFunction --principal '*' --invoked-via-function-url
```

確かめる（URL は Lambda コンソールか `aws lambda get-function-url-config` で見る）:

```bash
curl "https://<id>.lambda-url.ap-northeast-1.on.aws/v1/areas/132101/programs"
```

小金井市107件＋東京都27件の134件が返れば S1 の完了条件を満たす。
