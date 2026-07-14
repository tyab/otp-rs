//! 運賃計算。まず GTFS-Fares v1 (`fare_attributes` + `fare_rules`) に対応する。
//!
//! OTP の `ext.fares` 相当。日本の鉄道運賃は距離制で、GTFS では主に
//! `fare_rules` の `origin_id`/`destination_id` (運賃ゾーン間) で表現される。
//! 自前頻度 GTFS の JR は運賃データを持たないため、距離制運賃表を別途与える
//! 拡張ポイント (`FareModel`) を用意する。

use otp_core::Result;
use otp_gtfs::{FareAttribute, FareRule};

/// 運賃額 (円などの通貨単位。GTFS `price` をそのまま持つ)。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Money {
    pub amount: f64,
    // 通貨は当面 JPY 前提。多通貨対応は完全移植で。
}

/// 運賃計算に渡す1つの乗車区間 (どの運賃ゾーン間を、どの路線で乗ったか)。
#[derive(Debug, Clone)]
pub struct FareLeg {
    pub route_id: Option<otp_core::RouteId>,
    pub origin_zone: Option<String>,
    pub destination_zone: Option<String>,
    pub contains_zones: Vec<String>,
}

/// GTFS-Fares v1 の運賃エンジン。
#[derive(Debug, Default)]
pub struct FareModel {
    pub attributes: Vec<FareAttribute>,
    pub rules: Vec<FareRule>,
}

impl FareModel {
    pub fn from_gtfs(feed: &otp_gtfs::Feed) -> Self {
        Self {
            attributes: feed.fare_attributes.clone(),
            rules: feed.fare_rules.clone(),
        }
    }

    /// 1区間に適用される運賃 ID を規則から探す (route/origin/destination の一致)。
    ///
    /// TODO(移植): GTFS-Fares v1 の一致規則を厳密に実装
    /// (route_id・origin_id・destination_id・contains_id の AND 条件、
    /// 最安一致の選択)。乗換割引は fare_attributes.transfers で扱う。
    pub fn fare_for_leg(&self, _leg: &FareLeg) -> Option<&FareAttribute> {
        None // TODO
    }

    /// 経路全体 (複数区間) の合計運賃を計算する。
    ///
    /// TODO(移植): 乗換許容 (transfers/transfer_duration) を跨いだ通し運賃、
    /// 事業者跨ぎの分割・通算を実装。
    pub fn total_fare(&self, _legs: &[FareLeg]) -> Result<Money> {
        Err(otp_core::Error::Unimplemented("FareModel::total_fare"))
    }
}

/// JR のような GTFS 運賃を持たない事業者向けの距離制運賃フック (完全移植で本実装)。
pub trait DistanceFare {
    fn fare_for_distance(&self, km: f64) -> Money;
}
