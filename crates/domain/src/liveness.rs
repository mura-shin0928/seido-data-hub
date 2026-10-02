//! 取得の結果を、ジョブの扱いと資源への観測に分ける（§12 §17 §26-2）。
//!
//! ジョブの事実（取れたか・取れなかったか）と、資源の性質（生きているか・見つからないか）は別に扱う。
//! 403・robots・一時障害は、資源の性質について何も言えないので観測を返さない。

use url::Url;

use crate::canonical::{Hop, is_top_page};
use crate::fetch::{self, Redirect};
use crate::urls;

/// タイトルにこれを含むページは、見つからないことを伝えるページとみなす（小文字にそろえて比べる）
const NOT_FOUND_TITLES: [&str; 3] = ["見つかりません", "存在しません", "not found"];

#[derive(Debug, Clone, Copy)]
pub struct Input<'a> {
    /// 送ったリクエストの各段。先頭が取りに行った URL、最後が最終応答。無いこともある
    pub hops: &'a [Hop<'a>],
    pub ending: Ending<'a>,
    /// そのホストで、見つからないページのタイトルとして分かっている `title_hash`
    pub known_not_found_titles: &'a [&'a str],
}

#[derive(Debug, Clone, Copy)]
pub enum Ending<'a> {
    /// 転送を追い終えた最後の応答
    Response {
        status: u16,
        page: Option<Page<'a>>,
    },
    RobotsDenied,
    RobotsUnavailable,
    Network,
    /// 転送先が無い・多すぎる・ループした
    RedirectAnomaly,
    /// 許可リストの外などへの転送で止めた
    OutOfScope {
        location: &'a str,
    },
}

/// 読めた HTML の、ソフト404の判定に使う部分（本文の長さは使わない）
#[derive(Debug, Clone, Copy)]
pub struct Page<'a> {
    pub title: Option<&'a str>,
    pub title_hash: Option<&'a str>,
}

/// `urls.status` への効き方
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Job {
    Succeeded,
    /// 取りに行けない。再試行しても変わらない（403・robots・許可リストの外）
    Blocked,
    /// 一時的な失敗。あとで再試行する
    Retry,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Why {
    /// 404
    Status,
    /// 200 だが、タイトルが見つからないことを言っている
    TitleSaysNotFound,
    /// 200 だが、そのホストで既知の Not Found のタイトルと同じ
    KnownNotFoundTitle,
    /// 転送された先がトップページ。消えたページの受け皿
    TopRedirect,
}

/// 資源について分かったこと
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Observation {
    Alive,
    NotFound(Why),
    /// 410
    Gone,
}

impl Observation {
    /// `fetch_history.observation` に入れる名前
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Alive => "alive",
            Self::NotFound(Why::Status) => "not_found",
            Self::NotFound(Why::TitleSaysNotFound) => "soft_404_title",
            Self::NotFound(Why::KnownNotFoundTitle) => "soft_404_known_title",
            Self::NotFound(Why::TopRedirect) => "top_redirect",
            Self::Gone => "gone",
        }
    }
}

/// 許可リストの外へ、ホストだけが変わって恒久転送された（都の福祉保健局の例）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostMove {
    pub from_host_key: String,
    pub to_host_key: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verdict {
    pub job: Job,
    /// `urls.last_error_type` に入れる短い名前。成功のときは無い
    pub error_type: Option<&'static str>,
    /// 資源に当てる観測。資源の性質について何も言えないときは無い
    pub observation: Option<Observation>,
    pub host_move: Option<HostMove>,
}

impl Verdict {
    fn of(job: Job, error_type: Option<&'static str>) -> Self {
        Self {
            job,
            error_type,
            observation: None,
            host_move: None,
        }
    }

    fn observed(observation: Observation) -> Self {
        Self {
            observation: Some(observation),
            ..Self::of(Job::Succeeded, None)
        }
    }
}

