//! 統合経路探索。個別クレート (street/raptor/fares) をまとめ、リクエストから
//! 経路候補を組み立てる。babymobi の `Route` スキーマに対応する応答を返すのが最終形。
//!
//! OTP の `routing.algorithm` 統合層相当。1クエリのフロー:
//!   出発地→駅の徒歩 (street) → RAPTOR 乗換 (raptor) → 駅→目的地の徒歩 (street)
//!   → 運賃 (fares) → アクセシビリティ注記付きで応答。

use otp_core::{LatLng, Result, SecondsSinceMidnight};
use otp_street::WalkProfile;

/// モビリティ種別 (babymobi の mobilityMode に対応)。
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Mobility {
    Solo,
    Stroller,
    Wheelchair,
}

impl Mobility {
    pub fn walk_profile(self) -> WalkProfile {
        match self {
            Mobility::Solo => WalkProfile::normal(),
            Mobility::Stroller => WalkProfile::stroller(),
            Mobility::Wheelchair => WalkProfile::wheelchair(),
        }
    }
}

/// 経路探索リクエスト。
#[derive(Debug, Clone)]
pub struct RouteRequest {
    pub origin: LatLng,
    pub destination: LatLng,
    pub depart_at: SecondsSinceMidnight,
    pub mobility: Mobility,
}

/// 応答の1区間 (徒歩 or 乗車)。
#[derive(Debug, Clone)]
pub enum Leg {
    Walk {
        distance_m: f32,
        duration_s: u32,
        has_stairs: bool,
    },
    Transit {
        route_name: String,
        from_stop: String,
        to_stop: String,
        duration_s: u32,
    },
}

/// 応答の1経路。
#[derive(Debug, Clone)]
pub struct Itinerary {
    pub legs: Vec<Leg>,
    pub total_duration_s: u32,
    pub transfers: u8,
    /// 運賃 (円)。運賃データが無い区間を含む場合は None。
    pub fare_yen: Option<f64>,
}

/// エンジン本体。構築済みグラフ/時刻表/運賃モデルを保持し、リクエストに応答する。
///
/// これがネイティブサーバ (otp-server) の中身であり、将来 wasm32 で Worker に載せる対象。
pub struct Engine {
    pub street: otp_street::StreetGraph,
    pub timetable: otp_raptor::Timetable,
    pub fares: otp_fares::FareModel,
}

impl Engine {
    pub fn new(
        street: otp_street::StreetGraph,
        timetable: otp_raptor::Timetable,
        fares: otp_fares::FareModel,
    ) -> Self {
        Self {
            street,
            timetable,
            fares,
        }
    }

    /// 経路探索。
    ///
    /// TODO(移植): access/egress 徒歩探索 → RAPTOR → 運賃 → Itinerary 組み立て。
    /// まず「徒歩+鉄道1本」の最小ケースを通し、本家 OTP と突き合わせる。
    ///
    /// otp-street 側の準備は済んでいる: `self.street.route(from, to,
    /// &req.mobility.walk_profile())` が `WalkPath { nodes, distance_m,
    /// duration_s, has_stairs }` を返すので、access (出発地→最寄駅) と egress
    /// (最寄駅→目的地) はそれぞれ1回ずつ呼べば済む形になっている。駅の座標は
    /// `otp_raptor::Timetable` 側から引く必要があり (現状 stop_id ベースなので
    /// 座標を持たせるか別マッピングが要る)、そこが次スライスの最初の課題。
    /// `has_stairs` は `Leg::Walk` にそのまま渡せる。
    pub fn plan(&self, _req: &RouteRequest) -> Result<Vec<Itinerary>> {
        Err(otp_core::Error::Unimplemented("Engine::plan"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mobility_maps_to_profile() {
        assert_eq!(
            Mobility::Stroller.walk_profile().stairs_reluctance,
            WalkProfile::stroller().stairs_reluctance
        );
    }
}
