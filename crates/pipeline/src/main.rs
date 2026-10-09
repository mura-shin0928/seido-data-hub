use std::collections::BTreeSet;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use clap::{Parser, Subcommand};
use domain::canonical::Decision;
use domain::extract::{self, Extracted};
use domain::fetch::CharsetSource;
use domain::liveness::Verdict;
use domain::run_report::{Thresholds, render_markdown};
use pipeline::crawl;
use pipeline::fetch::{Body, Config, Fetcher, Outcome, Validators};
use pipeline::{host_moves, import_registry, lifecycle, link_related, resources, run_report};
use sea_orm::prelude::Uuid;

#[derive(Parser)]
#[command(about = "seido-data-hub のデータ取り込み・更新")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// 東京都の子育て支援制度レジストリ（JSON）を取り込む。psid で上書きするので何度流してもよい
    ImportRegistry {
        /// JSON の URL、またはローカルのファイルパス
        #[arg(long, default_value = domain::registry::DEFAULT_SOURCE_URL)]
        source: String,
    },
    /// URL を取得して結果を表示する（DB は使わない）。robots と同一ホスト2秒の間隔を守る。
    /// HTML なら本文を取り出し、規則とハッシュも表示する（同じ URL を2回渡すと body_hash を比べられる）。
    /// 代表 URL（canonical_url）とその根拠も表示する
    Fetch {
        /// 取得する URL。許可リストはここに渡したホストだけになる
        #[arg(required = true)]
        urls: Vec<String>,
        /// 前回の ETag（条件付き取得を試す）
        #[arg(long)]
        etag: Option<String>,
        /// 前回の Last-Modified（条件付き取得を試す）
        #[arg(long)]
        last_modified: Option<String>,
        /// 取り出した本文も表示する（日をまたいだ揺れを見比べるため）
        #[arg(long)]
        body: bool,
    },
    /// 時刻の来た URL を取得して記録する（DATABASE_URL を使う）。同時16件・同一ホスト1件・2秒間隔で robots.txt を守る。
    /// 途中で止めたら、10分（lease）後に流し直せば続きから進む。
    /// 終わりに実行サマリーを出す。§22 の警告に当たったら、記録したうえで終了コード1で終える
    Crawl {
        /// crawl_runs に残す実行の種類（例: sweep）
        #[arg(long, default_value = "manual")]
        kind: String,
        /// 時刻の来るこの時間だけ前の URL も取る（定期実行の起動のずれを吸収する）。最短の間隔（3日）より短くする
        #[arg(long, default_value_t = 0)]
        due_within_hours: u64,
        /// 応答がこの件数に満たなければ警告にして終了コード1で終える（何も取らない定期実行に気づくため）。0 は見ない
        #[arg(long, default_value_t = 0)]
        min_responses: u64,
    },
    /// 主たる URL が死んだ制度に、レジストリの行に書かれた同じサイトのページを関連 URL として結ぶ（DATABASE_URL を使う）。
    /// 1制度3件まで。巡回の前に流すと、続く巡回が生死を確かめる。何度流してもよい
    LinkRelated,
    /// 実行のサマリーを出す（DATABASE_URL を使う）。既定は最新の実行。
    /// 途中で止まった実行は履歴から数え直す（再試行・lease 切れの数は出ない）
    RunReport {
        /// 実行の id（省略すると最新の実行）
        #[arg(long)]
        run: Option<Uuid>,
    },
    /// 転送で見つかったホスト移行を、許可リストに入れてよいものとして承認する（DATABASE_URL を使う）。
    /// 承認するのは、移行先が本物で同じ組織のサイトだと確かめてから
    ApproveHostMove {
        /// 元のホスト（例: https://www.old.example.jp）
        #[arg(long)]
        from: String,
        /// 移行先のホスト
        #[arg(long)]
        to: String,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Command::ImportRegistry { source } => {
            let database_url = std::env::var("DATABASE_URL").context("DATABASE_URL が無い")?;
            let db = sea_orm::Database::connect(&database_url).await?;
            let json = import_registry::load_source(&source).await?;
            let imported = domain::registry::parse(&json)?;
            // 元データの誤りなので取り込みは止めない。直すのは元データ側
            for tag in &imported.unknown_tags {
                eprintln!(
                    "警告: タグの一覧に無いコード psid={} column={} value={:?}",
                    tag.psid, tag.column, tag.value
                );
            }
            let outcome = import_registry::import(&db, imported).await?;
            // 元データの誤りなので取り込みは止めない。直すのは元データ側
            for rejected in &outcome.rejected {
                eprintln!(
                    "警告: URL を登録できない psid={} url={:?} 理由={}",
                    rejected.psid, rejected.raw_url, rejected.reason
                );
            }
            let counts = outcome.counts;
            println!(
                "取り込み完了: 自治体 {} 件 / 制度 {} 件 / URL {} 件（結び付き {} 件・登録できない URL {} 件）",
                counts.areas,
                counts.programs,
                counts.urls,
                counts.program_urls,
                outcome.rejected.len()
            );
        }
        Command::Fetch {
            urls,
            etag,
            last_modified,
            body,
        } => {
            let allowed: BTreeSet<String> = urls
                .iter()
                .filter_map(|url| domain::urls::prepare(url).ok())
                .map(|url| url.host_key)
                .collect();
            let fetcher = Fetcher::new(Config::default(), allowed)?;
            let validators = Validators {
                etag,
                last_modified,
            };
            for url in &urls {
                print_fetch(url, &fetcher.fetch(url, &validators).await, body);
            }
        }
        Command::Crawl {
            kind,
            due_within_hours,
            min_responses,
        } => {
            let database_url = std::env::var("DATABASE_URL").context("DATABASE_URL が無い")?;
            let db = sea_orm::Database::connect(&database_url).await?;
            let allowed = crawl::allowed_hosts(&db).await?;
            let fetcher = Arc::new(Fetcher::new(Config::default(), allowed)?);
            let config = crawl::Config {
                due_within: Duration::from_secs(due_within_hours * 3600),
                thresholds: Thresholds {
                    min_responses,
                    ..Thresholds::default()
                },
                ..crawl::Config::default()
            };
            let (_, _, report) = crawl::run_recorded(&db, fetcher, &kind, &config).await?;
            let markdown = render_markdown(&report.meta, &report.stats, &report.alerts);
            println!("{markdown}");
            if let Some(path) = std::env::var_os("GITHUB_STEP_SUMMARY")
                && let Err(e) = run_report::append_step_summary(Path::new(&path), &markdown)
            {
                eprintln!("警告: 実行サマリーを書けない: {e}");
            }
            if !report.alerts.is_empty() {
                anyhow::bail!("警告が {} 件ある（crawl_runs.alerts）", report.alerts.len());
            }
        }
        Command::LinkRelated => {
            let database_url = std::env::var("DATABASE_URL").context("DATABASE_URL が無い")?;
            let db = sea_orm::Database::connect(&database_url).await?;
            let counts = link_related::link(&db).await?;
            println!(
                "関連 URL: 死んだ制度 {} 件 / 候補のある制度 {} 件 / 結び付き {} 件 / URL 新規 {} 件・既存 {} 件",
                counts.dead_programs,
                counts.programs_with_related,
                counts.links,
                counts.new_urls,
                counts.existing_urls
            );
            if !counts.dropped.is_empty() {
                let dropped: Vec<String> = counts
                    .dropped
                    .iter()
                    .map(|(reason, n)| format!("{} {n} 件", reason.label()))
                    .collect();
                println!("候補にしなかった URL: {}", dropped.join(" / "));
            }
        }
        Command::RunReport { run } => {
            let database_url = std::env::var("DATABASE_URL").context("DATABASE_URL が無い")?;
            let db = sea_orm::Database::connect(&database_url).await?;
            let report = run_report::load(&db, run).await?;
            println!(
                "{}",
                render_markdown(&report.meta, &report.stats, &report.alerts)
            );
        }
        Command::ApproveHostMove { from, to } => {
            let database_url = std::env::var("DATABASE_URL").context("DATABASE_URL が無い")?;
            let db = sea_orm::Database::connect(&database_url).await?;
            if !host_moves::approve(&db, &from, &to).await? {
                anyhow::bail!("その移行は観測されていない: {from} → {to}");
            }
            println!("承認した: {from} → {to}");
        }
    }
    Ok(())
}

