//! 1件の URL を、robots とホスト別の間隔を守って取得する（§11 §16 §19 §20）。
//!
//! DB には書かない。結果の保存と、どの URL をいつ取るかは呼び出し側が持つ。
//! `Fetcher` は1回の実行ごとに作る（robots のキャッシュを実行の間だけ持つ。実行は30分以内なので、
//! 設計の「1日キャッシュ」を超えない）。

use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, SystemTime};

use domain::extract;
use domain::fetch::{self, DecodedHtml, Kind, Pace, RobotsPolicy};
use domain::urls::{self, PreparedUrl, Rejection};
use reqwest::header::{self, HeaderMap};
use texting_robots::Robot;
use tokio::sync::{Mutex, OnceCell};
use tokio::time::Instant;
use url::Url;

/// reqwest の定数に無いヘッダ
const X_ROBOTS_TAG: header::HeaderName = header::HeaderName::from_static("x-robots-tag");

#[derive(Debug, Clone)]
pub struct Config {
    /// 同じホストへの最短の間隔。本番は2秒（テストでだけ縮める）
    pub min_interval: Duration,
    /// 接続までの時間の上限（仮置き）
    pub connect_timeout: Duration,
    /// 1リクエスト全体（本文の受信まで）の上限（仮置き）
    pub timeout: Duration,
    pub max_redirects: usize,
    pub max_body_bytes: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            min_interval: fetch::MIN_INTERVAL,
            connect_timeout: Duration::from_secs(10),
            timeout: Duration::from_secs(30),
            max_redirects: fetch::MAX_REDIRECTS,
            max_body_bytes: fetch::MAX_BODY_BYTES,
        }
    }
}

/// 前回の応答の validator。`Last-Modified` は送るだけで、値で変更を判定しない（§16）
#[derive(Debug, Clone, Default)]
pub struct Validators {
    pub etag: Option<String>,
    pub last_modified: Option<String>,
}

/// 1件の取得の結果。送ったリクエストはすべて `hops` に残る（転送の各段を含む）
#[derive(Debug)]
pub struct Fetch {
    pub hops: Vec<Hop>,
    pub outcome: Outcome,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hop {
    pub url: String,
    pub status: u16,
    pub elapsed: Duration,
}

#[derive(Debug)]
pub enum Outcome {
    /// 転送を追い終えた最後の応答（304・4xx・5xx を含む。status の意味づけは呼び出し側）
    Response(Response),
    /// robots.txt で不許可。リクエストは出していない
    RobotsDenied {
        url: String,
    },
    /// robots.txt が 5xx・通信エラー・解釈できない。そのホストは今回見送る
    RobotsUnavailable {
        host_key: String,
        reason: String,
    },
    /// 許可リストの外などへの転送。追わずに止め、転送先を残す（採るかどうかは後で判断する）
    OutOfScope {
        location: String,
        reason: Rejection,
    },
    MissingLocation,
    TooManyRedirects {
        location: String,
    },
    RedirectLoop {
        location: String,
    },
    Network {
        url: String,
        error: NetworkError,
        detail: String,
    },
}

#[derive(Debug)]
pub struct Response {
    /// 最終 URL（転送があれば転送先）
    pub url: String,
    pub status: u16,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub content_type: Option<String>,
    /// `X-Robots-Tag`（記録だけ。取得の可否には使わない。§19）
    pub x_robots_tag: Option<String>,
    pub body: Body,
    /// 受信した本文のバイト数（読まなかったときは0）
    pub bytes: u64,
    /// 受信したバイト列（復号前）のハッシュ。HTML・PDF を読みきったときだけ（§16 の `raw_hash`）
    pub raw_hash: Option<String>,
}

#[derive(Debug)]
pub enum Body {
    /// 304・4xx・5xx。本文を読んでいない
    NotRead,
    Html(DecodedHtml),
    Pdf(Vec<u8>),
    /// HTML・PDF 以外。本文を読まずに捨てた（値は Content-Type）
    Other(Option<String>),
    /// 上限を超えたので打ち切った
    TooLarge,
}

/// §12 の分類に合わせた通信エラーの種類
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetworkError {
    Timeout,
    Dns,
    Connect,
    Other,
}

impl NetworkError {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Timeout => "timeout",
            Self::Dns => "dns",
            Self::Connect => "connect",
            Self::Other => "other",
        }
    }
}

