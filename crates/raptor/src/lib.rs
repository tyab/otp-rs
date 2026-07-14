//! RAPTOR による時刻表ベースのマルチモーダル乗換探索。
//!
//! OTP の `raptor` パッケージ相当。`otp-gtfs::Feed` 群からコンパクトな時刻表
//! (`Timetable`) を構築し、ラウンドごとに到達可能停留所を更新する RAPTOR を回す。
//! バス込みでも詰めれば数十MB (実測見積り) で常駐可能な想定。

use otp_core::{Result, SecondsSinceMidnight, StopId};

/// 停留所のコンパクト添字。
pub type StopIdx = u32;
/// 便のコンパクト添字。
pub type TripIdx = u32;

/// RAPTOR 用に詰めた時刻表。
///
/// TODO(移植): route パターン化 (同一停車列の便をまとめる)、stop_times の
/// SoA 配列化、乗換 (transfers) の隣接リスト化。
#[derive(Debug, Default)]
pub struct Timetable {
    /// StopId ↔ StopIdx の対応。
    pub stops: Vec<StopId>,
    // TODO: patterns, trip_departures[pattern][trip][stop], transfers ...
}

impl Timetable {
    /// `otp-gtfs::Feed` 群から時刻表を構築する。
    pub fn build(_feeds: &[otp_gtfs::Feed]) -> Result<Timetable> {
        Err(otp_core::Error::Unimplemented("Timetable::build"))
    }
}

/// 徒歩アクセス/イグレス (出発地→駅, 駅→目的地) の1本。
/// `otp-street` の歩行探索結果から与える。
#[derive(Debug, Clone)]
pub struct StreetLink {
    pub stop: StopIdx,
    pub duration_s: u32,
}

/// RAPTOR クエリ。
#[derive(Debug, Clone)]
pub struct RaptorQuery {
    pub access: Vec<StreetLink>,
    pub egress: Vec<StreetLink>,
    pub earliest_departure: SecondsSinceMidnight,
    /// 最大乗換回数 (RAPTOR のラウンド数上限)。
    pub max_rounds: u8,
}

/// 探索結果の1区間。
#[derive(Debug, Clone)]
pub enum JourneyLeg {
    Walk { duration_s: u32 },
    Transit { trip: TripIdx, from: StopIdx, to: StopIdx, board_s: SecondsSinceMidnight, alight_s: SecondsSinceMidnight },
}

/// 探索結果の1経路 (Pareto 最適候補の1つ)。
#[derive(Debug, Clone)]
pub struct Journey {
    pub legs: Vec<JourneyLeg>,
    pub arrival_s: SecondsSinceMidnight,
    pub transfers: u8,
}

impl Timetable {
    /// RAPTOR 探索本体。到着時刻と乗換回数で Pareto 最適な複数経路を返す。
    ///
    /// TODO(移植): 標準 RAPTOR (ラウンド = 乗車→乗換→徒歩) を実装。
    pub fn search(&self, _query: &RaptorQuery) -> Result<Vec<Journey>> {
        Err(otp_core::Error::Unimplemented("Timetable::search"))
    }
}