fn print_extracted(extracted: &Extracted) {
    let hashes = &extracted.hashes;
    println!(
        "    規則 {}（版 {}）/ 本文 {} 文字 / リンク {} 件",
        extracted.rule.as_str(),
        extract::EXTRACTOR_VERSION,
        extracted.body_text.chars().count(),
        extracted.links.len()
    );
    println!("    タイトル {:?}", extracted.title);
    println!(
        "    更新日 {:?} / canonical {:?} / robots {:?}",
        extracted.page_updated_on.map(|d| d.to_string()),
        extracted.declared_canonical_url,
        extracted.robots_meta
    );
    println!("    body_hash  {}", hashes.body);
    println!("    page_hash  {}", hashes.page);
    println!("    title_hash {}", hashes.title.as_deref().unwrap_or("-"));
    println!("    links_hash {}", hashes.links);
}

fn print_decision(decision: &Decision) {
    println!(
        "    代表 URL {}（根拠 {}・結び方 {}）",
        decision.canonical_url,
        decision.source.as_str(),
        decision.relation.as_str()
    );
    if let Some(reason) = &decision.ignored_redirect {
        println!("    採らなかった転送: {reason}");
    }
    if let Some(reason) = &decision.ignored_declared {
        println!(
            "    採らなかった canonical {:?}: {reason}",
            decision.declared_canonical_url
        );
    }
}