pub struct Fetcher {
    client: reqwest::Client,
    config: Config,
    allowed_hosts: BTreeSet<String>,
    hosts: StdMutex<HashMap<String, Arc<Host>>>,
}

/// ホストごとの状態。`gate` を持っている間だけそのホストへリクエストを出せる（1並列）
struct Host {
    gate: Mutex<Gate>,
    robots: OnceCell<Robots>,
}

struct Gate {
    next_at: Option<Instant>,
    pace: Pace,
}

enum Robots {
    Rules(Box<Robot>),
    AllowAll,
    Unavailable(String),
}

/// 1リクエストの結果
struct Exchange {
    status: u16,
    headers: HeaderMap,
    retry_after: Option<Duration>,
    body: Body,
    raw: Option<Vec<u8>>,
    raw_hash: Option<String>,
    bytes: u64,
    elapsed: Duration,
}

/// 本文をどう読むか
enum Read {
    /// 取得する対象のページ。HTML・PDF だけを上限まで読む
    Page,
    /// robots.txt。上限までで打ち切って、そこまでを解釈する
    Robots,
}

impl Fetcher {
    /// 許可リストは `scheme + ホスト名`（`urls.host_key`）の集合。外への転送は追わない（§6）
    pub fn new(config: Config, allowed_hosts: BTreeSet<String>) -> anyhow::Result<Self> {
        let client = Self::client_builder(&config).build()?;
        Ok(Self::with_client(client, config, allowed_hosts))
    }

    /// 転送は自分で追うので切る（各段に robots と間隔を掛けるため）。
    /// テストはここに DNS の上書きを足してから `with_client` に渡す
    pub fn client_builder(config: &Config) -> reqwest::ClientBuilder {
        reqwest::Client::builder()
            .user_agent(fetch::USER_AGENT)
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(config.connect_timeout)
            .timeout(config.timeout)
    }

    pub fn with_client(
        client: reqwest::Client,
        config: Config,
        allowed_hosts: BTreeSet<String>,
    ) -> Self {
        Self {
            client,
            config,
            allowed_hosts,
            hosts: StdMutex::new(HashMap::new()),
        }
    }

    /// 1件を取得する。同じホストへの呼び出しが同時に来ても、直列・間隔を空けて送る
    pub async fn fetch(&self, url: &str, validators: &Validators) -> Fetch {
        let mut hops = Vec::new();
        let mut visited = HashSet::new();
        let mut current = match self.scope(url) {
            Ok(prepared) => prepared,
            Err(reason) => {
                let location = url.to_string();
                return Fetch {
                    hops,
                    outcome: Outcome::OutOfScope { location, reason },
                };
            }
        };
        let outcome = loop {
            visited.insert(current.normalized_url.clone());
            let host = self.host(&current.host_key);
            match self.robots(&current.host_key, &host).await {
                Robots::Unavailable(reason) => {
                    break Outcome::RobotsUnavailable {
                        host_key: current.host_key.clone(),
                        reason: reason.clone(),
                    };
                }
                Robots::Rules(robot) if !robot.allowed(&current.normalized_url) => {
                    break Outcome::RobotsDenied {
                        url: current.normalized_url.clone(),
                    };
                }
                _ => {}
            }

            let target = current.normalized_url.as_str();
            let exchange = match self
                .gated(&host, async || {
                    self.exchange(target, validators, Read::Page).await
                })
                .await
            {
                Ok(exchange) => exchange,
                Err(error) => break network(target, &error),
            };
            hops.push(Hop {
                url: current.normalized_url.clone(),
                status: exchange.status,
                elapsed: exchange.elapsed,
            });

            if fetch::redirect_kind(exchange.status).is_none() {
                break Outcome::Response(Response {
                    url: current.normalized_url.clone(),
                    status: exchange.status,
                    etag: text(&exchange.headers, header::ETAG),
                    last_modified: text(&exchange.headers, header::LAST_MODIFIED),
                    content_type: text(&exchange.headers, header::CONTENT_TYPE),
                    x_robots_tag: text(&exchange.headers, X_ROBOTS_TAG)
                        .map(|value| value.trim().to_ascii_lowercase()),
                    body: exchange.body,
                    bytes: exchange.bytes,
                    raw_hash: exchange.raw_hash,
                });
            }
            let Some(location) = resolve_location(target, &exchange.headers) else {
                break Outcome::MissingLocation;
            };
            if hops.len() > self.config.max_redirects {
                break Outcome::TooManyRedirects { location };
            }
            let next = match self.scope(&location) {
                Ok(next) => next,
                Err(reason) => break Outcome::OutOfScope { location, reason },
            };
            if visited.contains(&next.normalized_url) {
                break Outcome::RedirectLoop { location };
            }
            current = next;
        };
        Fetch { hops, outcome }
    }

