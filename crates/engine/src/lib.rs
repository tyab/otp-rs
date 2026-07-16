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
    /// arrive-by (到着時刻指定) フラグ。`true` のとき `depart_at` は「目的地への到着締切
    /// 時刻」を意味し、RAPTOR を後方探索 (最遅出発) で回す (`RaptorQuery::arrive_by`)。
    /// `false` なら従来どおり `depart_at` を出発時刻とする前方探索。
    pub arrive_by: bool,
}

/// 応答の1区間 (徒歩 or 乗車)。
///
/// babymobi の Route セグメント (駅名・座標・路線・地図折れ線) を組めるだけの情報を持つ。
/// `from`/`to` は「発地→着地」の名前と座標。徒歩の access/egress は `geometry` に街路の
/// 実経路 (折れ線) を持ち、乗換 (footpath) と乗車は 2点直線になる。
#[derive(Debug, Clone)]
pub enum Leg {
    Walk {
        from_name: String,
        from_coord: LatLng,
        to_name: String,
        to_coord: LatLng,
        distance_m: f32,
        duration_s: u32,
        has_stairs: bool,
        /// この徒歩区間がエレベーターを経由するか (アクセシビリティ明示用)。
        has_elevator: bool,
        /// 地図表示用の折れ線 (始点→終点)。access/egress は街路A*の実経路、
        /// 近接駅の footpath 乗換は 2点直線。
        geometry: Vec<LatLng>,
    },
    Transit {
        route_short_name: String,
        route_long_name: String,
        /// OTP 互換の mode 文字列 (SUBWAY/RAIL/TRAM/BUS)。
        mode: &'static str,
        /// GTFS agency_id (生値)。頻度ベース自前GTFS(BMC-FREQ)判定用。
        agency_id: String,
        from_name: String,
        from_coord: LatLng,
        to_name: String,
        to_coord: LatLng,
        duration_s: u32,
    },
}

/// 連続する `JourneyLeg::Transit` のうち「同一 route_id・同一停留所 (直前の to == 次の from)
/// かつ時間的に連続 (直前の降車時刻 == 次の乗車時刻 = 待ち0 = 同一車両の継続)」ものを
/// 1本の Transit にまとめる。RAPTOR がループ線の分岐等で同一路線を別パターンに分割して
/// 返す「見かけの乗換」を、本家 OTP と同様に同一路線の継続として1本化する
/// (from/board は先頭、to/alight は末尾)。
///
/// 待ち時間がある場合 (降車時刻 < 次の乗車時刻) は別便への実際の乗り継ぎ (再乗車) なので
/// **まとめない** — 実在する乗換/待ちを隠さないため (Codexレビュー指摘 P2)。異なる route_id
/// 同士や Walk を挟む乗換も当然まとめない (=正当な乗換として残す)。
fn merge_same_route_transit(legs: &[otp_raptor::JourneyLeg]) -> Vec<otp_raptor::JourneyLeg> {
    use otp_raptor::JourneyLeg;
    let mut out: Vec<JourneyLeg> = Vec::with_capacity(legs.len());
    for leg in legs {
        if let JourneyLeg::Transit { route_id, from, to, board_s, alight_s, .. } = leg {
            if let Some(JourneyLeg::Transit { route_id: p_route, to: p_to, alight_s: p_alight, .. }) = out.last_mut() {
                if *p_route == *route_id && *p_to == *from && *p_alight == *board_s {
                    // 待ち0の連続 → 直前 leg の終端を今の leg の終端まで伸ばす (中間駅は捨てる)。
                    *p_to = *to;
                    *p_alight = *alight_s;
                    continue;
                }
            }
        }
        out.push(leg.clone());
    }
    out
}

/// GTFS route_type → OTP 互換 mode 文字列。
fn otp_mode(rt: otp_gtfs::RouteType) -> &'static str {
    match rt {
        otp_gtfs::RouteType::Tram => "TRAM",
        otp_gtfs::RouteType::Subway => "SUBWAY",
        otp_gtfs::RouteType::Rail => "RAIL",
        otp_gtfs::RouteType::Bus => "BUS",
        otp_gtfs::RouteType::Other(_) => "RAIL",
    }
}

