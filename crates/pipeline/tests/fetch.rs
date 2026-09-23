//! 取得を、対象サイトの代わりに立てたテスト用サーバーに流す。
//!
//! サーバーは `127.0.0.1` の任意ポートで1つだけ立て、DNS の上書きで複数のホスト名をそこへ向ける。
//! URL は標準ポートのままなので、scope 判定を緩めずに確かめられる。

use std::collections::BTreeSet;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::body::Body as HttpBody;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::Response as HttpResponse;
use domain::extract::{self, Rule, digest};
use domain::urls::Rejection;
use pipeline::fetch::{Body, Config, Fetch, Fetcher, Outcome, Response, Validators};
use tokio::time::Instant;

const CITY: &str = "www.city.example.jp";
const TOWN: &str = "www.town.example.jp";
/// 許可リストに入れないホスト
const OTHER: &str = "www.other.example.jp";

/// サーバーが受けた1リクエスト
#[derive(Debug, Clone)]
struct Seen {
    host: String,
    path: String,
    at: Instant,
    headers: HeaderMap,
}

type Respond = dyn Fn(&str, &str, &HeaderMap) -> HttpResponse + Send + Sync;

#[derive(Clone)]
struct Site {
    respond: Arc<Respond>,
    seen: Arc<Mutex<Vec<Seen>>>,
}

impl Site {
    fn seen(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }

    fn paths(&self, host: &str) -> Vec<String> {
        self.seen()
            .into_iter()
            .filter(|s| s.host == host)
            .map(|s| s.path)
            .collect()
    }
}

async fn handle(State(site): State<Site>, request: Request) -> HttpResponse {
    let host = request
        .headers()
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .unwrap_or_default()
        .split(':')
        .next()
        .unwrap_or_default()
        .to_string();
    let path = request
        .uri()
        .path_and_query()
        .map(|p| p.as_str().to_string())
        .unwrap_or_default();
    let headers = request.headers().clone();
    site.seen.lock().unwrap().push(Seen {
        host: host.clone(),
        path: path.clone(),
        at: Instant::now(),
        headers: headers.clone(),
    });
    (site.respond)(&host, &path, &headers)
}