    fn scope(&self, url: &str) -> Result<PreparedUrl, Rejection> {
        let prepared = urls::prepare(url)?;
        urls::in_scope(&prepared, &self.allowed_hosts)?;
        Ok(prepared)
    }

    fn host(&self, host_key: &str) -> Arc<Host> {
        let mut hosts = self.hosts.lock().expect("ホストの表が壊れていない");
        hosts
            .entry(host_key.to_string())
            .or_insert_with(|| {
                Arc::new(Host {
                    gate: Mutex::new(Gate {
                        next_at: None,
                        pace: Pace::new(self.config.min_interval),
                    }),
                    robots: OnceCell::new(),
                })
            })
            .clone()
    }

    /// そのホストのゲートを取り、間隔が空くまで待ってから送る。受け取り終えた時刻から次の間隔を数える
    async fn gated(
        &self,
        host: &Host,
        send: impl AsyncFnOnce() -> Result<Exchange, reqwest::Error>,
    ) -> Result<Exchange, reqwest::Error> {
        let mut gate = host.gate.lock().await;
        if let Some(at) = gate.next_at {
            tokio::time::sleep_until(at).await;
        }
        let result = send().await;
        let wait = match &result {
            Ok(exchange) => gate
                .pace
                .after_response(exchange.status, exchange.retry_after),
            // 通信エラーでは間隔を変えない（retry とバックオフは呼び出し側）
            Err(_) => gate.pace.interval(),
        };
        gate.next_at = Some(Instant::now() + wait);
        result
    }

    /// robots.txt をホストごとに1回だけ取る（同時に呼ばれても1回）
    async fn robots<'a>(&self, host_key: &str, host: &'a Host) -> &'a Robots {
        host.robots
            .get_or_init(async || {
                let robots = self.load_robots(host_key).await;
                if let Robots::Rules(robot) = &robots
                    && let Some(delay) = robot.delay.filter(|d| d.is_finite() && *d > 0.0)
                {
                    host.gate.lock().await.pace.crawl_delay = Some(Duration::from_secs_f32(delay));
                }
                robots
            })
            .await
    }

    async fn load_robots(&self, host_key: &str) -> Robots {
        let mut url = format!("{host_key}/robots.txt");
        for _ in 0..=self.config.max_redirects {
            // robots.txt の取得もそのホストへの1リクエストとして、同じゲートを通る
            let Ok(prepared) = self.scope(&url) else {
                // 許可リストの外への転送は追わない。取れなかったのと同じ「制限なし」にし、
                // ページ側の転送で scope 外として記録されるのに任せる
                return Robots::AllowAll;
            };
            let host = self.host(&prepared.host_key);
            let target = prepared.normalized_url.as_str();
            let exchange = match self
                .gated(&host, async || {
                    self.exchange(target, &Validators::default(), Read::Robots)
                        .await
                })
                .await
            {
                Ok(exchange) => exchange,
                Err(error) => {
                    return Robots::Unavailable(format!(
                        "robots.txt を取れない（{}）",
                        network_kind(&error).as_str()
                    ));
                }
            };
            if fetch::redirect_kind(exchange.status).is_some()
                && let Some(location) = resolve_location(target, &exchange.headers)
            {
                url = location;
                continue;
            }
            return match fetch::robots_policy(exchange.status) {
                RobotsPolicy::AllowAll => Robots::AllowAll,
                RobotsPolicy::Skip => {
                    Robots::Unavailable(format!("robots.txt が {}", exchange.status))
                }
                RobotsPolicy::Parse => {
                    match Robot::new(fetch::ROBOTS_AGENT, &exchange.raw.unwrap_or_default()) {
                        Ok(robot) => Robots::Rules(Box::new(robot)),
                        Err(error) => {
                            Robots::Unavailable(format!("robots.txt を解釈できない: {error}"))
                        }
                    }
                }
            };
        }
        // 転送を追いきれなかった（RFC 9309 §2.3.1.2: 「無い」と同じ扱い）
        Robots::AllowAll
    }

    async fn exchange(
        &self,
        url: &str,
        validators: &Validators,
        read: Read,
    ) -> Result<Exchange, reqwest::Error> {
        let started = Instant::now();
        let mut request = self.client.get(url);
        if let Some(etag) = &validators.etag {
            request = request.header(header::IF_NONE_MATCH, etag);
        }
        if let Some(last_modified) = &validators.last_modified {
            request = request.header(header::IF_MODIFIED_SINCE, last_modified);
        }
        let mut response = request.send().await?;
        let status = response.status().as_u16();
        let headers = response.headers().clone();
        let retry_after = text(&headers, header::RETRY_AFTER)
            .and_then(|value| fetch::parse_retry_after(&value, SystemTime::now()));
        let content_type = text(&headers, header::CONTENT_TYPE);

        let success = (200..300).contains(&status);
        let (body, raw, raw_hash, bytes) = match read {
            _ if !success => (Body::NotRead, None, None, 0),
            Read::Robots => {
                let (mut raw, _) = read_limited(&mut response, fetch::MAX_ROBOTS_BYTES).await?;
                raw.truncate(fetch::MAX_ROBOTS_BYTES as usize);
                let bytes = raw.len() as u64;
                (Body::NotRead, Some(raw), None, bytes)
            }
            Read::Page => {
                let (body, raw_hash, bytes) = self
                    .read_page(&mut response, content_type.as_deref())
                    .await?;
                (body, None, raw_hash, bytes)
            }
        };
        Ok(Exchange {
            status,
            headers,
            retry_after,
            body,
            raw,
            raw_hash,
            bytes,
            elapsed: started.elapsed(),
        })
    }

    async fn read_page(
        &self,
        response: &mut reqwest::Response,
        content_type: Option<&str>,
    ) -> Result<(Body, Option<String>, u64), reqwest::Error> {
        // ヘッダだけで対象外と分かれば読まない
        if let Some(Kind::Other(value)) = fetch::kind_from_header(content_type) {
            return Ok((Body::Other(value), None, 0));
        }
        let limit = self.config.max_body_bytes;
        if response
            .content_length()
            .is_some_and(|length| length > limit)
        {
            return Ok((Body::TooLarge, None, 0));
        }
        let (raw, exceeded) = read_limited(response, limit).await?;
        let bytes = raw.len() as u64;
        if exceeded {
            return Ok((Body::TooLarge, None, bytes));
        }
        let raw_hash = extract::digest(&raw);
        let (body, raw_hash) = match fetch::classify(content_type, &raw) {
            Kind::Html => (
                Body::Html(fetch::decode_html(content_type, &raw)),
                Some(raw_hash),
            ),
            Kind::Pdf => (Body::Pdf(raw), Some(raw_hash)),
            Kind::Other(value) => (Body::Other(value), None),
        };
        Ok((body, raw_hash, bytes))
    }
}

