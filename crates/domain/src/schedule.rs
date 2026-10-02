//! 完了したジョブの次の状態・再試行の回数・次の時刻までの間隔を決める（§12 §18）。
//!
//! 仮置き: バックオフの指数と jitter は、§12 の式の読み方と「URL ごとに散らせば足りる」ことから置いた。
//! - 失敗の回数 n（1から）について `retry_base × 2^(n-1)` を `retry_cap` で頭打ちにする
//! - jitter は URL の id から決める 0〜`jitter_max`（ミリ秒）。乱数の依存を足さず、テストで値が決まる
//!
//! 時間はすべて `Duration`（ミリ秒の精度）で持つ。秒に丸めない。

use std::time::Duration;

use crate::liveness::Job;

const DAY: Duration = Duration::from_secs(86_400);

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
    /// 成功した HTML の次の取得までの間隔
    pub page_interval: Duration,
    /// 成功した PDF の次の取得までの間隔
    pub pdf_interval: Duration,
    /// `Blocked` の再評価までの間隔
    pub blocked_interval: Duration,
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
            page_interval: 7 * DAY,
            pdf_interval: 30 * DAY,
            blocked_interval: 7 * DAY,
            failed_final_interval: 7 * DAY,
        }
    }
}

impl Policy {
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
}

/// 完了したジョブの次の状態。`retry_count` は完了前の値、`seed` は jitter の元（URL の id）
pub fn next(policy: &Policy, job: Job, retry_count: i32, pdf: bool, seed: u128) -> Next {
    match job {
        Job::Succeeded => Next {
            status: Status::Succeeded,
            retry_count: 0,
            after: if pdf {
                policy.pdf_interval
            } else {
                policy.page_interval
            },
        },
        Job::Blocked => Next {
            status: Status::Blocked,
            retry_count,
            after: policy.blocked_interval,
        },
        Job::Retry => {
            let failures = retry_count.saturating_add(1);
            if failures > policy.max_retries {
                Next {
                    status: Status::FailedFinal,
                    retry_count: failures,
                    after: policy.failed_final_interval,
                }
            } else {
                Next {
                    status: Status::RetryWait,
                    retry_count: failures,
                    after: backoff(policy, failures) + jitter(policy, seed),
                }
            }
        }
    }
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
    use crate::liveness::Job;

    #[test]
    fn success_resets_retries_and_waits_the_initial_interval() {
        let p = Policy::default();
        assert_eq!(
            next(&p, Job::Succeeded, 2, false, 0),
            Next {
                status: Status::Succeeded,
                retry_count: 0,
                after: Duration::from_secs(7 * 86_400)
            }
        );
        assert_eq!(
            next(&p, Job::Succeeded, 0, true, 0).after,
            Duration::from_secs(30 * 86_400)
        );
    }

    #[test]
    fn retries_back_off_then_give_up_on_the_third_failure() {
        let p = Policy::default();
        assert_eq!(
            next(&p, Job::Retry, 0, false, 0),
            Next {
                status: Status::RetryWait,
                retry_count: 1,
                after: Duration::from_secs(10)
            }
        );
        assert_eq!(
            next(&p, Job::Retry, 1, false, 0).after,
            Duration::from_secs(20)
        );
        assert_eq!(
            next(&p, Job::Retry, 2, false, 0),
            Next {
                status: Status::FailedFinal,
                retry_count: 3,
                after: Duration::from_secs(7 * 86_400)
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
            next(&p, Job::Retry, 20, false, 0).after,
            Duration::from_secs(600)
        );
        assert_eq!(
            next(&p, Job::Retry, 0, false, 5_000).after,
            Duration::from_millis(15_000)
        );
        assert_eq!(
            next(&p, Job::Retry, 0, false, 5_001).after,
            Duration::from_secs(10)
        );
        assert_eq!(p.longest_retry_wait(), Duration::from_secs(605));
    }

    #[test]
    fn blocked_keeps_the_retry_count() {
        let p = Policy::default();
        assert_eq!(
            next(&p, Job::Blocked, 1, false, 0),
            Next {
                status: Status::Blocked,
                retry_count: 1,
                after: Duration::from_secs(7 * 86_400)
            }
        );
    }
}