/// テスト用サーバーを立て、そこへ向けた `Fetcher` を作る。許可リストは CITY と TOWN
async fn serve(
    config: Config,
    respond: impl Fn(&str, &str, &HeaderMap) -> HttpResponse + Send + Sync + 'static,
) -> (Fetcher, Site) {
    let site = Site {
        respond: Arc::new(respond),
        seen: Arc::default(),
    };
    let app = Router::new().fallback(handle).with_state(site.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let client = [CITY, TOWN, OTHER]
        .into_iter()
        .fold(Fetcher::client_builder(&config), |builder, host| {
            builder.resolve(host, addr)
        })
        .build()
        .unwrap();
    let allowed = BTreeSet::from([format!("http://{CITY}"), format!("http://{TOWN}")]);
    (Fetcher::with_client(client, config, allowed), site)
}

/// 間隔の確認以外は、待ち時間を縮めて速く流す
fn quick() -> Config {
    Config {
        min_interval: Duration::from_millis(20),
        ..Config::default()
    }
}

fn reply(status: u16) -> axum::http::response::Builder {
    HttpResponse::builder().status(StatusCode::from_u16(status).unwrap())
}

fn html(body: &str) -> HttpResponse {
    reply(200)
        .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
        .body(HttpBody::from(body.to_string()))
        .unwrap()
}

fn empty(status: u16) -> HttpResponse {
    reply(status).body(HttpBody::empty()).unwrap()
}

fn redirect(status: u16, location: &str) -> HttpResponse {
    reply(status)
        .header(header::LOCATION, location)
        .body(HttpBody::empty())
        .unwrap()
}

fn robots(body: &'static str) -> HttpResponse {
    reply(200)
        .header(header::CONTENT_TYPE, "text/plain")
        .body(HttpBody::from(body))
        .unwrap()
}

fn response(fetch: &Fetch) -> &Response {
    match &fetch.outcome {
        Outcome::Response(response) => response,
        other => panic!("応答が無い: {other:?}"),
    }
}

async fn get(fetcher: &Fetcher, url: &str) -> Fetch {
    fetcher.fetch(url, &Validators::default()).await
}

#[tokio::test]
async fn same_host_requests_are_two_seconds_apart_even_when_called_at_once() {
    // 間隔は本番の設定値（2秒）のまま流す
    let (fetcher, site) = serve(Config::default(), |_, path, _| match path {
        "/robots.txt" => empty(404),
        _ => html("<p>ok</p>"),
    })
    .await;

    let urls = ["a", "b", "c"].map(|name| format!("http://{CITY}/{name}.html"));
    let town = format!("http://{TOWN}/a.html");
    let started = Instant::now();
    let (a, b, c, d) = tokio::join!(
        get(&fetcher, &urls[0]),
        get(&fetcher, &urls[1]),
        get(&fetcher, &urls[2]),
        get(&fetcher, &town),
    );
    for fetch in [&a, &b, &c, &d] {
        assert_eq!(response(fetch).status, 200);
    }

    // robots.txt を含めて4リクエスト。受けた時刻の間隔がすべて2秒以上
    let times: Vec<Instant> = site
        .seen()
        .iter()
        .filter(|s| s.host == CITY)
        .map(|s| s.at)
        .collect();
    assert_eq!(times.len(), 4);
    for pair in times.windows(2) {
        let gap = pair[1] - pair[0];
        assert!(gap >= Duration::from_secs(2), "間隔が {gap:?}");
    }

    // 別のホストは CITY の待ちに巻き込まれない
    let town_first = site.seen().iter().find(|s| s.host == TOWN).unwrap().at;
    assert!(town_first - started < Duration::from_secs(1));
}

#[tokio::test]
async fn disallowed_paths_are_never_requested() {
    let (fetcher, site) = serve(quick(), |_, path, _| match path {
        "/robots.txt" => robots("User-agent: *\nDisallow: /private/\n"),
        _ => html("<p>ok</p>"),
    })
    .await;

    let denied = get(&fetcher, &format!("http://{CITY}/private/a.html")).await;
    assert!(
        matches!(denied.outcome, Outcome::RobotsDenied { .. }),
        "{denied:?}"
    );
    assert!(denied.hops.is_empty());

    let allowed = get(&fetcher, &format!("http://{CITY}/public/a.html")).await;
    assert_eq!(response(&allowed).status, 200);

    // robots.txt は1回だけ取る
    assert_eq!(site.paths(CITY), ["/robots.txt", "/public/a.html"]);
}

#[tokio::test]
async fn rules_for_our_agent_are_applied() {
    let (fetcher, site) = serve(quick(), |_, path, _| match path {
        "/robots.txt" => {
            robots("User-agent: seido-data-hub\nDisallow: /\n\nUser-agent: *\nAllow: /\n")
        }
        _ => html("<p>ok</p>"),
    })
    .await;

    let denied = get(&fetcher, &format!("http://{CITY}/a.html")).await;
    assert!(
        matches!(denied.outcome, Outcome::RobotsDenied { .. }),
        "{denied:?}"
    );
    assert_eq!(site.paths(CITY), ["/robots.txt"]);
}

#[tokio::test]
async fn missing_robots_means_no_restriction() {
    // 小金井市のように、robots.txt が 404 で HTML を返すホスト
    let (fetcher, _) = serve(quick(), |_, path, _| match path {
        "/robots.txt" => reply(404)
            .header(header::CONTENT_TYPE, "text/html")
            .body(HttpBody::from("<h1>Not Found</h1>Disallow: /"))
            .unwrap(),
        _ => html("<p>ok</p>"),
    })
    .await;

    let fetched = get(&fetcher, &format!("http://{CITY}/a.html")).await;
    assert_eq!(response(&fetched).status, 200);
}

#[tokio::test]
async fn unavailable_robots_skips_the_host() {
    let (fetcher, site) = serve(quick(), |_, path, _| match path {
        "/robots.txt" => empty(503),
        _ => html("<p>ok</p>"),
    })
    .await;

    for path in ["/a.html", "/b.html"] {
        let skipped = get(&fetcher, &format!("http://{CITY}{path}")).await;
        assert!(
            matches!(skipped.outcome, Outcome::RobotsUnavailable { .. }),
            "{skipped:?}"
        );
    }
    // 今回の実行では robots.txt を取り直さない
    assert_eq!(site.paths(CITY), ["/robots.txt"]);
}

#[tokio::test]
async fn robots_429_means_no_restriction_after_waiting() {
    let (fetcher, site) = serve(quick(), |_, path, _| match path {
        "/robots.txt" => reply(429)
            .header(header::RETRY_AFTER, "1")
            .body(HttpBody::empty())
            .unwrap(),
        _ => html("<p>ok</p>"),
    })
    .await;

    let fetched = get(&fetcher, &format!("http://{CITY}/a.html")).await;
    assert_eq!(response(&fetched).status, 200);

    let seen = site.seen();
    assert_eq!(site.paths(CITY), ["/robots.txt", "/a.html"]);
    let gap = seen[1].at - seen[0].at;
    assert!(
        gap >= Duration::from_secs(1),
        "Retry-After を待っていない: {gap:?}"
    );
}

#[tokio::test]
async fn not_modified_does_not_read_the_body() {
    let (fetcher, site) = serve(quick(), |_, path, headers| match path {
        "/robots.txt" => empty(404),
        _ if headers
            .get(header::IF_NONE_MATCH)
            .is_some_and(|v| v == "\"v1\"") =>
        {
            empty(304)
        }
        _ => reply(200)
            .header(header::CONTENT_TYPE, "text/html")
            .header(header::ETAG, "\"v1\"")
            .header(header::LAST_MODIFIED, "Wed, 23 Sep 2026 00:00:00 GMT")
            .body(HttpBody::from("<p>本文</p>"))
            .unwrap(),
    })
    .await;
    let url = format!("http://{CITY}/a.html");

    let first = get(&fetcher, &url).await;
    let first = response(&first);
    assert!(matches!(&first.body, Body::Html(html) if html.text == "<p>本文</p>"));
    assert_eq!(first.etag.as_deref(), Some("\"v1\""));

    let validators = Validators {
        etag: first.etag.clone(),
        last_modified: first.last_modified.clone(),
    };
    let second = fetcher.fetch(&url, &validators).await;
    let second = response(&second);
    assert_eq!(second.status, 304);
    assert!(matches!(second.body, Body::NotRead));
    assert_eq!(second.bytes, 0);

    let seen = site.seen();
    let sent = &seen.last().unwrap().headers;
    assert_eq!(sent[header::IF_NONE_MATCH], "\"v1\"");
    assert_eq!(
        sent[header::IF_MODIFIED_SINCE],
        "Wed, 23 Sep 2026 00:00:00 GMT"
    );
}

#[tokio::test]
async fn redirects_are_followed_and_recorded() {
    let (fetcher, _) = serve(quick(), |host, path, _| match (host, path) {
        (_, "/robots.txt") => empty(404),
        (CITY, "/old.html") => redirect(301, "/new.html"),
        // 別の許可されたホストへの転送は追う
        (CITY, "/moved.html") => redirect(302, &format!("http://{TOWN}/moved.html")),
        _ => html("<p>ok</p>"),
    })
    .await;

    let fetched = get(&fetcher, &format!("http://{CITY}/old.html")).await;
    let statuses: Vec<(String, u16)> = fetched
        .hops
        .iter()
        .map(|hop| (hop.url.clone(), hop.status))
        .collect();
    assert_eq!(
        statuses,
        [
            (format!("http://{CITY}/old.html"), 301),
            (format!("http://{CITY}/new.html"), 200),
        ]
    );
    assert_eq!(response(&fetched).url, format!("http://{CITY}/new.html"));

    let moved = get(&fetcher, &format!("http://{CITY}/moved.html")).await;
    assert_eq!(response(&moved).url, format!("http://{TOWN}/moved.html"));
}

#[tokio::test]
async fn redirects_outside_the_allow_list_are_recorded_but_not_followed() {
    let (fetcher, site) = serve(quick(), |host, path, _| match (host, path) {
        (_, "/robots.txt") => empty(404),
        (CITY, _) => redirect(301, &format!("http://{OTHER}/kosodate.html")),
        _ => html("<p>ok</p>"),
    })
    .await;

    let fetched = get(&fetcher, &format!("http://{CITY}/kosodate.html")).await;
    match &fetched.outcome {
        Outcome::OutOfScope { location, reason } => {
            assert_eq!(location, &format!("http://{OTHER}/kosodate.html"));
            assert_eq!(
                reason,
                &Rejection::HostNotAllowed {
                    host_key: format!("http://{OTHER}")
                }
            );
        }
        other => panic!("scope 外で止まっていない: {other:?}"),
    }
    assert_eq!(fetched.hops.len(), 1);
    assert!(site.paths(OTHER).is_empty());
}

#[tokio::test]
async fn redirect_loops_and_long_chains_stop() {
    let (fetcher, _) = serve(quick(), |_, path, _| match path {
        "/robots.txt" => empty(404),
        "/loop-a.html" => redirect(302, "/loop-b.html"),
        "/loop-b.html" => redirect(302, "/loop-a.html"),
        _ => {
            // /chain/0 → /chain/1 → ... と終わらない転送
            let n: u32 = path.trim_start_matches("/chain/").parse().unwrap_or(0);
            redirect(301, &format!("/chain/{}", n + 1))
        }
    })
    .await;

    let looped = get(&fetcher, &format!("http://{CITY}/loop-a.html")).await;
    assert!(
        matches!(looped.outcome, Outcome::RedirectLoop { .. }),
        "{looped:?}"
    );
    assert_eq!(looped.hops.len(), 2);

    let chain = get(&fetcher, &format!("http://{CITY}/chain/0")).await;
    assert!(
        matches!(chain.outcome, Outcome::TooManyRedirects { .. }),
        "{chain:?}"
    );
    // 5段まで追い、6段目の転送は追わない
    assert_eq!(chain.hops.len(), 6);
}

#[tokio::test]
async fn bodies_over_ten_megabytes_are_cut() {
    const ELEVEN_MB: usize = 11 * 1024 * 1024;
    let (fetcher, _) = serve(quick(), |_, path, _| match path {
        "/robots.txt" => empty(404),
        // Content-Length で分かる
        "/large.html" => reply(200)
            .header(header::CONTENT_TYPE, "text/html")
            .body(HttpBody::from(vec![b'a'; ELEVEN_MB]))
            .unwrap(),
        // 長さを言わずに送り続ける
        _ => {
            let chunks = (0..11).map(|_| Ok::<_, std::io::Error>(vec![b'a'; 1024 * 1024]));
            reply(200)
                .header(header::CONTENT_TYPE, "text/html")
                .body(HttpBody::from_stream(futures_util::stream::iter(chunks)))
                .unwrap()
        }
    })
    .await;

    let large = get(&fetcher, &format!("http://{CITY}/large.html")).await;
    let large = response(&large);
    assert!(matches!(large.body, Body::TooLarge));
    assert_eq!(large.bytes, 0);

    let streamed = get(&fetcher, &format!("http://{CITY}/streamed.html")).await;
    let streamed = response(&streamed);
    assert!(matches!(streamed.body, Body::TooLarge));
    assert!(streamed.bytes <= 11 * 1024 * 1024);
}

#[tokio::test]
async fn content_type_and_charset_are_decided_from_the_response() {
    let (fetcher, _) = serve(quick(), |_, path, _| match path {
        "/robots.txt" => empty(404),
        "/photo.png" => reply(200)
            .header(header::CONTENT_TYPE, "image/png")
            .body(HttpBody::from(vec![0x89, b'P', b'N', b'G']))
            .unwrap(),
        "/file.pdf" => reply(200)
            .header(header::CONTENT_TYPE, "application/pdf")
            .body(HttpBody::from("%PDF-1.7\n"))
            .unwrap(),
        // 「子育て」を Shift_JIS で
        _ => reply(200)
            .header(header::CONTENT_TYPE, "text/html; charset=Shift_JIS")
            .body(HttpBody::from(b"<p>\x8e\x71\x88\xe7\x82\xc4</p>".to_vec()))
            .unwrap(),
    })
    .await;

    let image = get(&fetcher, &format!("http://{CITY}/photo.png")).await;
    let image = response(&image);
    assert!(matches!(&image.body, Body::Other(Some(t)) if t == "image/png"));
    assert_eq!(image.bytes, 0);
    assert_eq!(image.raw_hash, None);

    let pdf = get(&fetcher, &format!("http://{CITY}/file.pdf")).await;
    assert!(matches!(&response(&pdf).body, Body::Pdf(bytes) if bytes.starts_with(b"%PDF-")));
    assert_eq!(response(&pdf).raw_hash, Some(digest(b"%PDF-1.7\n")));

    let page = get(&fetcher, &format!("http://{CITY}/sjis.html")).await;
    // raw_hash は復号する前のバイト列から作る
    assert_eq!(
        response(&page).raw_hash,
        Some(digest(b"<p>\x8e\x71\x88\xe7\x82\xc4</p>"))
    );
    match &response(&page).body {
        Body::Html(html) => {
            assert_eq!(html.text, "<p>子育て</p>");
            assert_eq!(html.encoding, "Shift_JIS");
        }
        other => panic!("HTML として読めていない: {other:?}"),
    }
}

#[tokio::test]
async fn the_same_page_fetched_twice_has_the_same_body_hash() {
    // 自治体サイトによくある形: 本文は id="main_contents"。ヘッダの時刻・script のトークン・コメントは毎回変わる
    let count = Arc::new(AtomicU32::new(0));
    let (fetcher, _) = serve(quick(), move |_, path, _| match path {
        "/robots.txt" => empty(404),
        _ => {
            let n = count.fetch_add(1, Ordering::SeqCst);
            reply(200)
                .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
                .header("X-Robots-Tag", "NoArchive")
                .body(HttpBody::from(format!(
                    r#"<!DOCTYPE html><html><head><title>児童手当｜○○市</title>
                    <script>var token = "t{n}";</script></head><body>
                    <header><p>現在 10:0{n}</p><nav><a href="/">トップ</a></nav></header>
                    <div id="main_contents"><h1>児童手当</h1>
                    <p>中学生までの子を養育している方に支給します。</p>
                    <a href="shinsei.html">申請</a><!-- generated {n} --></div>
                    <footer><p>更新日：2024年3月1日</p></footer></body></html>"#
                )))
                .unwrap()
        }
    })
    .await;

    let url = format!("http://{CITY}/kosodate/teate.html");
    let first = get(&fetcher, &url).await;
    let second = get(&fetcher, &url).await;
    let (first, second) = (response(&first), response(&second));
    assert_eq!(first.x_robots_tag.as_deref(), Some("noarchive"));

    // 生の応答は毎回違う
    assert_ne!(first.raw_hash, second.raw_hash);

    let [a, b] = [first, second].map(|response| match &response.body {
        Body::Html(html) => extract::extract(&html.text, &response.url),
        other => panic!("HTML として読めていない: {other:?}"),
    });
    assert_eq!(a.hashes.body, b.hashes.body);
    assert_eq!(a.rule, Rule::IdMain);
    assert_eq!(a.rule.as_str(), "id_main");
    assert_eq!(
        a.body_text,
        "児童手当\n中学生までの子を養育している方に支給します。\n申請"
    );
    assert_eq!(a.links, [format!("http://{CITY}/kosodate/shinsei.html")]);
    assert_eq!(
        a.page_updated_on.map(|d| d.to_string()).as_deref(),
        Some("2024-03-01")
    );
    // ページ全体はヘッダの時刻で変わる
    assert_ne!(a.hashes.page, b.hashes.page);
}

#[tokio::test]
async fn our_name_is_sent_as_the_user_agent() {
    let (fetcher, site) = serve(quick(), |_, path, _| match path {
        "/robots.txt" => empty(404),
        _ => html("<p>ok</p>"),
    })
    .await;
    get(&fetcher, &format!("http://{CITY}/a.html")).await;
    for seen in site.seen() {
        assert_eq!(seen.headers[header::USER_AGENT], domain::fetch::USER_AGENT);
    }
}