/// 上限を超えたところで読むのをやめる。戻り値の2つ目は「超えたか」
async fn read_limited(
    response: &mut reqwest::Response,
    limit: u64,
) -> Result<(Vec<u8>, bool), reqwest::Error> {
    let mut raw = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        raw.extend_from_slice(&chunk);
        if raw.len() as u64 > limit {
            return Ok((raw, true));
        }
    }
    Ok((raw, false))
}

fn text(headers: &HeaderMap, name: header::HeaderName) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string)
}

/// 相対の `Location` を、いまの URL を基準に絶対 URL にする
fn resolve_location(base: &str, headers: &HeaderMap) -> Option<String> {
    let location = text(headers, header::LOCATION)?;
    let joined = Url::parse(base).ok()?.join(location.trim()).ok()?;
    Some(joined.into())
}

fn network(url: &str, error: &reqwest::Error) -> Outcome {
    Outcome::Network {
        url: url.to_string(),
        error: network_kind(error),
        detail: chain(error),
    }
}

fn network_kind(error: &reqwest::Error) -> NetworkError {
    if error.is_timeout() {
        NetworkError::Timeout
    } else if error.is_connect() {
        // reqwest は DNS の失敗を接続エラーとして返すので、原因の文から分ける
        let detail = chain(error);
        if detail.contains("dns error") || detail.contains("failed to lookup address") {
            NetworkError::Dns
        } else {
            NetworkError::Connect
        }
    } else {
        NetworkError::Other
    }
}

/// エラーの原因をたどって1行にする
fn chain(error: &(dyn std::error::Error + 'static)) -> String {
    let mut parts = vec![error.to_string()];
    let mut source = error.source();
    while let Some(cause) = source {
        parts.push(cause.to_string());
        source = cause.source();
    }
    parts.join(": ")
}
