//! 統合経路探索。個別クレート (street/raptor/fares) をまとめ、リクエストから
//! 経路候補を組み立てる。babymobi の `Route` スキーマに対応する応答を返すのが最終形。
//!
//! OTP の `routing.algorithm` 統合層相当。1クエリのフロー:
//!   出発地→駅の徒歩 (street) → RAPTOR 乗換 (raptor) → 駅→目的地の徒歩 (street)
//!   → 運賃 (fares) → アクセシビリティ注記付きで応答。

use std::collections::HashMap;

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
    /// 対象サービス日 (YYYYMMDD)。RAPTOR の calendar/calendar_dates 判定に必須
    /// (`otp_raptor::RaptorQuery::service_date` にそのまま渡す)。当初の型定義には
    /// 無かったが、日付を跨ぐ運行判定なしに `plan()` は組めないため追加した。
    pub service_date: u32,
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

/// access/egress 徒歩探索で近傍駅を探す半径 (メートル)。OTP の `maxAccessEgressDuration`
/// に相当する打ち切り。1kmは徒歩12〜15分程度で、都心の駅間隔なら複数駅が候補に入る。
const ACCESS_EGRESS_RADIUS_M: f64 = 1000.0;

/// 半径内で見つかった近傍駅のうち、実際に `street.route` (A*) を試す上限数。
/// 直線距離が近い順に試す (`Timetable::nearby_stops` がソート済み)。上限を設けないと
/// 密集駅エリアで A* 呼び出し回数が線形に増え、レイテンシが悪化する。
const MAX_ACCESS_EGRESS_CANDIDATES: usize = 5;