/// 2本の探索 (全モード / 鉄道限定) の結果をまとめる際の重複判定シグネチャ。
///
/// 「同一経路」を乗車 (`Leg::Transit`) leg の並び = 各 leg の
/// (route_short_name + route_long_name, 発駅名 → 着駅名) の連結で表す。徒歩 leg は
/// access/egress や footpath の些細な差で揺れるため署名に含めない (乗車の骨格が一致すれば
/// 同一経路とみなす)。最速経路が既に全鉄道なら鉄道限定探索が同じ乗車列を返すため
/// 署名が一致し、`plan` の dedupe で二重を防げる。
fn itinerary_signature(it: &Itinerary) -> String {
    let mut sig = String::new();
    for leg in &it.legs {
        if let Leg::Transit { route_short_name, route_long_name, from_name, to_name, .. } = leg {
            sig.push_str(route_short_name);
            sig.push('\u{1}');
            sig.push_str(route_long_name);
            sig.push('\u{1}');
            sig.push_str(from_name);
            sig.push_str("->");
            sig.push_str(to_name);
            sig.push('\u{1f}');
        }
    }
    sig
}

/// 応答の1経路。
#[derive(Debug, Clone)]
pub struct Itinerary {
    pub legs: Vec<Leg>,
    pub total_duration_s: u32,
    pub transfers: u8,
    /// 運賃 (円)。運賃データが無い区間を含む場合は None。
    pub fare_yen: Option<f64>,
    /// この経路が実際に出発地を発つ絶対時刻 (0時からの秒)。depart-at では `req.depart_at`
    /// と一致するが、arrive-by (`req.arrive_by`) では締切から逆算した **最遅出発時刻** に
    /// なる (経路ごとに異なりうる)。応答の start/end 時刻の anchor に使う (`total_duration_s`
    /// と合わせて到着 = `depart_s + total_duration_s`)。
    pub depart_s: SecondsSinceMidnight,
}

/// access/egress 徒歩探索で近傍駅を探す半径 (メートル)。OTP の `maxAccessEgressDuration`
/// に相当する打ち切り。1kmは徒歩12〜15分程度で、都心の駅間隔なら複数駅が候補に入る。
const ACCESS_EGRESS_RADIUS_M: f64 = 1000.0;

/// 半径内で見つかった近傍駅のうち、実際に `street.route` (A*) を試す上限数。
/// 直線距離が近い順に試す (`Timetable::nearby_stops` がソート済み)。この数だけ
/// 1.45M ノードの街路グラフに対し A* を走らせるため、レイテンシの支配的要因になる
/// (新宿/東京など高密度エリアでは 10 にすると access/egress で計 20 回 = 実測~9.5秒で
/// Worker タイムアウト境界に達した)。5 (元値) では方南町周辺で正解バス停が5番目=
/// ギリギリだったため、余裕を1つ足した 6 とする (乗換閾値400m/ラウンド5拡大で
/// 経路自体は改善済みのため、候補数は最小限で足りる)。半径1kmでも上限される。
const MAX_ACCESS_EGRESS_CANDIDATES: usize = 6;

/// RAPTOR のラウンド数上限 (=最大乗換回数+1)。ターミナル経由で乗換が増える郊外直通
/// (例: 方南支線→丸ノ内線→京王線→京王高尾線) を拾えるよう5に (最大乗換4回)。
const MAX_RAPTOR_ROUNDS: u8 = 5;

/// 応答に載せる経路数の上限。最速 (バスになりうる) + 鉄道の代替を両立させたうえで、
/// 密集エリアで候補が膨らんでも UI が扱いやすい控えめな本数に絞る。鉄道の代替は
/// この上限内でも最低1本は残す (`plan` の truncate 参照)。
const MAX_ITINERARIES: usize = 4;

