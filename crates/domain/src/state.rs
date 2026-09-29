//! 資源の状態遷移（§17）。
//!
//! 状態は資源の4値だけ。`moved`（別の資源への統合）は代表 URL の変化の解決で立てるので、ここでは動かさない。

use crate::liveness::Observation;

/// 見つからない観測がこの回数に達したら `deleted` にする（**仮置き**）。
/// 数えるのは観測の回数だけで、週1の間隔は守らせない（再訪の層の仕事）
pub const DELETE_AFTER_NOT_FOUND: i32 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Active,
    DeletionCandidate,
    Deleted,
    Moved,
}

impl State {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::DeletionCandidate => "deletion_candidate",
            Self::Deleted => "deleted",
            Self::Moved => "moved",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "active" => Some(Self::Active),
            "deletion_candidate" => Some(Self::DeletionCandidate),
            "deleted" => Some(Self::Deleted),
            "moved" => Some(Self::Moved),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Transition {
    pub state: State,
    pub consecutive_not_found: i32,
    /// 削除候補・削除済みから戻った。呼び出し側が `body_hash` を前回と比べる合図
    pub restored: bool,
}

pub fn next(state: State, consecutive_not_found: i32, observation: Observation) -> Transition {
    let stay = |state, consecutive_not_found| Transition {
        state,
        consecutive_not_found,
        restored: false,
    };
    match (state, observation) {
        (State::Moved, _) => stay(State::Moved, consecutive_not_found),
        (_, Observation::Alive) => Transition {
            state: State::Active,
            consecutive_not_found: 0,
            restored: matches!(state, State::DeletionCandidate | State::Deleted),
        },
        (_, Observation::Gone) => stay(State::Deleted, consecutive_not_found.saturating_add(1)),
        (_, Observation::NotFound(_)) => {
            let count = consecutive_not_found.saturating_add(1);
            if state == State::Deleted || count >= DELETE_AFTER_NOT_FOUND {
                stay(State::Deleted, count)
            } else {
                stay(State::DeletionCandidate, count)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::liveness::{Observation, Why};

    fn not_found() -> Observation {
        Observation::NotFound(Why::Status)
    }

    /// 観測を順に当てて、(状態, 数) の並びを返す
    fn run(start: State, observations: &[Observation]) -> Vec<(State, i32)> {
        let (mut state, mut count) = (start, 0);
        observations
            .iter()
            .map(|&observation| {
                let t = next(state, count, observation);
                (state, count) = (t.state, t.consecutive_not_found);
                (t.state, t.consecutive_not_found)
            })
            .collect()
    }

    #[test]
    fn three_not_founds_in_a_row_delete() {
        assert_eq!(
            run(State::Active, &[not_found(); 4]),
            vec![
                (State::DeletionCandidate, 1),
                (State::DeletionCandidate, 2),
                (State::Deleted, 3),
                (State::Deleted, 4),
            ]
        );
    }

    #[test]
    fn gone_deletes_at_once() {
        assert_eq!(
            run(State::Active, &[Observation::Gone]),
            vec![(State::Deleted, 1)]
        );
    }

    #[test]
    fn a_page_that_comes_back_is_active_again() {
        for start in [State::DeletionCandidate, State::Deleted] {
            let t = next(start, 2, Observation::Alive);
            assert_eq!(
                (t.state, t.consecutive_not_found, t.restored),
                (State::Active, 0, true),
                "{start:?}"
            );
        }
        let t = next(State::Active, 0, Observation::Alive);
        assert_eq!((t.state, t.restored), (State::Active, false));
    }

    #[test]
    fn a_run_of_not_founds_is_broken_by_a_live_response() {
        assert_eq!(
            run(
                State::Active,
                &[not_found(), Observation::Alive, not_found()]
            ),
            vec![
                (State::DeletionCandidate, 1),
                (State::Active, 0),
                (State::DeletionCandidate, 1),
            ]
        );
    }

    #[test]
    fn a_moved_resource_is_left_alone() {
        for observation in [Observation::Alive, not_found(), Observation::Gone] {
            let t = next(State::Moved, 0, observation);
            assert_eq!(
                (t.state, t.consecutive_not_found, t.restored),
                (State::Moved, 0, false)
            );
        }
    }

    #[test]
    fn names_match_the_database_values() {
        for (state, name) in [
            (State::Active, "active"),
            (State::DeletionCandidate, "deletion_candidate"),
            (State::Deleted, "deleted"),
            (State::Moved, "moved"),
        ] {
            assert_eq!(state.as_str(), name);
            assert_eq!(State::parse(name), Some(state));
        }
        assert_eq!(State::parse("blocked"), None);
    }
}