pub fn classify(input: &Input<'_>) -> Verdict {
    match input.ending {
        Ending::Response { status, page } => response(input, status, page),
        Ending::RobotsDenied => Verdict::of(Job::Blocked, Some("robots_denied")),
        Ending::RobotsUnavailable => Verdict::of(Job::Retry, Some("robots_unavailable")),
        Ending::Network => Verdict::of(Job::Retry, Some("network")),
        Ending::RedirectAnomaly => Verdict::of(Job::Retry, Some("redirect")),
        Ending::OutOfScope { location } => {
            let host_move = input.hops.last().and_then(|last| host_move(last, location));
            let error_type = if host_move.is_some() {
                "host_moved"
            } else {
                "out_of_scope"
            };
            Verdict {
                host_move,
                ..Verdict::of(Job::Blocked, Some(error_type))
            }
        }
    }
}

fn response(input: &Input<'_>, status: u16, page: Option<Page<'_>>) -> Verdict {
    match status {
        200..=299 => Verdict::observed(alive_or_soft_404(input, page)),
        // 応答があった。本文は無いので他には何も言えない
        304 => Verdict::observed(Observation::Alive),
        404 => Verdict::observed(Observation::NotFound(Why::Status)),
        410 => Verdict::observed(Observation::Gone),
        401 | 403 => Verdict::of(Job::Blocked, Some("forbidden")),
        429 => Verdict::of(Job::Retry, Some("rate_limited")),
        500..=599 => Verdict::of(Job::Retry, Some("server_error")),
        _ => Verdict::of(Job::Retry, Some("unexpected_status")),
    }
}

fn alive_or_soft_404(input: &Input<'_>, page: Option<Page<'_>>) -> Observation {
    if let (Some(start), Some(last)) = (input.hops.first(), input.hops.last())
        && input.hops.len() > 1
        && is_top_page(last.url)
        && !is_top_page(start.url)
    {
        return Observation::NotFound(Why::TopRedirect);
    }
    let Some(page) = page else {
        return Observation::Alive;
    };
    if page.title.is_some_and(says_not_found) {
        return Observation::NotFound(Why::TitleSaysNotFound);
    }
    if page
        .title_hash
        .is_some_and(|hash| input.known_not_found_titles.contains(&hash))
    {
        return Observation::NotFound(Why::KnownNotFoundTitle);
    }
    Observation::Alive
}

fn says_not_found(title: &str) -> bool {
    let title = title.to_lowercase();
    NOT_FOUND_TITLES.iter().any(|phrase| title.contains(phrase))
}

