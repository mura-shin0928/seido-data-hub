use std::collections::BTreeSet;

use anyhow::Context;
use clap::{Parser, Subcommand};
use domain::fetch::CharsetSource;
use pipeline::fetch::{Body, Config, Fetcher, Outcome, Validators};
use pipeline::import_registry;

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
    /// URL を取得して結果を表示する（DB は使わない）。robots と同一ホスト2秒の間隔を守る
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
                print_fetch(url, &fetcher.fetch(url, &validators).await);
            }
        }
    }
    Ok(())
}

fn print_fetch(url: &str, fetch: &pipeline::fetch::Fetch) {
    println!("{url}");
    for hop in &fetch.hops {
        println!(
            "  {} {} ({} ms)",
            hop.status,
            hop.url,
            hop.elapsed.as_millis()
        );
    }
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
}