/// エンジン本体。構築済みグラフ/時刻表/運賃モデルを保持し、リクエストに応答する。
///
/// これがネイティブサーバ (otp-server) の中身であり、将来 wasm32 で Worker に載せる対象。
///
/// `fares` はフィード (事業者) 単位で保持する: `otp_gtfs::Feed::load_from_dir_namespaced`
/// が付ける名前空間 prefix (例: 都営なら `"6"`, `crates/raptor/examples/plan.rs` の
/// モジュールdoc参照) をキーにした `HashMap`。運賃計算は「乗車 Leg が属すフィードの
/// `FareModel` を引いて計算する」設計 (`compute_fare` 参照) なので、呼び出し側は
/// `Timetable::build` に渡したのと同じフィード群から、同じ prefix で
/// `HashMap::from([(prefix, otp_fares::FareModel::from_gtfs(&feed)), ...])` を組んで渡す。
pub struct Engine {
    pub street: otp_street::StreetGraph,
    pub timetable: otp_raptor::Timetable,
    pub fares: HashMap<String, otp_fares::FareModel>,
}

impl Engine {
    pub fn new(
        street: otp_street::StreetGraph,
        timetable: otp_raptor::Timetable,
        fares: HashMap<String, otp_fares::FareModel>,
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
    /// 運賃 (`fare_yen`) は各 `Journey` から [`Engine::compute_fare`] で計算する
    /// (フィードごとの `FareModel` を突き合わせる。詳細は `compute_fare` のdoc参照)。
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

        // 2本の探索を「同一の access/egress リンク」で走らせる (時刻表は1つを共有:
        // 追加メモリなし)。単一基準 (最早到着) RAPTOR は停留所ごとに最早ラベルしか
        // 残さないため、鉄道経路がバス経路に上書きされて表に出ない (方南町→高尾山口 等で
        // 実測)。鉄道限定の第2探索を別途走らせ、バスに勝てなくても鉄道の代替を拾う。
        //   q_fast … 全モード。最速 (バスになりうる)。
        //   q_rail … 鉄道限定。鉄道のみの代替経路。
        let q_fast = otp_raptor::RaptorQuery {
            access: access.links.clone(),
            egress: egress.links.clone(),
            earliest_departure: req.depart_at,
            service_date: req.service_date,
            max_rounds: MAX_RAPTOR_ROUNDS,
            rail_only: false,
            arrive_by: req.arrive_by,
        };
        let q_rail = otp_raptor::RaptorQuery {
            access: access.links,
            egress: egress.links,
            earliest_departure: req.depart_at,
            service_date: req.service_date,
            max_rounds: MAX_RAPTOR_ROUNDS,
            rail_only: true,
            arrive_by: req.arrive_by,
        };

        let fast_journeys = self.timetable.search(&q_fast)?;

        // fast を先に Itinerary 化し、乗車 leg の並び (路線名 + 発着駅名) を
        // シグネチャにして重複排除する (fast 内の Pareto 重複も畳む)。
        let mut fast_its: Vec<Itinerary> = Vec::new();
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        for j in &fast_journeys {
            let it = self.journey_to_itinerary(j, req, &access.paths, &egress.paths);
            if seen.insert(itinerary_signature(&it)) {
                fast_its.push(it);
            }
        }

        // 鉄道限定の第2探索は「最速経路がバスを含む」ときだけ走らせる。最速が既に鉄道なら
        // それが答えで、遅い鉄道代替を別途出す必要はない (Pareto集合に遅いバスが混じっても
        // 最速が鉄道なら第2探索は無駄)。最速 itinerary だけで判定することで、全鉄道が最速の
        // 高密度OD (新宿→東京 等) を単一探索に保ち、2回分の遅延を避ける。
        let fastest_has_bus = fast_its
            .iter()
            .min_by_key(|it| it.total_duration_s)
            .is_some_and(|it| it.legs.iter().any(|l| matches!(l, Leg::Transit { mode, .. } if *mode == "BUS")));

        let mut rail_its: Vec<Itinerary> = Vec::new();
        if fastest_has_bus {
            let rail_journeys = self.timetable.search(&q_rail)?;
            // 鉄道限定の結果のうち、fast に既出でない経路だけを「鉄道の代替」として拾う。
            // 鉄道が到達不能なら rail_journeys は空で代替は増えない (fast だけを返す)。
            for j in &rail_journeys {
                let it = self.journey_to_itinerary(j, req, &access.paths, &egress.paths);
                if seen.insert(itinerary_signature(&it)) {
                    rail_its.push(it);
                }
            }
        }

        // 到着が早い順に並べ、表示上限 MAX_ITINERARIES に絞る。ただし鉄道の代替は
        // バスより遅くても最低1本は残す (単純 truncate では fast が MAX_ITINERARIES 本
        // Pareto で埋めると鉄道が押し出されうるため、明示的に枠を確保する)。
        fast_its.sort_by_key(|it| it.total_duration_s);
        rail_its.sort_by_key(|it| it.total_duration_s);
        let mut itineraries = fast_its;
        itineraries.extend(rail_its.iter().cloned());
        itineraries.sort_by_key(|it| it.total_duration_s);
        if itineraries.len() > MAX_ITINERARIES {
            itineraries.truncate(MAX_ITINERARIES);
            // 上限内に鉄道の代替が1本も残らなかったら、末尾を最速の鉄道代替で差し替える。
            if let Some(best_rail) = rail_its.first() {
                let rail_sig = itinerary_signature(best_rail);
                let has_rail = itineraries.iter().any(|it| itinerary_signature(it) == rail_sig);
                if !has_rail {
                    let last = itineraries.len() - 1;
                    itineraries[last] = best_rail.clone();
                    itineraries.sort_by_key(|it| it.total_duration_s);
                }
            }
        }
        Ok(itineraries)
    }