/// 最後の段が恒久転送で、ホストだけが変わり、パスとクエリが同じ
fn host_move(last: &Hop<'_>, location: &str) -> Option<HostMove> {
    if fetch::redirect_kind(last.status) != Some(Redirect::Permanent) {
        return None;
    }
    let from = urls::prepare(last.url).ok()?;
    let to = urls::prepare(location).ok()?;
    let (a, b) = (
        Url::parse(&from.normalized_url).ok()?,
        Url::parse(&to.normalized_url).ok()?,
    );
    // ホスト（domain + port）が同じなら host move ではない（scheme は無視）
    if a.host() == b.host() {
        return None;
    }
    (a.path() == b.path() && a.query() == b.query()).then_some(HostMove {
        from_host_key: from.host_key,
        to_host_key: to.host_key,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const CITY: &str = "https://www.city.example.jp";

    fn verdict(steps: &[(&str, u16)], ending: Ending<'_>) -> Verdict {
        let hops: Vec<Hop<'_>> = steps
            .iter()
            .map(|&(url, status)| Hop { url, status })
            .collect();
        classify(&Input {
            hops: &hops,
            ending,
            known_not_found_titles: &[],
        })
    }

    fn respond(status: u16, title: Option<&str>) -> Ending<'_> {
        Ending::Response {
            status,
            page: Some(Page {
                title,
                title_hash: None,
            }),
        }
    }

    #[test]
    fn observations_have_the_names_kept_in_history() {
        assert_eq!(Observation::Alive.as_str(), "alive");
        assert_eq!(Observation::NotFound(Why::Status).as_str(), "not_found");
        assert_eq!(
            Observation::NotFound(Why::TitleSaysNotFound).as_str(),
            "soft_404_title"
        );
        assert_eq!(
            Observation::NotFound(Why::KnownNotFoundTitle).as_str(),
            "soft_404_known_title"
        );
        assert_eq!(
            Observation::NotFound(Why::TopRedirect).as_str(),
            "top_redirect"
        );
        assert_eq!(Observation::Gone.as_str(), "gone");
    }

    #[test]
    fn statuses_map_to_a_job_and_an_observation() {
        let steps = [("https://www.city.example.jp/a.html", 200)];
        let cases = [
            (200, Job::Succeeded, Some(Observation::Alive)),
            (304, Job::Succeeded, Some(Observation::Alive)),
            (
                404,
                Job::Succeeded,
                Some(Observation::NotFound(Why::Status)),
            ),
            (410, Job::Succeeded, Some(Observation::Gone)),
            (401, Job::Blocked, None),
            (403, Job::Blocked, None),
            (429, Job::Retry, None),
            (500, Job::Retry, None),
            (503, Job::Retry, None),
            (400, Job::Retry, None),
        ];
        for (status, job, observation) in cases {
            let got = verdict(&steps, respond(status, None));
            assert_eq!(
                (got.job, got.observation),
                (job, observation),
                "status {status}"
            );
        }
    }

    #[test]
    fn a_permanent_redirect_to_a_404_is_not_found() {
        // 小金井市の /smph/ の実測: 301 の先が404
        let got = verdict(
            &[
                ("https://www.city.example.jp/smph/a.html", 301),
                ("https://www.city.example.jp/a.html", 404),
            ],
            respond(404, None),
        );
        assert_eq!(got.observation, Some(Observation::NotFound(Why::Status)));
        assert_eq!(got.job, Job::Succeeded);
    }

    #[test]
    fn endings_without_a_response() {
        let cases = [
            (Ending::RobotsDenied, Job::Blocked, "robots_denied"),
            (Ending::RobotsUnavailable, Job::Retry, "robots_unavailable"),
            (Ending::Network, Job::Retry, "network"),
            (Ending::RedirectAnomaly, Job::Retry, "redirect"),
        ];
        for (ending, job, error_type) in cases {
            let got = verdict(&[], ending);
            assert_eq!(got.job, job);
            assert_eq!(got.error_type, Some(error_type));
            assert_eq!(got.observation, None);
            assert_eq!(got.host_move, None);
        }
        let forbidden = verdict(&[], respond(403, None));
        assert_eq!(forbidden.error_type, Some("forbidden"));
    }

    #[test]
    fn a_redirect_to_the_top_page_is_not_found() {
        for status in [301, 302, 307, 308] {
            let got = verdict(
                &[
                    ("https://www.city.example.jp/old.html", status),
                    ("https://www.city.example.jp/", 200),
                ],
                respond(200, Some("○○市")),
            );
            assert_eq!(
                got.observation,
                Some(Observation::NotFound(Why::TopRedirect)),
                "{status}"
            );
        }
    }

    #[test]
    fn top_pages_and_ordinary_redirects_are_alive() {
        let cases: [&[(&str, u16)]; 4] = [
            &[("https://www.city.example.jp/", 200)],
            &[
                ("https://www.city.example.jp/", 301),
                ("https://www.city.example.jp/index.html", 200),
            ],
            &[
                ("https://www.city.example.jp/index.html", 301),
                ("https://www.city.example.jp/", 200),
            ],
            &[
                ("https://www.city.example.jp/a.html", 301),
                ("https://www.city.example.jp/b.html", 200),
            ],
        ];
        for steps in cases {
            let got = verdict(steps, respond(200, Some("○○市")));
            assert_eq!(got.observation, Some(Observation::Alive), "{steps:?}");
        }
    }

    #[test]
    fn titles_that_say_not_found_are_soft_404s() {
        let steps = [("https://www.city.example.jp/x.html", 200)];
        for title in [
            "ページが見つかりません｜○○市",
            "指定されたページは存在しません",
            "404 Not Found",
            "Page NOT FOUND",
        ] {
            let got = verdict(&steps, respond(200, Some(title)));
            assert_eq!(
                got.observation,
                Some(Observation::NotFound(Why::TitleSaysNotFound)),
                "{title}"
            );
        }
    }

    #[test]
    fn ordinary_titles_stay_alive_even_when_they_mention_missing_things() {
        let steps = [("https://www.city.example.jp/x.html", 200)];
        for title in [
            "児童手当のご案内",
            "保育所が見つからない場合",
            "存在しない手続きについて",
        ] {
            let got = verdict(&steps, respond(200, Some(title)));
            assert_eq!(got.observation, Some(Observation::Alive), "{title}");
        }
        assert_eq!(
            verdict(&steps, respond(200, None)).observation,
            Some(Observation::Alive)
        );
        // PDF など本文を読まない応答
        let pdf = verdict(
            &steps,
            Ending::Response {
                status: 200,
                page: None,
            },
        );
        assert_eq!(pdf.observation, Some(Observation::Alive));
    }

    #[test]
    fn a_title_hash_known_as_not_found_is_a_soft_404() {
        let hops = [Hop {
            url: "https://www.city.example.jp/x.html",
            status: 200,
        }];
        let got = classify(&Input {
            hops: &hops,
            ending: Ending::Response {
                status: 200,
                page: Some(Page {
                    title: Some("お知らせ"),
                    title_hash: Some("abc"),
                }),
            },
            known_not_found_titles: &["abc"],
        });
        assert_eq!(
            got.observation,
            Some(Observation::NotFound(Why::KnownNotFoundTitle))
        );
    }

    #[test]
    fn a_permanent_redirect_to_another_host_with_the_same_path_is_a_host_move() {
        let got = verdict(
            &[("https://www.old.example.jp/a/b.html?id=1", 301)],
            Ending::OutOfScope {
                location: "https://www.new.example.jp/a/b.html?id=1",
            },
        );
        assert_eq!(got.job, Job::Blocked);
        assert_eq!(got.error_type, Some("host_moved"));
        assert_eq!(got.observation, None);
        assert_eq!(
            got.host_move,
            Some(HostMove {
                from_host_key: "https://www.old.example.jp".to_string(),
                to_host_key: "https://www.new.example.jp".to_string(),
            })
        );
    }

    #[test]
    fn other_redirects_out_of_scope_are_not_host_moves() {
        let cases = [
            // パスが変わる
            (
                ("https://www.old.example.jp/a.html", 301),
                "https://www.new.example.jp/b.html",
            ),
            // クエリが変わる
            (
                ("https://www.old.example.jp/a.html?x=1", 301),
                "https://www.new.example.jp/a.html",
            ),
            // 一時転送
            (
                ("https://www.old.example.jp/a.html", 302),
                "https://www.new.example.jp/a.html",
            ),
            // 同じホスト（scheme が変わるだけ）
            (
                ("http://www.old.example.jp/a.html", 301),
                "https://www.old.example.jp/a.html",
            ),
        ];
        for ((from, status), location) in cases {
            let got = verdict(&[(from, status)], Ending::OutOfScope { location });
            assert_eq!(got.job, Job::Blocked, "{from}");
            assert_eq!(got.error_type, Some("out_of_scope"), "{from}");
            assert_eq!(got.host_move, None, "{from}");
        }
        // 最初の URL が scope 外（転送の段が無い）
        let none = verdict(&[], Ending::OutOfScope { location: CITY });
        assert_eq!(none.host_move, None);
    }
}
