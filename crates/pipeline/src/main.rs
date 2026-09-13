use anyhow::Context;
use clap::{Parser, Subcommand};
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
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let database_url = std::env::var("DATABASE_URL").context("DATABASE_URL が無い")?;
    let db = sea_orm::Database::connect(&database_url).await?;

    match cli.command {
        Command::ImportRegistry { source } => {
            let json = import_registry::load_source(&source).await?;
            let imported = domain::registry::parse(&json)?;
            // 元データの誤りなので取り込みは止めない。直すのは元データ側
            for tag in &imported.unknown_tags {
                eprintln!(
                    "警告: タグの一覧に無いコード psid={} column={} value={:?}",
                    tag.psid, tag.column, tag.value
                );
            }
            let counts = import_registry::import(&db, imported).await?;
            println!(
                "取り込み完了: 自治体 {} 件 / 制度 {} 件",
                counts.areas, counts.programs
            );
        }
    }
    Ok(())
}
