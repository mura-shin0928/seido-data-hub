//! DB も HTTP も知らない純粋ロジック。

pub mod canonical;
pub mod change;
pub mod extract;
pub mod fetch;
pub mod liveness;
pub mod registry;
pub mod related;
pub mod run_report;
pub mod schedule;
pub mod state;
pub mod tags;
pub mod urls;