    /// `origin` 近傍の駅への徒歩経路 (access) をまとめて引く。`WalkPath` を
    /// 駅ごとに保持しておき、`journey_to_itinerary` で distance_m・has_stairs の
    /// 復元に使う。
    ///
    /// 候補駅 (最大 [`MAX_ACCESS_EGRESS_CANDIDATES`]) それぞれに個別 A* を走らせる
    /// 代わりに、`route_one_to_many` で **1回の Dijkstra** にまとめる (結果は各駅
    /// への `route()` とフィールド単位で一致)。
    fn access_links(&self, origin: LatLng, profile: &WalkProfile) -> WalkLinks {
        let candidates: Vec<(otp_raptor::StopIdx, LatLng)> = self
            .timetable
            .nearby_stops(origin, ACCESS_EGRESS_RADIUS_M)
            .into_iter()
            .take(MAX_ACCESS_EGRESS_CANDIDATES)
            .collect();
        let coords: Vec<LatLng> = candidates.iter().map(|(_, c)| *c).collect();
        let paths = self.street.route_one_to_many(origin, &coords, profile);
        Self::build_walk_links(candidates, paths)
    }

    /// `destination` 近傍の駅からの徒歩経路 (egress) をまとめて引く。
    ///
    /// 無向グラフなので、目的地からの **1回の Dijkstra** (`route_many_to_one`) で
    /// 全候補駅→目的地の経路を得る (各駅への `route(stop, destination)` と一致)。
    fn egress_links(&self, destination: LatLng, profile: &WalkProfile) -> WalkLinks {
        let candidates: Vec<(otp_raptor::StopIdx, LatLng)> = self
            .timetable
            .nearby_stops(destination, ACCESS_EGRESS_RADIUS_M)
            .into_iter()
            .take(MAX_ACCESS_EGRESS_CANDIDATES)
            .collect();
        let coords: Vec<LatLng> = candidates.iter().map(|(_, c)| *c).collect();
        let paths = self.street.route_many_to_one(&coords, destination, profile);
        Self::build_walk_links(candidates, paths)
    }

    /// 候補駅と (それに位置対応する) `WalkPath` 群から `StreetLink` 一覧と
    /// 駅ごとの `WalkPath` マップを組む。経路が引けなかった (`None`) 駅は捨てる
    /// (旧 `collect_walk_links` が `route()` の Err を捨てていたのと同じ挙動)。
    fn build_walk_links(
        candidates: Vec<(otp_raptor::StopIdx, LatLng)>,
        paths: Vec<Option<otp_street::WalkPath>>,
    ) -> WalkLinks {
        let mut links = Vec::new();
        let mut path_map = HashMap::new();
        for ((stop, _), path) in candidates.into_iter().zip(paths) {
            if let Some(path) = path {
                let duration_s = path.physical_duration_s.round() as u32;
                links.push(otp_raptor::StreetLink { stop, duration_s });
                path_map.insert(stop, path);
            }
        }
        WalkLinks { links, paths: path_map }
    }

