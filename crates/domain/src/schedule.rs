//! 完了したジョブの次の状態・再試行の回数・次の時刻までの間隔を決める（§12 §18）。
//!
//! 仮置き: バックオフの指数と jitter は、§12 の式の読み方と「URL ごとに散らせば足りる」ことから置いた。
//! - 失敗の回数 n（1から）について `retry_base × 2^(n-1)` を `retry_cap` で頭打ちにする
//! - jitter は URL の id から決める 0〜`jitter_max`（ミリ秒）。乱数の依存を足さず、テストで値が決まる
//!
//! 時間はすべて `Duration`（ミリ秒の精度）で持つ。秒に丸めない。
//!
//! 成功と blocked のたびに、URL ごとの再訪の間隔を伸び縮みさせる（§18）。種類ごとの既定値:
//!
//! | `Bounds` | initial | min | max |
//! |---|---|---|---|
//! | `page` | 7日 | 3日 | 14日 |
//! | `pdf` | 30日 | 14日 | 90日 |
//! | `deletion_candidate` | 7日 | 3日 | 7日 |
//! | `blocked` | 7日 | 7日 | 30日 |
//!
//! 仮置き:
//! - 前回の間隔がまだ無ければ `initial`。あれば種類の `min`〜`max` に収め直してから伸び縮みさせる
//!   （種類が変わったときに、前の種類の間隔を引きずらない）
//! - HTML・PDF は、変化ありで半分・変化なしで1.5倍・比べられなければそのまま。
//!   `deletion_candidate` は変えず、`blocked` は毎回1.5倍にする
//! - 失敗（再試行・`FailedFinal`）は間隔を変えず、渡された値のまま返す。待ちは再試行のバックオフに任せる

use std::time::Duration;

use crate::change::Change;
use crate::liveness::Job;
use crate::state::State;

const DAY: Duration = Duration::from_secs(86_400);

/// 再訪の間隔の初期値と下限・上限
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Bounds {
    pub initial: Duration,
    pub min: Duration,
    pub max: Duration,
}

impl Bounds {
    const fn days(initial: u64, min: u64, max: u64) -> Self {
        Self {
            initial: Duration::from_secs(initial * 86_400),
            min: Duration::from_secs(min * 86_400),
            max: Duration::from_secs(max * 86_400),
        }
    }
}

/// 間隔の種類
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Page,
    Pdf,
    DeletionCandidate,
    Blocked,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Policy {
    /// 再試行の最大回数。これを超える失敗で `FailedFinal` にする
    pub max_retries: i32,
    /// 1回目の失敗後の待ち
    pub retry_base: Duration,
    /// バックオフの上限（jitter を足す前）
    pub retry_cap: Duration,
    /// jitter の上限
    pub jitter_max: Duration,
    /// 成功した HTML の再訪の間隔
    pub page: Bounds,
    /// 成功した PDF の再訪の間隔
    pub pdf: Bounds,
    /// 削除候補の再訪の間隔
    pub deletion_candidate: Bounds,
    /// `Blocked` の再評価の間隔
    pub blocked: Bounds,
    /// `FailedFinal` の再評価までの間隔
    pub failed_final_interval: Duration,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            max_retries: 2,
            retry_base: Duration::from_secs(10),
            retry_cap: Duration::from_secs(600),
            jitter_max: Duration::from_secs(5),
            page: Bounds::days(7, 3, 14),
            pdf: Bounds::days(30, 14, 90),
            deletion_candidate: Bounds::days(7, 3, 7),
            blocked: Bounds::days(7, 7, 30),
            failed_final_interval: 7 * DAY,
        }
    }
}

impl Policy {
    /// 種類ごとの間隔の初期値と上下限
    pub fn bounds(&self, kind: Kind) -> Bounds {
        match kind {
            Kind::Page => self.page,
            Kind::Pdf => self.pdf,
            Kind::DeletionCandidate => self.deletion_candidate,
            Kind::Blocked => self.blocked,
        }
    }

    /// 実行の中で再試行を待つ上限（`retry_cap` + `jitter_max`）
    pub fn longest_retry_wait(&self) -> Duration {
        self.retry_cap + self.jitter_max
    }
}

/// 完了後の `urls.status`
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Succeeded,
    RetryWait,
    FailedFinal,
    Blocked,
}