fn print_fetch(url: &str, fetch: &pipeline::fetch::Fetch, show_body: bool) {
    println!("{url}");
    for hop in &fetch.hops {
        println!(
            "  {} {} ({} ms)",
            hop.status,
            hop.url,
            hop.elapsed.as_millis()
        );
    }
    let extracted = crawl::extract_of(fetch);
    match &fetch.outcome {
        Outcome::Response(response) => {
            let body = match &response.body {
                Body::NotRead => "本文を読んでいない".to_string(),
                Body::Html(html) => {
                    let source = match html.source {
                        CharsetSource::Default => "判定元なし",
                        other => other.as_str(),
                    };
                    let replaced = if html.had_errors {
                        "・置き換えあり"
                    } else {
                        ""
                    };
                    format!("HTML {}（{source}{replaced}）", html.encoding)
                }
                Body::Pdf(_) => "PDF".to_string(),
                Body::Other(content_type) => format!("対象外 {content_type:?}"),
                Body::TooLarge => "上限を超えたので打ち切った".to_string(),
            };
            println!(
                "  → {} / {body} / {} バイト",
                response.status, response.bytes
            );
            println!(
                "    ETag {:?} / Last-Modified {:?} / Content-Type {:?}",
                response.etag, response.last_modified, response.content_type
            );
            if let Some(raw_hash) = &response.raw_hash {
                println!("    raw_hash   {raw_hash}");
            }
            if let Some(extracted) = &extracted {
                print_extracted(extracted);
                if show_body {
                    println!("{}", extracted.body_text);
                }
            }
            if let Some(tag) = &response.x_robots_tag {
                println!("    X-Robots-Tag {tag:?}");
            }
            // DB を使わないので、ホスト単位の判定は当てずにページ単位の検証だけで決める
            let declared = extracted
                .as_ref()
                .and_then(|e| e.declared_canonical_url.as_deref());
            match resources::decide(fetch, declared, true) {
                Some(decision) => print_decision(&decision),
                None => println!("    代表 URL は決めない（資源に触らない応答）"),
            }
        }
        Outcome::RobotsDenied { url } => println!("  → robots.txt で不許可: {url}"),
        Outcome::RobotsUnavailable { host_key, reason } => {
            println!("  → {host_key} を今回見送る: {reason}")
        }
        Outcome::OutOfScope { location, reason } => {
            println!("  → scope 外への転送で止めた: {location}（{reason}）")
        }
        Outcome::MissingLocation => println!("  → 転送先（Location）が無い"),
        Outcome::TooManyRedirects { location } => println!("  → 転送が多すぎる: 次は {location}"),
        Outcome::RedirectLoop { location } => println!("  → 転送がループした: {location}"),
        Outcome::Network { url, error, detail } => {
            println!("  → 通信エラー（{}）{url}: {detail}", error.as_str())
        }
    }
    print_verdict(&lifecycle::verdict(fetch, extracted.as_ref(), &[]));
}

fn print_verdict(verdict: &Verdict) {
    println!(
        "    ジョブ {:?}{} / 資源への観測 {:?}",
        verdict.job,
        verdict
            .error_type
            .map(|error_type| format!("（{error_type}）"))
            .unwrap_or_default(),
        verdict.observation
    );
    if let Some(host_move) = &verdict.host_move {
        println!(
            "    ホスト移行の候補: {} → {}",
            host_move.from_host_key, host_move.to_host_key
        );
    }
}