    /// RAPTOR の `Journey` を engine の `Itinerary` へ変換する。
    fn journey_to_itinerary(
        &self,
        journey: &otp_raptor::Journey,
        req: &RouteRequest,
        access_paths: &HashMap<otp_raptor::StopIdx, otp_street::WalkPath>,
        egress_paths: &HashMap<otp_raptor::StopIdx, otp_street::WalkPath>,
    ) -> Itinerary {
        // 同一路線・同一停留所での連続 Transit を1本にまとめてから変換する。RAPTOR は
        // ループ線の分岐 (例: 都営大江戸線 都庁前) で同一路線を別パターンとして分割し、
        // Walk を挟まない連続 Transit leg = 「見かけの乗換」を返すことがある。本家 OTP は
        // 同一路線内の継続を乗換に数えないため、ここでまとめて表示 leg と乗換数を揃える。
        let merged = merge_same_route_transit(&journey.legs);
        let last_idx = merged.len().saturating_sub(1);
        // 発地→着地を順に辿るカーソル (直前 leg の到達地点)。徒歩 leg の from/to 名前・
        // 座標を、その両端 (出発地/目的地 or 駅) から解決するために使う。
        let mut cursor_name = "出発地".to_string();
        let mut cursor_coord = req.origin;

        let mut legs: Vec<Leg> = Vec::with_capacity(merged.len());
        for (i, leg) in merged.iter().enumerate() {
            match leg {
                otp_raptor::JourneyLeg::Walk { stop, duration_s } => {
                    let stop_name = self.timetable.stop_name(*stop).to_string();
                    let stop_coord = self.timetable.stop_coord(*stop);
                    let (from_name, from_coord, to_name, to_coord, distance_m, has_stairs, has_elevator, geometry) = if i == 0 {
                        // access: 出発地 → 乗車駅。街路A*の実経路をジオメトリに。
                        let p = access_paths.get(stop);
                        let geom = p.map(|p| self.street.path_coords(p)).unwrap_or_default();
                        let (d, s, e) = p.map(|p| (p.distance_m, p.has_stairs, p.has_elevator)).unwrap_or((0.0, false, false));
                        (cursor_name.clone(), cursor_coord, stop_name, stop_coord, d, s, e, geom)
                    } else if i == last_idx {
                        // egress: 降車駅 → 目的地。
                        let p = egress_paths.get(stop);
                        let geom = p.map(|p| self.street.path_coords(p)).unwrap_or_default();
                        let (d, s, e) = p.map(|p| (p.distance_m, p.has_stairs, p.has_elevator)).unwrap_or((0.0, false, false));
                        (cursor_name.clone(), cursor_coord, "目的地".to_string(), req.destination, d, s, e, geom)
                    } else {
                        // RAPTOR 内部の近接駅徒歩乗換 (footpath)。直線距離近似のため距離・段差は
                        // 保持せず、ジオメトリは 2点直線 (otp_raptor モジュール doc 参照)。
                        (cursor_name.clone(), cursor_coord, stop_name.clone(), stop_coord, 0.0, false, false, vec![cursor_coord, stop_coord])
                    };
                    // access 以外はジオメトリが空になりうる (footpath は上で2点直線を入れた
                    // ので空にならないが、街路経路が取れなかった端点用のフォールバック)。
                    let geometry = if geometry.len() >= 2 { geometry } else { vec![from_coord, to_coord] };
                    cursor_name = to_name.clone();
                    cursor_coord = to_coord;
                    legs.push(Leg::Walk { from_name, from_coord, to_name, to_coord, distance_m, duration_s: *duration_s, has_stairs, has_elevator, geometry });
                }
                otp_raptor::JourneyLeg::Transit { route_short_name, route_long_name, route_type, agency_id, from, to, board_s, alight_s, .. } => {
                    let from_name = self.timetable.stop_name(*from).to_string();
                    let from_coord = self.timetable.stop_coord(*from);
                    let to_name = self.timetable.stop_name(*to).to_string();
                    let to_coord = self.timetable.stop_coord(*to);
                    cursor_name = to_name.clone();
                    cursor_coord = to_coord;
                    legs.push(Leg::Transit {
                        route_short_name: route_short_name.clone(),
                        route_long_name: route_long_name.clone(),
                        mode: otp_mode(*route_type),
                        agency_id: agency_id.clone(),
                        from_name,
                        from_coord,
                        to_name,
                        to_coord,
                        duration_s: (alight_s - board_s).max(0) as u32,
                    });
                }
            }
        }

        // 乗換数はまとめ後の Transit 本数 - 1 (アクセス/イグレス徒歩や同一路線内継続は数えない)。
        let transit_count = merged.iter().filter(|l| matches!(l, otp_raptor::JourneyLeg::Transit { .. })).count();
        // 出発地を発つ絶対時刻。depart-at では `req.depart_at` (=出発時刻) そのまま (従来動作を
        // バイト単位で保つ)。arrive-by では締切から逆算した最遅出発時刻を経路から復元する
        // (先頭 Transit の board − それより前の徒歩 = access 徒歩の合計)。
        let depart_s = if req.arrive_by { journey_departure_s(journey) } else { req.depart_at };
        Itinerary {
            legs,
            total_duration_s: (journey.arrival_s - depart_s).max(0) as u32,
            transfers: transit_count.saturating_sub(1) as u8,
            fare_yen: self.compute_fare(journey),
            depart_s,
        }
    }