impl Status {
    /// `urls.status` に書く値
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Succeeded => "succeeded",
            Status::RetryWait => "retry_wait",
            Status::FailedFinal => "failed_final",
            Status::Blocked => "blocked",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Next {
    pub status: Status,
    /// 完了後の `retry_count`
    pub retry_count: i32,
    /// 次に取れるようになるまでの間隔
    pub after: Duration,
    /// `urls.recrawl_interval_secs` に書く値。失敗では渡された値のまま
    pub interval: Option<Duration>,
}

/// 完了したジョブと、その取得の種類・内容の変化
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Visit {
    pub job: Job,
    pub kind: Kind,
    pub change: Change,
}

/// 間隔の種類。blocked のジョブ、削除候補の資源、PDF、それ以外（HTML）の順に決める
pub fn kind(job: Job, state: Option<State>, pdf: bool) -> Kind {
    if job == Job::Blocked {
        Kind::Blocked
    } else if state == Some(State::DeletionCandidate) {
        Kind::DeletionCandidate
    } else if pdf {
        Kind::Pdf
    } else {
        Kind::Page
    }
}

/// 完了したジョブの次の状態。`retry_count` は完了前の値、`interval` は前回の間隔、`seed` は jitter の元（URL の id）
pub fn next(
    policy: &Policy,
    visit: Visit,
    retry_count: i32,
    interval: Option<Duration>,
    seed: u128,
) -> Next {
    match visit.job {
        Job::Succeeded => {
            let new = adapt(policy.bounds(visit.kind), visit, interval);
            Next {
                status: Status::Succeeded,
                retry_count: 0,
                after: new,
                interval: Some(new),
            }
        }
        Job::Blocked => {
            let new = adapt(policy.bounds(visit.kind), visit, interval);
            Next {
                status: Status::Blocked,
                retry_count,
                after: new,
                interval: Some(new),
            }
        }
        Job::Retry => {
            let failures = retry_count.saturating_add(1);
            if failures > policy.max_retries {
                Next {
                    status: Status::FailedFinal,
                    retry_count: failures,
                    after: policy.failed_final_interval,
                    interval,
                }
            } else {
                Next {
                    status: Status::RetryWait,
                    retry_count: failures,
                    after: backoff(policy, failures) + jitter(policy, seed),
                    interval,
                }
            }
        }
    }
}

/// 前回の間隔を種類の範囲に収め、変化に応じて伸び縮みさせて、もう一度収める
fn adapt(bounds: Bounds, visit: Visit, interval: Option<Duration>) -> Duration {
    let Some(interval) = interval else {
        return bounds.initial;
    };
    let base = interval.clamp(bounds.min, bounds.max);
    let moved = match (visit.kind, visit.change) {
        (Kind::Blocked, _) => base.mul_f64(1.5),
        (Kind::DeletionCandidate, _) => base,
        (_, Change::Changed) => base / 2,
        (_, Change::Unchanged) => base.mul_f64(1.5),
        (_, Change::Unknown) => base,
    };
    moved.clamp(bounds.min, bounds.max)
}

/// n回目（1から）の失敗後の待ち。2の冪は飽和させ、`retry_cap` で頭打ちにする
fn backoff(policy: &Policy, failures: i32) -> Duration {
    let exponent = u32::try_from(failures.saturating_sub(1)).unwrap_or(0);
    let factor = 2u32.checked_pow(exponent).unwrap_or(u32::MAX);
    policy
        .retry_base
        .saturating_mul(factor)
        .min(policy.retry_cap)
}

/// `seed % (jitter_max のミリ秒 + 1)` ミリ秒
fn jitter(policy: &Policy, seed: u128) -> Duration {
    let millis = (seed % (policy.jitter_max.as_millis() + 1)) as u64;
    Duration::from_millis(millis)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::change::Change;
    use crate::liveness::Job;
    use crate::state::State;

    fn d(n: u64) -> Duration {
        Duration::from_secs(n * 86_400)
    }

    fn ok(kind: Kind, change: Change) -> Visit {
        Visit {
            job: Job::Succeeded,
            kind,
            change,
        }
    }

    fn retry() -> Visit {
        Visit {
            job: Job::Retry,
            kind: Kind::Page,
            change: Change::Unknown,
        }
    }

    #[test]
    fn success_resets_retries_and_waits_the_initial_interval() {
        let p = Policy::default();
        assert_eq!(
            next(&p, ok(Kind::Page, Change::Unknown), 2, None, 0),
            Next {
                status: Status::Succeeded,
                retry_count: 0,
                after: d(7),
                interval: Some(d(7)),
            }
        );
        assert_eq!(
            next(&p, ok(Kind::Pdf, Change::Unknown), 0, None, 0).after,
            d(30)
        );
    }

    #[test]
    fn retries_back_off_then_give_up_on_the_third_failure() {
        let p = Policy::default();
        assert_eq!(
            next(&p, retry(), 0, None, 0),
            Next {
                status: Status::RetryWait,
                retry_count: 1,
                after: Duration::from_secs(10),
                interval: None,
            }
        );
        assert_eq!(next(&p, retry(), 1, None, 0).after, Duration::from_secs(20));
        assert_eq!(
            next(&p, retry(), 2, None, 0),
            Next {
                status: Status::FailedFinal,
                retry_count: 3,
                after: d(7),
                interval: None,
            }
        );
    }

    #[test]
    fn backoff_is_capped_and_jitter_stays_within_five_seconds() {
        let p = Policy {
            max_retries: 100,
            ..Policy::default()
        };
        assert_eq!(
            next(&p, retry(), 20, None, 0).after,
            Duration::from_secs(600)
        );
        assert_eq!(
            next(&p, retry(), 0, None, 5_000).after,
            Duration::from_millis(15_000)
        );
        assert_eq!(
            next(&p, retry(), 0, None, 5_001).after,
            Duration::from_secs(10)
        );
        assert_eq!(p.longest_retry_wait(), Duration::from_secs(605));
    }

    #[test]
    fn blocked_keeps_the_retry_count() {
        let p = Policy::default();
        let blocked = Visit {
            job: Job::Blocked,
            kind: Kind::Blocked,
            change: Change::Unknown,
        };
        assert_eq!(
            next(&p, blocked, 1, None, 0),
            Next {
                status: Status::Blocked,
                retry_count: 1,
                after: d(7),
                interval: Some(d(7)),
            }
        );
    }

    #[test]
    fn a_change_halves_and_no_change_stretches_within_the_page_bounds() {
        let p = Policy::default();
        assert_eq!(
            next(&p, ok(Kind::Page, Change::Changed), 0, Some(d(7)), 0).after,
            Duration::from_secs(302_400)
        ); // 3.5日
        assert_eq!(
            next(
                &p,
                ok(Kind::Page, Change::Changed),
                0,
                Some(Duration::from_secs(302_400)),
                0
            )
            .after,
            d(3)
        );
        assert_eq!(
            next(&p, ok(Kind::Page, Change::Unchanged), 0, Some(d(7)), 0).after,
            Duration::from_secs(907_200)
        ); // 10.5日
        assert_eq!(
            next(
                &p,
                ok(Kind::Page, Change::Unchanged),
                0,
                Some(Duration::from_secs(907_200)),
                0
            )
            .after,
            d(14)
        );
    }

    #[test]
    fn the_initial_value_is_used_without_a_previous_interval_and_unknown_keeps_it() {
        let p = Policy::default();
        let n = next(&p, ok(Kind::Page, Change::Unchanged), 0, None, 0);
        assert_eq!((n.after, n.interval), (d(7), Some(d(7))));
        assert_eq!(
            next(&p, ok(Kind::Pdf, Change::Unknown), 0, None, 0).after,
            d(30)
        );
        assert_eq!(
            next(&p, ok(Kind::Page, Change::Unknown), 0, Some(d(10)), 0).after,
            d(10)
        );
    }

    #[test]
    fn pdfs_stretch_up_to_ninety_days() {
        let p = Policy::default();
        assert_eq!(
            next(&p, ok(Kind::Pdf, Change::Unchanged), 0, Some(d(30)), 0).after,
            d(45)
        );
        assert_eq!(
            next(&p, ok(Kind::Pdf, Change::Unchanged), 0, Some(d(80)), 0).after,
            d(90)
        );
        assert_eq!(
            next(&p, ok(Kind::Pdf, Change::Changed), 0, Some(d(20)), 0).after,
            d(14)
        );
    }

    #[test]
    fn deletion_candidates_stay_between_three_and_seven_days() {
        let p = Policy::default();
        assert_eq!(
            next(
                &p,
                ok(Kind::DeletionCandidate, Change::Unchanged),
                0,
                Some(d(14)),
                0
            )
            .after,
            d(7)
        );
        assert_eq!(
            next(
                &p,
                ok(Kind::DeletionCandidate, Change::Unchanged),
                0,
                Some(Duration::from_secs(302_400)),
                0
            )
            .after,
            Duration::from_secs(302_400)
        );
    }

    #[test]
    fn blocked_stretches_up_to_thirty_days() {
        let p = Policy::default();
        let blocked = Visit {
            job: Job::Blocked,
            kind: Kind::Blocked,
            change: Change::Unknown,
        };
        assert_eq!(next(&p, blocked, 1, None, 0).after, d(7));
        assert_eq!(
            next(&p, blocked, 1, Some(d(3)), 0).after,
            Duration::from_secs(907_200)
        ); // 7日に収めてから ×1.5
        assert_eq!(next(&p, blocked, 1, Some(d(25)), 0).after, d(30));
        assert_eq!(next(&p, blocked, 1, None, 0).retry_count, 1);
    }

    #[test]
    fn failures_keep_the_interval() {
        let p = Policy::default();
        let n = next(&p, retry(), 0, Some(d(10)), 0);
        assert_eq!(
            (n.status, n.after, n.interval),
            (Status::RetryWait, Duration::from_secs(10), Some(d(10)))
        );
        let n = next(&p, retry(), 2, Some(d(10)), 0);
        assert_eq!(
            (n.status, n.after, n.interval),
            (Status::FailedFinal, d(7), Some(d(10)))
        );
    }

    #[test]
    fn the_kind_follows_the_job_the_state_and_the_type() {
        assert_eq!(kind(Job::Blocked, None, false), Kind::Blocked);
        assert_eq!(
            kind(Job::Succeeded, Some(State::DeletionCandidate), true),
            Kind::DeletionCandidate
        );
        assert_eq!(kind(Job::Succeeded, Some(State::Active), true), Kind::Pdf);
        assert_eq!(
            kind(Job::Succeeded, Some(State::Deleted), false),
            Kind::Page
        );
        assert_eq!(kind(Job::Succeeded, None, false), Kind::Page);
    }
}