/// RAPTOR のラウンド数上限 (=最大乗換回数+1)。都心区間なら3回もあれば十分。
const MAX_RAPTOR_ROUNDS: u8 = 4;

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

    /// 経路探索: 出発地→駅の徒歩 (access) → RAPTOR 乗換 → 駅→目的地の徒歩 (egress)。
    ///
    /// フロー:
    ///   1. `req.origin`/`req.destination` それぞれの半径 [`ACCESS_EGRESS_RADIUS_M`]
    ///      以内の駅を `Timetable::nearby_stops` で集め、近い順に最大
    ///      [`MAX_ACCESS_EGRESS_CANDIDATES`] 駅だけ `street.route` (A*, mobility に
    ///      応じた `WalkProfile`) で実際に歩行経路を引く。
    ///   2. 各駅への `WalkPath::physical_duration_s` (壁時計時間。探索用の一般化
    ///      コスト `duration_s` ではない) を `StreetLink::duration_s` として
    ///      `RaptorQuery` に渡し、RAPTOR 探索を実行する。
    ///   3. 返ってきた `Journey` 群を `Itinerary` (Leg::Walk / Leg::Transit) に
    ///      変換する。先頭/末尾の `JourneyLeg::Walk` は access/egress の
    ///      `WalkPath` (手順1で保持済み) から distance_m・has_stairs を補う。
    ///      RAPTOR 内部の乗換徒歩 (footpath, 近接駅間の直線距離近似) は
    ///      distance_m を持たないため 0.0 とする (既知の限界。`otp_raptor` の
    ///      モジュール doc 参照)。
    ///   4. 到着が早い順 (`total_duration_s` 昇順) にソートして返す。
    ///
    /// 運賃 (`fare_yen`) は今回のスライスでは常に `None` (次スライスで実装)。
    ///
    /// street グラフが未構築 (`self.street.nodes` が空) の場合や、半径内に
    /// access/egress どちらかの候補駅が1つも無い/実際に歩行経路が引けない場合は
    /// エラーにせず空 `Vec` を返す (「経路が見つからなかった」という正常系)。
    pub fn plan(&self, req: &RouteRequest) -> Result<Vec<Itinerary>> {
        if self.street.nodes.is_empty() {
            return Ok(Vec::new());
        }
        let profile = req.mobility.walk_profile();

        let access = self.access_links(req.origin, &profile);
        let egress = self.egress_links(req.destination, &profile);
        if access.links.is_empty() || egress.links.is_empty() {
            return Ok(Vec::new());
        }

        let query = otp_raptor::RaptorQuery {
            access: access.links,
            egress: egress.links,
            earliest_departure: req.depart_at,
            service_date: req.service_date,
            max_rounds: MAX_RAPTOR_ROUNDS,
        };

        let journeys = self.timetable.search(&query)?;

        let mut itineraries: Vec<Itinerary> = journeys
            .iter()
            .map(|j| self.journey_to_itinerary(j, req, &access.paths, &egress.paths))
            .collect();
        itineraries.sort_by_key(|it| it.total_duration_s);
        Ok(itineraries)
    }

    /// `origin` 近傍の駅への徒歩経路 (access) をまとめて引く。`WalkPath` を
    /// 駅ごとに保持しておき、`journey_to_itinerary` で distance_m・has_stairs の
    /// 復元に使う。
    fn access_links(&self, origin: LatLng, profile: &WalkProfile) -> WalkLinks {
        let candidates = self.timetable.nearby_stops(origin, ACCESS_EGRESS_RADIUS_M);
        self.collect_walk_links(candidates, |stop_coord| self.street.route(origin, stop_coord, profile))
    }

    /// `destination` 近傍の駅からの徒歩経路 (egress) をまとめて引く。
    fn egress_links(&self, destination: LatLng, profile: &WalkProfile) -> WalkLinks {
        let candidates = self.timetable.nearby_stops(destination, ACCESS_EGRESS_RADIUS_M);
        self.collect_walk_links(candidates, |stop_coord| self.street.route(stop_coord, destination, profile))
    }

    fn collect_walk_links(
        &self,
        candidates: Vec<(otp_raptor::StopIdx, LatLng)>,
        route_to_or_from: impl Fn(LatLng) -> otp_core::Result<otp_street::WalkPath>,
    ) -> WalkLinks {
        let mut links = Vec::new();
        let mut paths = HashMap::new();
        for (stop, stop_coord) in candidates.into_iter().take(MAX_ACCESS_EGRESS_CANDIDATES) {
            if let Ok(path) = route_to_or_from(stop_coord) {
                let duration_s = path.physical_duration_s.round() as u32;
                links.push(otp_raptor::StreetLink { stop, duration_s });
                paths.insert(stop, path);
            }
        }
        WalkLinks { links, paths }
    }

    /// RAPTOR の `Journey` を engine の `Itinerary` へ変換する。
    fn journey_to_itinerary(
        &self,
        journey: &otp_raptor::Journey,
        req: &RouteRequest,
        access_paths: &HashMap<otp_raptor::StopIdx, otp_street::WalkPath>,
        egress_paths: &HashMap<otp_raptor::StopIdx, otp_street::WalkPath>,
    ) -> Itinerary {
        let last_idx = journey.legs.len().saturating_sub(1);
        let legs: Vec<Leg> = journey
            .legs
            .iter()
            .enumerate()
            .map(|(i, leg)| match leg {
                otp_raptor::JourneyLeg::Walk { stop, duration_s } => {
                    let (distance_m, has_stairs) = if i == 0 {
                        access_paths.get(stop).map(|p| (p.distance_m, p.has_stairs)).unwrap_or((0.0, false))
                    } else if i == last_idx {
                        egress_paths.get(stop).map(|p| (p.distance_m, p.has_stairs)).unwrap_or((0.0, false))
                    } else {
                        // RAPTOR 内部の近接駅徒歩乗換 (footpath)。直線距離近似のため
                        // 距離は保持していない (otp_raptor モジュール doc 参照)。
                        (0.0, false)
                    };
                    Leg::Walk { distance_m, duration_s: *duration_s, has_stairs }
                }
                otp_raptor::JourneyLeg::Transit { route_short_name, from, to, board_s, alight_s, .. } => Leg::Transit {
                    route_name: route_short_name.clone(),
                    from_stop: self.timetable.stop_ids[*from as usize].as_str().to_string(),
                    to_stop: self.timetable.stop_ids[*to as usize].as_str().to_string(),
                    duration_s: (alight_s - board_s).max(0) as u32,
                },
            })
            .collect();

        Itinerary {
            legs,
            total_duration_s: (journey.arrival_s - req.depart_at).max(0) as u32,
            transfers: journey.transfers,
            fare_yen: None,
        }
    }
}

/// [`Engine::walk_links`] の戻り値: RAPTOR に渡す `StreetLink` 一覧と、
/// 変換時に distance_m/has_stairs を引くための駅ごとの `WalkPath`。
struct WalkLinks {
    links: Vec<otp_raptor::StreetLink>,
    paths: HashMap<otp_raptor::StopIdx, otp_street::WalkPath>,
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