    /// 経路全体の運賃 (円) を計算する。
    ///
    /// `journey.legs` を先頭から走査し、連続する `JourneyLeg::Transit` を「フィード
    /// (route_id の名前空間 prefix, [`feed_prefix`]) が同じ」限り1グループにまとめる
    /// (`Leg::Walk` を挟むか、フィードが変わったところでグループを区切る)。これは
    /// 本家 OTP の実測 (`otp_fares` モジュールdoc「都営単一事業者・乗換1回」) で、
    /// 同一事業者内の乗換を挟む2 leg に同一の1個の運賃product が紐付くことに対応する
    /// (同一駅乗換は `otp_raptor::Timetable::search` が明示的な `Walk` leg を挟まずに
    /// 連続する `Transit` leg として返すため、この単純な「Walk無しなら同一グループ」
    /// 判定で実データの事例をカバーできる。近接駅を歩いて跨ぐ同一事業者内乗換は
    /// 別グループ扱いになり別途課金される近似だが、実データではこのケースは
    /// 都営↔メトロ等の事業者跨ぎでしか観測していない)。
    ///
    /// 各グループを、そのフィードの `FareModel::total_fare` に渡して運賃を求め、
    /// グループ間 (=事業者間) は単純合算する (本家 OTP 実測「事業者跨ぎ」参照)。
    ///
    /// いずれかのグループで運賃が求まらない場合 (対応する `FareModel` が `self.fares` に
    /// 無い = zone_id を持たないフィード、例: 自前頻度 GTFS の JR。または zone_id は
    /// あるが該当する `fare_rules` が無い) は、判明分だけの部分合計を返さず全体を
    /// `None` にする (実際より安い金額を誤って提示しないため)。
    fn compute_fare(&self, journey: &otp_raptor::Journey) -> Option<f64> {
        let legs = &journey.legs;
        let mut total = 0.0;
        let mut i = 0;
        while i < legs.len() {
            let otp_raptor::JourneyLeg::Transit { route_id, .. } = &legs[i] else {
                i += 1;
                continue;
            };
            let prefix = feed_prefix(route_id.as_str()).to_string();

            let mut group: Vec<otp_fares::FareLeg> = Vec::new();
            while let Some(otp_raptor::JourneyLeg::Transit { route_id: rid, from, to, .. }) = legs.get(i) {
                if feed_prefix(rid.as_str()) != prefix {
                    break;
                }
                group.push(otp_fares::FareLeg {
                    route_id: Some(rid.clone()),
                    origin_zone: self.timetable.stop_zone(*from).map(str::to_string),
                    destination_zone: self.timetable.stop_zone(*to).map(str::to_string),
                    contains_zones: Vec::new(),
                });
                i += 1;
            }

            let model = self.fares.get(&prefix)?;
            let fare = model.total_fare(&group).ok()?;
            total += fare.amount;
        }
        Some(total)
    }
}

/// 経路が実際に出発地を発つ絶対時刻 (0時からの秒) を `Journey` から復元する。
/// 先頭 Transit の board 時刻から、それより前に現れる徒歩 leg (access 徒歩) の所要合計を
/// 差し引く (出発地を出て access 徒歩を経て最初の便に乗る = board − access 徒歩)。arrive-by の
/// 最遅出発時刻の算出に使う。Transit を含まない経路 (通常 RAPTOR では発生しない) は
/// 到着時刻をそのまま返す (所要0扱いのフォールバック)。
fn journey_departure_s(journey: &otp_raptor::Journey) -> SecondsSinceMidnight {
    let mut walk_before: i32 = 0;
    for leg in &journey.legs {
        match leg {
            otp_raptor::JourneyLeg::Walk { duration_s, .. } => walk_before += *duration_s as i32,
            otp_raptor::JourneyLeg::Transit { board_s, .. } => return board_s - walk_before,
        }
    }
    journey.arrival_s
}

/// 名前空間化済み ID (`"<feed_prefix>:<raw_id>"`, `otp_gtfs::Feed::namespace` 参照) から
/// フィード prefix を取り出す。namespace 化されていない ID (`:` を含まない。単一フィード
/// 構成のテスト等) は ID 全体をそのまま prefix として扱う (フォールバック)。
fn feed_prefix(id: &str) -> &str {
    id.split_once(':').map(|(prefix, _)| prefix).unwrap_or(id)
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

    #[test]
    fn merge_collapses_same_route_same_stop_but_keeps_real_transfers() {
        use otp_core::{RouteId, TripId};
        use otp_raptor::JourneyLeg;
        let transit = |route: &str, from: u32, to: u32, board: i32, alight: i32| JourneyLeg::Transit {
            route_id: RouteId::new(route),
            route_short_name: route.into(),
            route_long_name: route.into(),
            route_type: otp_gtfs::RouteType::Subway,
            agency_id: "toei".into(),
            trip_id: TripId::new("t"),
            from,
            to,
            board_s: board,
            alight_s: alight,
        };
        // (A) 同一路線(大江戸線)を都庁前(stop5)で分割した連続 Transit → 1本に統合。
        let split = vec![transit("Oedo", 1, 5, 28800, 28860), transit("Oedo", 5, 9, 28860, 29760)];
        let m = merge_same_route_transit(&split);
        assert_eq!(m.len(), 1, "同一路線・同一停留所の連続は1本に");
        match &m[0] {
            JourneyLeg::Transit { from, to, board_s, alight_s, .. } => {
                assert_eq!((*from, *to, *board_s, *alight_s), (1, 9, 28800, 29760), "先頭from/board〜末尾to/alightに伸びる");
            }
            _ => panic!("Transitのはず"),
        }
        // (B) 別路線の同一駅乗換 (大江戸線→丸ノ内線) は正当な乗換として残す。
        let xfer = vec![transit("Oedo", 1, 5, 28800, 28860), transit("Marunouchi", 5, 9, 28900, 29200)];
        assert_eq!(merge_same_route_transit(&xfer).len(), 2, "別路線は統合しない");
        // (C) 同一路線でも Walk (別駅への徒歩乗換) を挟むなら残す。
        let walked = vec![
            transit("Oedo", 1, 5, 28800, 28860),
            JourneyLeg::Walk { stop: 6, duration_s: 120 },
            transit("Oedo", 6, 9, 29000, 29500),
        ];
        let transit_n = merge_same_route_transit(&walked).iter().filter(|l| matches!(l, JourneyLeg::Transit { .. })).count();
        assert_eq!(transit_n, 2, "Walkを挟む場合は統合しない");
        // (D) 同一路線・同一停留所でも待ち時間がある (降車28860 < 次の乗車29000) なら別便への
        //     再乗車 = 実際の乗換なので統合しない (実乗換を隠さない)。
        let reboard = vec![transit("Oedo", 1, 5, 28800, 28860), transit("Oedo", 5, 9, 29000, 29760)];
        assert_eq!(merge_same_route_transit(&reboard).len(), 2, "待ちがある再乗車は統合しない");
    }
}
