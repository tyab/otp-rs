//! RAPTOR による時刻表ベースのマルチモーダル乗換探索。
//!
//! OTP の `raptor` パッケージ相当。`otp-gtfs::Feed` 群からコンパクトな時刻表
//! (`Timetable`) を構築し、ラウンドごとに到達可能停留所を更新する標準 RAPTOR を回す。
//!
//! ## このスライスのスコープ
//! - **鉄道のみ** (`RouteType::is_rail()` = Tram/Subway/Rail)。バスは除外し時刻表を小さく保つ。
//! - **複数フィード対応**: `Timetable::build` は複数 `Feed` をマージするが、ID の
//!   一意性は呼び出し側の責任 (`otp_gtfs::Feed::load_from_dir_namespaced` で
//!   事前に名前空間化しておくこと)。実測: 都営/メトロ/京王/東武/りんかい GTFS は
//!   いずれも route_id/service_id に "0"〜"4" 等の小さい整数を事業者間で使い回して
//!   おり、素朴なマージは別事業者のカレンダー/路線を上書きして壊れる。
//! - **乗換モデル**: transfers.txt はどのフィードにも存在しない (実測)。代わりに
//!   (a) 同一正規化停留所内 (`parent_station` 集約後の同一 StopIdx) の乗換には
//!   既定バッファ `DEFAULT_TRANSFER_BUFFER_S` を課す、(b) 異なる停留所間で直線距離が
//!   `MAX_WALK_TRANSFER_M` 以内なら徒歩時間 (`WALK_SPEED_MPS` 基準) + バッファの
//!   乗換エッジを張る (事業者をまたいでも可、例: メトロ↔都営 白金高輪)。
//!   直線距離ベースの近似のため、地下通路のように迂回が大きい実際の徒歩経路とは
//!   厳密には一致しない (`scripts/compare_otp.sh` の実測差分を参照)。
//! - access/egress は駅指定 (`StreetLink`) で与える。徒歩ルーティングは otp-street 側の役割。

use std::collections::HashMap;

use otp_core::{LatLng, Result, RouteId, SecondsSinceMidnight, StopId, TripId};
use otp_gtfs::Feed;

/// 乗換に要する既定バッファ (秒)。transfers.txt が無いフィード (実測: 都営/メトロ/
/// りんかい/京王/東武いずれも `transfers.txt` を含まない) で、同一駅内乗換・近接駅
/// 徒歩乗換の両方に加える固定の乗換所要時間。OTP は実際の駅構内/街路ネットワークの
/// 徒歩時間を使うため厳密には一致しないが、「0分乗換」よりは実態に近い下限値として使う。
const DEFAULT_TRANSFER_BUFFER_S: u32 = 120;

/// 近接駅の徒歩乗換を許容する最大直線距離 (メートル)。これを超える駅間は接続しない。
/// 150mでは新宿のような巨大ターミナルの異事業者間乗換 (メトロ新宿↔京王/JR新宿は
/// 実測324m離れている) が接続されず、「方南町→中野坂上→新宿→京王→高尾山口」等の
/// 自然な鉄道経路がグラフ上に存在しなくなる。ターミナルの乗換動線を拾えるよう400mに拡大
/// (徒歩時間はエッジに反映されるためRAPTORが所要増を織り込んで最適化する。実測との
/// 突き合わせは `scripts/compare_otp.sh` を参照)。
const MAX_WALK_TRANSFER_M: f64 = 400.0;

/// 徒歩速度 (m/s)。実測 (OTP2 公式ドキュメント, 2026-07-15 確認):
/// `RouteRequest` の `walkSpeed` 既定値は 1.35 m/s、`RouterConfiguration` の
/// `routingDefaults` サンプルでは 1.3 m/s。ここでは直線距離しか使えないこのスライスの
/// 近似 (実際の徒歩経路は地下通路・街路網の迂回でより長くなりがち) を織り込み、
/// やや保守的な 1.3 m/s を採用する。
const WALK_SPEED_MPS: f64 = 1.3;

/// 停留所のコンパクト添字。
pub type StopIdx = u32;
/// パターン (同一停車列を共有する便の集合) のコンパクト添字。
pub type PatternIdx = u32;

/// 1便 (trip) の、パターン内での到着/出発時刻列。
/// `arrivals[i]` / `departures[i]` は `Pattern::stops[i]` に対応する。
#[derive(Debug, Clone)]
pub struct PatternTrip {
    pub trip_id: TripId,
    pub service_id: otp_core::ServiceId,
    pub arrivals: Vec<SecondsSinceMidnight>,
    pub departures: Vec<SecondsSinceMidnight>,
}

/// 同一停車パターン (停留所の並びが完全一致する便の集合)。
/// `trips` は `departures[0]` (始発停留所の出発時刻) 昇順にソート済み。
#[derive(Debug, Clone)]
pub struct Pattern {
    pub route_id: RouteId,
    pub route_short_name: String,
    /// GTFS `route_long_name`。babymobi 側の路線ID突合 (railwayIdFromOtpRouteName) は
    /// long_name を鍵にするため、short とは別に保持する。空なら short_name を流用。
    pub route_long_name: String,
    /// 交通モード (mode 文字列 SUBWAY/RAIL/TRAM/BUS の生成に使う)。
    pub route_type: otp_gtfs::RouteType,
    /// GTFS `agency_id` (生値, 名前空間化しない)。頻度ベース自前GTFS(BMC-FREQ)判定に使う。
    pub agency_id: String,
    pub stops: Vec<StopIdx>,
    pub trips: Vec<PatternTrip>,
}

/// 運行日カレンダー (`calendar.txt` + `calendar_dates.txt` を1サービスIDに統合したもの)。
#[derive(Debug, Clone, Default)]
struct ServiceCalendar {
    /// `calendar.txt` に無いサービス (calendar_dates のみで定義) なら None。
    weekdays: Option<[bool; 7]>,
    start_date: u32,
    end_date: u32,
    added_dates: Vec<u32>,
    removed_dates: Vec<u32>,
}

/// 乗換エッジ1本 (RAPTOR のラウンド間で使う「footpath」)。同一駅内の乗換バッファ
/// (自己ループ, `to_stop == 自身`) と、近接駅への徒歩乗換の両方をこの型で表す。
#[derive(Debug, Clone, Copy)]
struct Transfer {
    to_stop: StopIdx,
    duration_s: u32,
}

/// RAPTOR 用に詰めた時刻表。
#[derive(Debug, Default)]
pub struct Timetable {
    /// StopIdx → 正規化された StopId (`parent_station` があればそれ、無ければ自身)。
    pub stop_ids: Vec<StopId>,
    /// StopIdx → 停留所名 (`Stop::name`)。babymobi の Route セグメント表示 (駅名) 用。
    /// 同一正規化停留所に複数の生 stop が属す場合は最初に見つかった非空名を採用。
    stop_names: Vec<String>,
    /// 生の stop_id (プラットフォーム含む) と正規化後 StopId の両方から引ける索引。
    stop_lookup: HashMap<StopId, StopIdx>,
    pub patterns: Vec<Pattern>,
    /// StopIdx → そのパターン内での出現位置一覧。
    stop_patterns: Vec<Vec<(PatternIdx, u32)>>,
    /// StopIdx → 乗換エッジ一覧 (自己バッファ + 近接駅徒歩乗換)。
    transfers: Vec<Vec<Transfer>>,
    calendars: HashMap<otp_core::ServiceId, ServiceCalendar>,
    /// StopIdx → 正規化停留所の座標 (同一正規化停留所に属す生 stop の平均)。
    /// otp-engine が「座標 → 近傍駅」を引く (`nearby_stops`) ためにここで保持する。
    stop_coords: Vec<LatLng>,
    /// StopIdx → 運賃ゾーン (`Stop::zone_id`, 既に名前空間化済み)。otp-engine が
    /// `JourneyLeg::Transit` の from/to から `otp_fares::FareLeg` を組むために使う
    /// (`Timetable::stop_zone`)。同一正規化停留所 (`parent_station` 集約後) に複数の
    /// 生 stop が属す場合は、最初に見つかった非空の zone_id を採用する (実データは
    /// どのフィードも parent_station が空でこのケースはほぼ発生しない。座標平均と違い
    /// 「複数ゾーンの平均」は意味を持たないため単純な early-win にした)。
    stop_zones: Vec<Option<String>>,
}

/// 時刻表に含める交通モードの絞り込み。
///
/// バスは停留所数・便数が鉄道より桁違いに多く (都営バスだけで停留所5000超・
/// stop_times 130万行)、鉄道のみの用途ではメモリ・起動時間の無駄になるため
/// 既定は [`ModeFilter::RailOnly`]。ベビーカー主軸の BabyMobi ではノンステップ
/// バスが深い地下ホームの代替になりうるため [`ModeFilter::RailAndBus`] を選べる。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModeFilter {
    /// Tram/Subway/Rail のみ (`RouteType::is_rail`)。
    RailOnly,
    /// 鉄道 + Bus。
    RailAndBus,
}

impl ModeFilter {
    fn accepts(self, rt: otp_gtfs::RouteType) -> bool {
        match self {
            ModeFilter::RailOnly => rt.is_rail(),
            ModeFilter::RailAndBus => rt.is_rail() || rt == otp_gtfs::RouteType::Bus,
        }
    }
}

impl Timetable {
    /// `otp-gtfs::Feed` 群から鉄道のみの時刻表を構築する ([`ModeFilter::RailOnly`])。
    ///
    /// 複数フィードをまたぐ場合は、呼び出し側が事前に
    /// `Feed::load_from_dir_namespaced` 等で stop_id/route_id/trip_id/service_id を
    /// フィード単位で一意にしておくこと (このメソッド自体はマージするだけで、
    /// ID の一意性検証は行わない)。
    pub fn build(feeds: &[Feed]) -> Result<Timetable> {
        Self::build_with_modes(feeds, ModeFilter::RailOnly)
    }

    /// [`Timetable::build`] と同じだが、含める交通モードを [`ModeFilter`] で指定する。
    pub fn build_with_modes(feeds: &[Feed], modes: ModeFilter) -> Result<Timetable> {
        // 1. 停留所の正規化 (parent_station への集約) と StopIdx 割り当て。
        //    正規化後の StopId ごとに座標 (緯度経度の平均) も集計する
        //    (近接駅の徒歩乗換判定に使う。実データはどのフィードも parent_station が
        //    空なので通常は1駅1点の平均、フィクスチャのようにプラットフォームが
        //    parent_station で束ねられる場合のみ複数点の平均になる)。
        let mut stop_ids: Vec<StopId> = Vec::new();
        let mut stop_lookup: HashMap<StopId, StopIdx> = HashMap::new();
        let mut canonical_of: HashMap<StopId, StopId> = HashMap::new();
        let mut coord_acc: HashMap<StopId, (f64, f64, u32)> = HashMap::new();
        let mut zone_of: HashMap<StopId, String> = HashMap::new();
        let mut name_of: HashMap<StopId, String> = HashMap::new();

        for feed in feeds {
            for stop in &feed.stops {
                let canonical = stop.parent_station.clone().unwrap_or_else(|| stop.id.clone());
                canonical_of.insert(stop.id.clone(), canonical.clone());
                if !stop_lookup.contains_key(&canonical) {
                    let idx = stop_ids.len() as StopIdx;
                    stop_ids.push(canonical.clone());
                    stop_lookup.insert(canonical.clone(), idx);
                }
                let idx = stop_lookup[&canonical];
                stop_lookup.entry(stop.id.clone()).or_insert(idx);

                let acc = coord_acc.entry(canonical.clone()).or_insert((0.0, 0.0, 0));
                acc.0 += stop.lat;
                acc.1 += stop.lng;
                acc.2 += 1;

                // 最初に見つかった非空の zone_id / 名前を採用 (struct doc 参照)。
                if let Some(z) = &stop.zone_id {
                    zone_of.entry(canonical.clone()).or_insert_with(|| z.clone());
                }
                // 名前は「その正規化停留所自身 (親駅 or 単独駅)」の名前を優先し、
                // 無ければプラットフォーム名で埋める (例: parent_station C の名前 "C駅" を
                // プラットフォーム "C駅1番線" より優先。実データは parent_station が空なので
                // 常に自身の名前)。
                if !stop.name.is_empty() {
                    if stop.id == canonical {
                        name_of.insert(canonical.clone(), stop.name.clone());
                    } else {
                        name_of.entry(canonical.clone()).or_insert_with(|| stop.name.clone());
                    }
                }
            }
        }

        // 2. 対象モードの路線のみ抽出 (route_id → route の索引をフィードごとに作る)。
        let mut included_routes: HashMap<RouteId, &otp_gtfs::Route> = HashMap::new();
        for feed in feeds {
            for route in &feed.routes {
                if modes.accepts(route.route_type) {
                    included_routes.insert(route.id.clone(), route);
                }
            }
        }

        // 3. trip_id → route の索引、service_id の索引。
        let mut trip_route: HashMap<TripId, &otp_gtfs::Trip> = HashMap::new();
        for feed in feeds {
            for trip in &feed.trips {
                if included_routes.contains_key(&trip.route_id) {
                    trip_route.insert(trip.id.clone(), trip);
                }
            }
        }

        // 4. trip_id ごとに stop_times を stop_sequence 順に集める。
        let mut stop_times_by_trip: HashMap<TripId, Vec<&otp_gtfs::StopTime>> = HashMap::new();
        for feed in feeds {
            for st in &feed.stop_times {
                if trip_route.contains_key(&st.trip_id) {
                    stop_times_by_trip.entry(st.trip_id.clone()).or_default().push(st);
                }
            }
        }

        // 4.5. frequencies.txt 索引: trip_id → その頻度運行ウィンドウ群 (対象モードの便のみ)。
        let mut freqs_by_trip: HashMap<TripId, Vec<&otp_gtfs::Frequency>> = HashMap::new();
        for feed in feeds {
            for f in &feed.frequencies {
                if trip_route.contains_key(&f.trip_id) {
                    freqs_by_trip.entry(f.trip_id.clone()).or_default().push(f);
                }
            }
        }

        // 5. パターン化: (route_id, 正規化停留所列) が同じ trip をグルーピング。
        let mut pattern_index: HashMap<(RouteId, Vec<StopIdx>), PatternIdx> = HashMap::new();
        let mut patterns: Vec<Pattern> = Vec::new();

        for (trip_id, mut sts) in stop_times_by_trip {
            sts.sort_by_key(|st| st.stop_sequence);
            if sts.len() < 2 {
                continue; // 1停留所以下の便はRAPTOR上意味がないので除外
            }
            let trip = trip_route[&trip_id];
            let stops: Vec<StopIdx> = sts
                .iter()
                .map(|st| {
                    let canonical = canonical_of.get(&st.stop_id).cloned().unwrap_or_else(|| st.stop_id.clone());
                    *stop_lookup.entry(canonical).or_insert_with(|| unreachable!("stop should be pre-registered"))
                })
                .collect();
            let arrivals: Vec<SecondsSinceMidnight> = sts.iter().map(|st| st.arrival).collect();
            let departures: Vec<SecondsSinceMidnight> = sts.iter().map(|st| st.departure).collect();

            let key = (trip.route_id.clone(), stops.clone());
            let pattern_idx = *pattern_index.entry(key).or_insert_with(|| {
                let route = included_routes[&trip.route_id];
                patterns.push(Pattern {
                    route_id: trip.route_id.clone(),
                    route_short_name: if route.short_name.is_empty() { route.long_name.clone() } else { route.short_name.clone() },
                    route_long_name: if route.long_name.is_empty() { route.short_name.clone() } else { route.long_name.clone() },
                    route_type: route.route_type,
                    agency_id: route.agency_id.as_ref().map(|a| a.as_str().to_string()).unwrap_or_default(),
                    stops,
                    trips: Vec::new(),
                });
                (patterns.len() - 1) as PatternIdx
            });
            // frequencies.txt があれば雛形 trip を各ウィンドウ内で headway 間隔に展開する
            // (頻度ベースダイヤ = 自前頻度 GTFS の JR/私鉄/京王/東武)。無ければ確定時刻の
            // 便としてそのまま1本積む。展開時刻は「先頭発を基準にした相対 stop_times」を
            // 各発車時刻ぶんシフトする。
            match freqs_by_trip.get(&trip_id) {
                Some(windows) => {
                    let base_start = departures[0];
                    for f in windows {
                        if f.headway_secs == 0 {
                            continue; // 0除算/無限ループ防止 (不正データは黙って飛ばす)
                        }
                        let mut dep = f.start_time;
                        while dep < f.end_time {
                            let offset = dep - base_start;
                            patterns[pattern_idx as usize].trips.push(PatternTrip {
                                trip_id: TripId::new(format!("{}@{dep}", trip_id.as_str())),
                                service_id: trip.service_id.clone(),
                                arrivals: arrivals.iter().map(|&a| a + offset).collect(),
                                departures: departures.iter().map(|&d| d + offset).collect(),
                            });
                            dep += f.headway_secs as i32;
                        }
                    }
                }
                None => {
                    patterns[pattern_idx as usize].trips.push(PatternTrip {
                        trip_id: trip_id.clone(),
                        service_id: trip.service_id.clone(),
                        arrivals,
                        departures,
                    });
                }
            }
        }

        for pattern in &mut patterns {
            pattern.trips.sort_by(|a, b| a.departures[0].cmp(&b.departures[0]).then_with(|| a.trip_id.0.cmp(&b.trip_id.0)));
        }

        // 6. stop_patterns 逆引き索引の構築。
        let mut stop_patterns: Vec<Vec<(PatternIdx, u32)>> = vec![Vec::new(); stop_ids.len()];
        for (p_idx, pattern) in patterns.iter().enumerate() {
            for (pos, &stop) in pattern.stops.iter().enumerate() {
                stop_patterns[stop as usize].push((p_idx as PatternIdx, pos as u32));
            }
        }

        // 7. カレンダーの統合。
        let mut calendars: HashMap<otp_core::ServiceId, ServiceCalendar> = HashMap::new();
        for feed in feeds {
            for cal in &feed.calendars {
                calendars.entry(cal.service_id.clone()).or_default().weekdays = Some(cal.weekdays);
                let entry = calendars.get_mut(&cal.service_id).unwrap();
                entry.start_date = cal.start_date;
                entry.end_date = cal.end_date;
            }
            for cd in &feed.calendar_dates {
                let entry = calendars.entry(cd.service_id.clone()).or_default();
                if cd.added {
                    entry.added_dates.push(cd.date);
                } else {
                    entry.removed_dates.push(cd.date);
                }
            }
        }

        // 8. 乗換エッジの構築 (transfers.txt 非対応: 実測で都営/メトロ/りんかい/京王/
        //    東武いずれのフィードにも transfers.txt が存在しないことを確認済み)。
        //    - 自己ループ: 同一正規化停留所内の乗換に既定バッファを課す
        //      (parent_station で束ねたプラットフォーム間、および同一 stop_id を
        //      複数パターンが共有するケース, 例: 都営大江戸線 都庁前 のループ⇄放射線分岐)。
        //    - 近接エッジ: 正規化停留所同士の直線距離が閾値以内なら、徒歩時間+バッファの
        //      エッジを双方向に張る (同一事業者内の同名別プラットフォーム、および
        //      フィードをまたぐ同一/隣接駅の乗換、例: 東京メトロ↔都営 白金高輪)。
        let stop_coords: Vec<LatLng> = stop_ids
            .iter()
            .map(|id| {
                let (lat_sum, lng_sum, n) = coord_acc.get(id).copied().unwrap_or((0.0, 0.0, 1));
                LatLng::new(lat_sum / n.max(1) as f64, lng_sum / n.max(1) as f64)
            })
            .collect();

        // 同一停留所内 (自己ループ) の乗換バッファは `transfers` に別エントリとして
        // 持たず、`search` 側でパターン乗車チェック時に「直前ラベルが `Parent::Board`
        // (=乗換無しでは同一停留所にそのまま留まっている) なら既定バッファを課す」
        // 形で扱う (egress/到着時刻の報告にバッファを混ぜ込まないため。詳細は
        // `search` 内のコメント参照)。ここで作るのは異なる正規化停留所間の
        // 近接徒歩乗換エッジのみ。
        let n = stop_ids.len();
        let mut transfers: Vec<Vec<Transfer>> = vec![Vec::new(); n];
        for i in 0..n {
            for j in (i + 1)..n {
                let dist_m = stop_coords[i].haversine_m(&stop_coords[j]);
                if dist_m <= MAX_WALK_TRANSFER_M {
                    let walk_s = (dist_m / WALK_SPEED_MPS).ceil() as u32;
                    let duration_s = walk_s + DEFAULT_TRANSFER_BUFFER_S;
                    transfers[i].push(Transfer { to_stop: j as StopIdx, duration_s });
                    transfers[j].push(Transfer { to_stop: i as StopIdx, duration_s });
                }
            }
        }

        let stop_zones: Vec<Option<String>> = stop_ids.iter().map(|id| zone_of.get(id).cloned()).collect();
        let stop_names: Vec<String> = stop_ids
            .iter()
            .map(|id| name_of.get(id).cloned().unwrap_or_else(|| id.as_str().to_string()))
            .collect();

        Ok(Timetable { stop_ids, stop_names, stop_lookup, patterns, stop_patterns, transfers, calendars, stop_coords, stop_zones })
    }

    /// StopIdx → 停留所名。名前が無ければ StopId を返す (フォールバック)。
    pub fn stop_name(&self, stop: StopIdx) -> &str {
        self.stop_names.get(stop as usize).map(String::as_str).unwrap_or("")
    }

    /// GTFS 生 stop_id (プラットフォーム含む) または正規化済み StopId から StopIdx を引く。
    pub fn stop_idx(&self, stop_id: &StopId) -> Option<StopIdx> {
        self.stop_lookup.get(stop_id).copied()
    }

    /// StopIdx → 座標。
    pub fn stop_coord(&self, stop: StopIdx) -> LatLng {
        self.stop_coords[stop as usize]
    }

    /// StopIdx → 運賃ゾーン (`zone_id`, 名前空間化済み)。無ければ `None`
    /// (`zone_id` 列を持たないフィード、例: 自前頻度 GTFS の JR)。otp-engine が
    /// `otp_fares::FareLeg` を組むために使う。
    pub fn stop_zone(&self, stop: StopIdx) -> Option<&str> {
        self.stop_zones[stop as usize].as_deref()
    }

    /// `coord` から直線距離 `radius_m` 以内の停留所を、近い順に返す。
    ///
    /// otp-engine の access/egress 徒歩探索の候補駅集めに使う (`Engine::plan`)。
    /// 停留所数に対して線形探索 (東京都心規模の駅数なら十分高速。広域化する
    /// 場合はグリッド索引への切り替えを検討: `otp-street::nearest_node` の
    /// コメント参照)。
    pub fn nearby_stops(&self, coord: LatLng, radius_m: f64) -> Vec<(StopIdx, LatLng)> {
        let mut found: Vec<(StopIdx, LatLng, f64)> = self
            .stop_coords
            .iter()
            .enumerate()
            .map(|(i, &c)| (i as StopIdx, c, coord.haversine_m(&c)))
            .filter(|&(_, _, d)| d <= radius_m)
            .collect();
        found.sort_by(|a, b| a.2.partial_cmp(&b.2).unwrap_or(std::cmp::Ordering::Equal));
        found.into_iter().map(|(i, c, _)| (i, c)).collect()
    }

    fn service_active(&self, service_id: &otp_core::ServiceId, date: u32) -> bool {
        let Some(cal) = self.calendars.get(service_id) else { return false };
        if cal.removed_dates.contains(&date) {
            return false;
        }
        if cal.added_dates.contains(&date) {
            return true;
        }
        match cal.weekdays {
            Some(weekdays) => {
                if date < cal.start_date || date > cal.end_date {
                    return false;
                }
                weekdays[weekday_index(date)]
            }
            None => false,
        }
    }
}

/// YYYYMMDD の曜日インデックスを返す (0=月曜 .. 6=日曜、GTFS calendar.txt の列順に合わせる)。
/// Howard Hinnant の `days_from_civil` (proleptic Gregorian, 1970-01-01 起点) を使用。
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400; // [0, 399]
    let mp = (m + 9) % 12; // [0, 11]
    let doy = (153 * mp + 2) / 5 + d - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146097 + doe - 719468
}

fn weekday_index(date: u32) -> usize {
    let y = (date / 10000) as i64;
    let m = ((date / 100) % 100) as i64;
    let d = (date % 100) as i64;
    let days = days_from_civil(y, m, d);
    // 1970-01-01 は木曜 (Mon=0 起点で index=3)。
    (((days + 3) % 7 + 7) % 7) as usize
}

/// 徒歩アクセス/イグレス (出発地→駅, 駅→目的地) の1本。
/// `otp-street` の歩行探索結果から与える。このスライスでは駅指定 (duration=0) のみを使う。
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
    /// 対象サービス日 (YYYYMMDD)。calendar / calendar_dates の判定に使う。
    /// (拡張: 当初の型に無かったが、実際の日付を跨ぐ運行判定に必須なため追加した)
    pub service_date: u32,
    /// 最大乗換回数 (RAPTOR のラウンド数上限)。
    pub max_rounds: u8,
    /// 鉄道限定フラグ。`true` のとき、`route_type` が鉄道 (`RouteType::is_rail`) でない
    /// パターン (=バス) には一切乗車しない (乗換 footpath は従来どおり有効)。同一時刻表を
    /// 共有したまま「鉄道のみの代替経路」を第2の探索として得るために使う (バス専用の
    /// 時刻表を別途構築せずに済む = 追加メモリなし)。既存呼び出しは `false` で従来動作。
    pub rail_only: bool,
    /// arrive-by (到着時刻指定) フラグ。`false` なら従来どおり `earliest_departure` を
    /// 出発時刻とする前方探索 (最早到着)。`true` のとき `earliest_departure` は「目的地への
    /// 到着締切時刻 T」を意味し、egress 停留所側から時間を遡る後方 RAPTOR を回して
    /// 「T までに到着し、かつ出発をできるだけ遅らせる (最遅出発)」経路を返す。
    /// 返す `Journey` は前方 (出発地→目的地) 順の leg 列・絶対時刻で、前方探索と同一の
    /// `Journey`/`JourneyLeg` 型なので下流 (`journey_to_itinerary`) は無改修。既存呼び出しは
    /// `false` で従来動作 (バイト単位で不変)。
    pub arrive_by: bool,
}

/// 探索結果の1区間。
#[derive(Debug, Clone)]
pub enum JourneyLeg {
    Walk { stop: StopIdx, duration_s: u32 },
    Transit {
        route_id: RouteId,
        route_short_name: String,
        route_long_name: String,
        route_type: otp_gtfs::RouteType,
        agency_id: String,
        trip_id: TripId,
        from: StopIdx,
        to: StopIdx,
        board_s: SecondsSinceMidnight,
        alight_s: SecondsSinceMidnight,
    },
}

/// 探索結果の1経路 (Pareto 最適候補の1つ)。
#[derive(Debug, Clone)]
pub struct Journey {
    pub legs: Vec<JourneyLeg>,
    pub arrival_s: SecondsSinceMidnight,
    pub transfers: u8,
}

/// RAPTOR の1ラウンド内のラベル。`arrival` は最早到着時刻、`parent` はどう到達したか。
#[derive(Debug, Clone, Copy)]
struct Label {
    arrival: i64,
    parent: Option<Parent>,
}

#[derive(Debug, Clone, Copy)]
enum Parent {
    /// access リンクで直接到達 (0ラウンド目)。
    Access,
    /// egress リンクで目的地側から直接到達 (arrive-by 後方探索の0ラウンド目)。
    /// 前方探索の [`Parent::Access`] と対称。arrive-by 探索でのみ生成される。
    Egress,
    /// パターン `pattern` の便に `board_stop` (パターン内位置 `board_pos`) で乗車し、
    /// `alight_pos` で降りた。乗車時に参照した「1ラウンド前のラベル」が属していたラウンドを
    /// `prev_round` に保持し、遡上時にどのラウンドを見ればよいか自己完結させる。
    Board { pattern: PatternIdx, trip_idx: usize, board_stop: StopIdx, board_pos: usize, alight_pos: usize, prev_round: usize },
    /// 乗換エッジ (`Timetable::transfers`, 近接駅の徒歩乗換) を辿って `from_stop` から
    /// 同一ラウンド内で到達した。同一ラウンド内の遷移なので `prev_round` は持たない
    /// (遡上時は `cur_round` を変えずに `from_stop` へ移る)。
    Transfer { from_stop: StopIdx, duration_s: u32 },
}

const INF: i64 = i64::MAX;
/// arrive-by 後方探索の未到達ラベル値。前方探索が最早到着を `INF` で初期化して
/// 最小化するのと対称に、後方探索は最遅在線時刻を `NEG` で初期化して最大化する。
const NEG: i64 = i64::MIN;

impl Timetable {
    /// RAPTOR 探索本体。到着時刻と乗換回数で Pareto 最適な複数経路を返す。
    /// 見つからなければ空 Vec (エラーではない: 「その日は運行が無い」等の正常系)。
    pub fn search(&self, query: &RaptorQuery) -> Result<Vec<Journey>> {
        // arrive-by (到着締切指定) は egress 側から時間を遡る後方 RAPTOR に委譲する。
        // 前方探索 (以下) のコードはそのまま = 従来動作をバイト単位で保つ。
        if query.arrive_by {
            return self.search_arrive_by(query);
        }
        let n = self.stop_ids.len();
        let mut rounds: Vec<Vec<Label>> = Vec::with_capacity(query.max_rounds as usize + 1);

        let mut round0 = vec![Label { arrival: INF, parent: None }; n];
        for link in &query.access {
            let t = query.earliest_departure as i64 + link.duration_s as i64;
            let slot = &mut round0[link.stop as usize];
            if t < slot.arrival {
                *slot = Label { arrival: t, parent: Some(Parent::Access) };
            }
        }
        rounds.push(round0);

        let mut best: Vec<i64> = rounds[0].iter().map(|l| l.arrival).collect();
        let mut marked: Vec<StopIdx> = query.access.iter().map(|l| l.stop).filter(|&s| (s as usize) < n).collect();
        marked.sort_unstable();
        marked.dedup();

        for k in 1..=query.max_rounds as usize {
            let prev = rounds[k - 1].clone();
            let mut cur = prev.clone();

            let mut touched_patterns: Vec<PatternIdx> = Vec::new();
            for &s in &marked {
                for &(p, _pos) in &self.stop_patterns[s as usize] {
                    touched_patterns.push(p);
                }
            }
            touched_patterns.sort_unstable();
            touched_patterns.dedup();

            let mut next_marked: Vec<StopIdx> = Vec::new();

            for &p in &touched_patterns {
                let pattern = &self.patterns[p as usize];
                // 鉄道限定探索では非鉄道パターン (バス) への乗車を丸ごとスキップする。
                // 乗換 footpath は下段でそのまま辿るため、駅間の徒歩接続は影響を受けない。
                if query.rail_only && !pattern.route_type.is_rail() {
                    continue;
                }
                let Some(start_pos) = pattern.stops.iter().position(|s| marked.binary_search(s).is_ok()) else {
                    continue;
                };

                let mut boarded: Option<(usize, usize)> = None; // (trip_idx, board_pos)
                for pos in start_pos..pattern.stops.len() {
                    let stop = pattern.stops[pos];

                    if let Some((trip_idx, board_pos)) = boarded {
                        let trip = &pattern.trips[trip_idx];
                        let arr = trip.arrivals[pos] as i64;
                        if arr < best[stop as usize] && arr < cur[stop as usize].arrival {
                            cur[stop as usize] = Label {
                                arrival: arr,
                                parent: Some(Parent::Board {
                                    pattern: p,
                                    trip_idx,
                                    board_stop: pattern.stops[board_pos],
                                    board_pos,
                                    alight_pos: pos,
                                    prev_round: k - 1,
                                }),
                            };
                            next_marked.push(stop);
                        }
                    }

                    let prev_label = &prev[stop as usize];
                    let prev_arrival = prev_label.arrival;
                    if prev_arrival != INF {
                        // 直前ラベルが `Board` (=乗換エッジを経由せず同一停留所にそのまま
                        // 留まっている) なら、同一停留所内乗換の既定バッファを課す。
                        // `Access`/`Transfer` はそれぞれ「初回乗車」「乗換エッジの所要時間に
                        // 既にバッファ込み」なので追加しない (二重計上を避ける)。
                        let boarding_ready = prev_arrival
                            + match prev_label.parent {
                                Some(Parent::Board { .. }) => DEFAULT_TRANSFER_BUFFER_S as i64,
                                _ => 0,
                            };
                        if let Some(candidate) = earliest_catchable_trip(pattern, pos, boarding_ready, query.service_date, self) {
                            let is_better = match boarded {
                                None => true,
                                Some((cur_trip, _)) => pattern.trips[candidate].departures[pos] < pattern.trips[cur_trip].departures[pos],
                            };
                            if is_better {
                                boarded = Some((candidate, pos));
                            }
                        }
                    }
                }
            }

            next_marked.sort_unstable();
            next_marked.dedup();
            for &s in &next_marked {
                if cur[s as usize].arrival < best[s as usize] {
                    best[s as usize] = cur[s as usize].arrival;
                }
            }

            // 乗換緩和 (footpath relaxation): このラウンドで乗車により到達した停留所
            // (`next_marked`) から、近接駅の徒歩乗換エッジを1ホップだけ辿る。連鎖乗換は
            // しない (標準的な RAPTOR の footpath 前提に合わせる。徒歩を2回繋ぐより、
            // 乗換エッジ自体を広く張る方が実態に合う)。
            let mut transfer_marked: Vec<StopIdx> = Vec::new();
            for &s in &next_marked {
                let arrival_at_s = cur[s as usize].arrival;
                if arrival_at_s == INF {
                    continue;
                }
                for tr in &self.transfers[s as usize] {
                    let target = tr.to_stop as usize;
                    let candidate = arrival_at_s + tr.duration_s as i64;
                    if candidate < best[target] && candidate < cur[target].arrival {
                        cur[target] = Label { arrival: candidate, parent: Some(Parent::Transfer { from_stop: s, duration_s: tr.duration_s }) };
                        transfer_marked.push(tr.to_stop);
                    }
                }
            }
            next_marked.extend(transfer_marked);
            next_marked.sort_unstable();
            next_marked.dedup();
            for &s in &next_marked {
                if cur[s as usize].arrival < best[s as usize] {
                    best[s as usize] = cur[s as usize].arrival;
                }
            }

            rounds.push(cur);
            if next_marked.is_empty() {
                break;
            }
            marked = next_marked;
        }

        let mut journeys = Vec::new();
        let mut best_so_far = INF;
        for (k, round) in rounds.iter().enumerate() {
            let mut best_egress: Option<(StopIdx, i64, u32)> = None;
            for link in &query.egress {
                let Some(label) = round.get(link.stop as usize) else { continue };
                if label.arrival == INF {
                    continue;
                }
                let total = label.arrival + link.duration_s as i64;
                if best_egress.is_none_or(|(_, t, _)| total < t) {
                    best_egress = Some((link.stop, total, link.duration_s));
                }
            }
            if let Some((stop, total, egress_duration)) = best_egress {
                if total < best_so_far {
                    best_so_far = total;
                    let legs = self.reconstruct(&rounds, k, stop, egress_duration, query);
                    let transit_legs = legs.iter().filter(|l| matches!(l, JourneyLeg::Transit { .. })).count();
                    journeys.push(Journey {
                        legs,
                        arrival_s: total as SecondsSinceMidnight,
                        transfers: transit_legs.saturating_sub(1) as u8,
                    });
                }
            }
        }

        Ok(journeys)
    }

    fn reconstruct(&self, rounds: &[Vec<Label>], round: usize, egress_stop: StopIdx, egress_duration: u32, query: &RaptorQuery) -> Vec<JourneyLeg> {
        let mut legs_rev: Vec<JourneyLeg> = Vec::new();
        if egress_duration > 0 {
            legs_rev.push(JourneyLeg::Walk { stop: egress_stop, duration_s: egress_duration });
        }

        let mut cur_round = round;
        let mut cur_stop = egress_stop;
        loop {
            let label = rounds[cur_round][cur_stop as usize];
            match label.parent {
                Some(Parent::Board { pattern, trip_idx, board_stop, board_pos, alight_pos, prev_round }) => {
                    let pat = &self.patterns[pattern as usize];
                    let trip = &pat.trips[trip_idx];
                    legs_rev.push(JourneyLeg::Transit {
                        route_id: pat.route_id.clone(),
                        route_short_name: pat.route_short_name.clone(),
                        route_long_name: pat.route_long_name.clone(),
                        route_type: pat.route_type,
                        agency_id: pat.agency_id.clone(),
                        trip_id: trip.trip_id.clone(),
                        from: board_stop,
                        to: cur_stop,
                        board_s: trip.departures[board_pos],
                        alight_s: trip.arrivals[alight_pos],
                    });
                    cur_stop = board_stop;
                    cur_round = prev_round;
                }
                Some(Parent::Transfer { from_stop, duration_s }) => {
                    if duration_s > 0 {
                        legs_rev.push(JourneyLeg::Walk { stop: cur_stop, duration_s });
                    }
                    cur_stop = from_stop;
                    // 乗換エッジは同一ラウンド内の遷移: cur_round は変えない。
                }
                Some(Parent::Access) => {
                    if let Some(link) = query.access.iter().find(|l| l.stop == cur_stop) {
                        if link.duration_s > 0 {
                            legs_rev.push(JourneyLeg::Walk { stop: cur_stop, duration_s: link.duration_s });
                        }
                    }
                    break;
                }
                // Egress は arrive-by 後方探索専用の親で前方探索では生成されない。到達不能
                // ラベルの遡上 (None) と併せて防御的に打ち切る。
                Some(Parent::Egress) | None => break,
            }
        }

        legs_rev.reverse();
        legs_rev
    }

    /// arrive-by (到着締切指定) の後方 RAPTOR 探索本体。
    ///
    /// [`Timetable::search`] (前方 = 最早到着) を時間反転で鏡写しにしたもの。egress 停留所
    /// (目的地近傍) に「締切 T − egress 徒歩」で座標を張り、時間を遡りながら各停留所の
    /// **最遅在線時刻** (=そこを出発してもまだ T までに目的地へ着ける最も遅い時刻) を
    /// ラウンドごとに **最大化** して緩和する。パターンは後方 (dest 寄り → origin 寄り) に
    /// 走査し、「pos で降車 (arrival ≤ 締切) できる最も遅い便」に乗って、より前方 (origin 寄り)
    /// の停留所へ「その便の departure」を伝播する。最後に access 停留所 (出発地近傍) を
    /// egress 相当として、出発地の出発時刻 (= access 停留所の最遅在線時刻 − access 徒歩) を
    /// 最大化する経路を集める。
    ///
    /// 返す `Journey` は前方探索と同じく **出発地→目的地順** の leg 列・絶対 board/alight
    /// 時刻を持ち、`arrival_s` は目的地への実到着時刻 (T 以下を構成上保証)。ラベルの意味だけ
    /// 「最早到着」→「最遅在線」に読み替え、`Label.arrival` フィールドは最遅在線時刻を保持する。
    fn search_arrive_by(&self, query: &RaptorQuery) -> Result<Vec<Journey>> {
        let n = self.stop_ids.len();
        // 締切 T (arrive-by では earliest_departure フィールドを到着締切として読む)。
        let deadline = query.earliest_departure as i64;
        let mut rounds: Vec<Vec<Label>> = Vec::with_capacity(query.max_rounds as usize + 1);

        // 0ラウンド目: egress 停留所に「T − egress 徒歩」を張る (この時刻までにその駅に
        // いれば徒歩で締切内に目的地へ着ける)。最大化なので大きい方を採用する。
        let mut round0 = vec![Label { arrival: NEG, parent: None }; n];
        for link in &query.egress {
            let t = deadline - link.duration_s as i64;
            let slot = &mut round0[link.stop as usize];
            if t > slot.arrival {
                *slot = Label { arrival: t, parent: Some(Parent::Egress) };
            }
        }
        rounds.push(round0);

        let mut best: Vec<i64> = rounds[0].iter().map(|l| l.arrival).collect();
        let mut marked: Vec<StopIdx> = query.egress.iter().map(|l| l.stop).filter(|&s| (s as usize) < n).collect();
        marked.sort_unstable();
        marked.dedup();

        for k in 1..=query.max_rounds as usize {
            let prev = rounds[k - 1].clone();
            let mut cur = prev.clone();

            let mut touched_patterns: Vec<PatternIdx> = Vec::new();
            for &s in &marked {
                for &(p, _pos) in &self.stop_patterns[s as usize] {
                    touched_patterns.push(p);
                }
            }
            touched_patterns.sort_unstable();
            touched_patterns.dedup();

            let mut next_marked: Vec<StopIdx> = Vec::new();

            for &p in &touched_patterns {
                let pattern = &self.patterns[p as usize];
                if query.rail_only && !pattern.route_type.is_rail() {
                    continue;
                }
                // 後方走査の起点は marked のうち最も後方 (dest 寄り) の位置。そこから前方へ
                // 「降車→前方停留所の乗車時刻を伝播」する (前方探索が最初の marked から
                // 後方へ走るのと対称)。
                let Some(end_pos) = pattern.stops.iter().rposition(|s| marked.binary_search(s).is_ok()) else {
                    continue;
                };

                let mut boarded: Option<(usize, usize)> = None; // (trip_idx, alight_pos)
                for pos in (0..=end_pos).rev() {
                    let stop = pattern.stops[pos];

                    if let Some((trip_idx, alight_pos)) = boarded {
                        // この便に (前方 pos で) 乗車し、alight_pos で降りる。前方停留所 pos の
                        // 最遅在線時刻 = その便の departure[pos]。最大化する。
                        let trip = &pattern.trips[trip_idx];
                        let dep = trip.departures[pos] as i64;
                        if dep > best[stop as usize] && dep > cur[stop as usize].arrival {
                            cur[stop as usize] = Label {
                                arrival: dep,
                                parent: Some(Parent::Board {
                                    pattern: p,
                                    trip_idx,
                                    board_stop: stop,
                                    board_pos: pos,
                                    alight_pos,
                                    prev_round: k - 1,
                                }),
                            };
                            next_marked.push(stop);
                        }
                    }

                    let prev_label = &prev[stop as usize];
                    let prev_time = prev_label.arrival;
                    if prev_time != NEG {
                        // 直前ラベルが `Board` (=乗換エッジを経由せず同一停留所でそのまま
                        // 次便に乗り継ぐ) なら同一駅乗換バッファを差し引く: この便の降車は
                        // 「次便の乗車 (prev_time) − バッファ」以前でなければならない。
                        // `Egress`/`Transfer` は追加しない (前方探索の二重計上回避と対称)。
                        let alight_ready = prev_time
                            - match prev_label.parent {
                                Some(Parent::Board { .. }) => DEFAULT_TRANSFER_BUFFER_S as i64,
                                _ => 0,
                            };
                        if let Some(candidate) = latest_alightable_trip(pattern, pos, alight_ready, query.service_date, self) {
                            let is_better = match boarded {
                                None => true,
                                Some((cur_trip, _)) => pattern.trips[candidate].arrivals[pos] > pattern.trips[cur_trip].arrivals[pos],
                            };
                            if is_better {
                                boarded = Some((candidate, pos));
                            }
                        }
                    }
                }
            }

            next_marked.sort_unstable();
            next_marked.dedup();
            for &s in &next_marked {
                if cur[s as usize].arrival > best[s as usize] {
                    best[s as usize] = cur[s as usize].arrival;
                }
            }

            // 乗換緩和 (footpath, 逆向き): 乗車で更新した停留所から近接駅へ徒歩1ホップ。
            // 前方の「到着 + 徒歩」に対し、後方は「最遅在線 − 徒歩」で相手側の最遅在線を
            // 最大化する (乗換エッジは対称なので所要は同じ)。
            let mut transfer_marked: Vec<StopIdx> = Vec::new();
            for &s in &next_marked {
                let latest_at_s = cur[s as usize].arrival;
                if latest_at_s == NEG {
                    continue;
                }
                for tr in &self.transfers[s as usize] {
                    let target = tr.to_stop as usize;
                    let candidate = latest_at_s - tr.duration_s as i64;
                    if candidate > best[target] && candidate > cur[target].arrival {
                        // `from_stop` フィールドには「dest 寄り (この徒歩の到達側 = s)」を格納する。
                        // 前方探索では from_stop に「発地側」を入れるが、逆探索の遡上は dest 側へ
                        // 進むため、reconstruct_arrive_by 側でこの向きに読み替える。
                        cur[target] = Label { arrival: candidate, parent: Some(Parent::Transfer { from_stop: s, duration_s: tr.duration_s }) };
                        transfer_marked.push(tr.to_stop);
                    }
                }
            }
            next_marked.extend(transfer_marked);
            next_marked.sort_unstable();
            next_marked.dedup();
            for &s in &next_marked {
                if cur[s as usize].arrival > best[s as usize] {
                    best[s as usize] = cur[s as usize].arrival;
                }
            }

            rounds.push(cur);
            if next_marked.is_empty() {
                break;
            }
            marked = next_marked;
        }

        // 経路収集: access 停留所 (出発地近傍) を egress 相当として、出発地の出発時刻
        // (= access 停留所の最遅在線時刻 − access 徒歩) を最大化する経路をラウンドごとに拾う。
        let mut journeys = Vec::new();
        let mut best_departure = NEG;
        for (k, round) in rounds.iter().enumerate() {
            let mut best_access: Option<(StopIdx, i64, u32)> = None; // (stop, 出発地出発時刻, access徒歩)
            for link in &query.access {
                let Some(label) = round.get(link.stop as usize) else { continue };
                if label.arrival == NEG {
                    continue;
                }
                let depart_origin = label.arrival - link.duration_s as i64;
                if best_access.is_none_or(|(_, d, _)| depart_origin > d) {
                    best_access = Some((link.stop, depart_origin, link.duration_s));
                }
            }
            if let Some((stop, depart_origin, access_duration)) = best_access {
                if depart_origin > best_departure {
                    best_departure = depart_origin;
                    let (legs, arrival) = self.reconstruct_arrive_by(&rounds, k, stop, access_duration, deadline, query);
                    let transit_legs = legs.iter().filter(|l| matches!(l, JourneyLeg::Transit { .. })).count();
                    journeys.push(Journey {
                        legs,
                        arrival_s: arrival as SecondsSinceMidnight,
                        transfers: transit_legs.saturating_sub(1) as u8,
                    });
                }
            }
        }

        Ok(journeys)
    }

    /// arrive-by 探索のラベル遡上。access 停留所 (round `round`) から目的地方向へ親ポインタを
    /// 辿り、**出発地→目的地順** の leg 列を直接組む (前方探索の `reconstruct` のような最終
    /// reverse は不要: 逆探索の遡上方向がそのまま前方順になる)。戻り値は (leg 列, 目的地への
    /// 実到着時刻)。実到着 = 末尾 Transit の alight + それ以降の徒歩 (footpath + egress) 合計で、
    /// 構成上 `deadline` 以下になる。
    fn reconstruct_arrive_by(&self, rounds: &[Vec<Label>], round: usize, access_stop: StopIdx, access_duration: u32, deadline: i64, query: &RaptorQuery) -> (Vec<JourneyLeg>, i64) {
        let mut legs: Vec<JourneyLeg> = Vec::new();
        // 先頭: 出発地 → access 停留所の徒歩。
        if access_duration > 0 {
            legs.push(JourneyLeg::Walk { stop: access_stop, duration_s: access_duration });
        }

        let mut cur_round = round;
        let mut cur_stop = access_stop;
        loop {
            let label = rounds[cur_round][cur_stop as usize];
            match label.parent {
                Some(Parent::Board { pattern, trip_idx, board_pos, alight_pos, prev_round, .. }) => {
                    let pat = &self.patterns[pattern as usize];
                    let trip = &pat.trips[trip_idx];
                    let alight_stop = pat.stops[alight_pos];
                    legs.push(JourneyLeg::Transit {
                        route_id: pat.route_id.clone(),
                        route_short_name: pat.route_short_name.clone(),
                        route_long_name: pat.route_long_name.clone(),
                        route_type: pat.route_type,
                        agency_id: pat.agency_id.clone(),
                        trip_id: trip.trip_id.clone(),
                        from: cur_stop, // = pat.stops[board_pos]
                        to: alight_stop,
                        board_s: trip.departures[board_pos],
                        alight_s: trip.arrivals[alight_pos],
                    });
                    cur_stop = alight_stop;
                    cur_round = prev_round;
                }
                Some(Parent::Transfer { from_stop, duration_s }) => {
                    // 逆探索では from_stop に dest 寄り (徒歩の到達側) を格納している。
                    // 前方順ではこの徒歩は cur_stop → from_stop なので、to 側 = from_stop。
                    if duration_s > 0 {
                        legs.push(JourneyLeg::Walk { stop: from_stop, duration_s });
                    }
                    cur_stop = from_stop;
                    // 乗換エッジは同一ラウンド内の遷移: cur_round は変えない。
                }
                Some(Parent::Egress) => {
                    if let Some(link) = query.egress.iter().find(|l| l.stop == cur_stop) {
                        if link.duration_s > 0 {
                            legs.push(JourneyLeg::Walk { stop: cur_stop, duration_s: link.duration_s });
                        }
                    }
                    break;
                }
                Some(Parent::Access) | None => break, // arrive-by では Access は生成されない (防御的)
            }
        }

        // 実到着時刻 = 末尾 Transit の alight + それ以降の徒歩 (footpath + egress) 合計。
        let mut arrival = deadline;
        if let Some(last_transit) = legs.iter().rposition(|l| matches!(l, JourneyLeg::Transit { .. })) {
            if let JourneyLeg::Transit { alight_s, .. } = &legs[last_transit] {
                let trailing_walk: i64 = legs[last_transit + 1..]
                    .iter()
                    .filter_map(|l| match l {
                        JourneyLeg::Walk { duration_s, .. } => Some(*duration_s as i64),
                        _ => None,
                    })
                    .sum();
                arrival = *alight_s as i64 + trailing_walk;
            }
        }
        (legs, arrival)
    }
}

/// パターン内位置 `pos` において、`not_before` 以降に出発し、かつ `date` に運行している
/// 最も早い便を探す (パターン内の便は始発出発時刻昇順 = 追い越し無し前提で各位置でも昇順)。
fn earliest_catchable_trip(pattern: &Pattern, pos: usize, not_before: i64, date: u32, tt: &Timetable) -> Option<usize> {
    pattern
        .trips
        .iter()
        .position(|t| t.departures[pos] as i64 >= not_before && tt.service_active(&t.service_id, date))
}

/// [`earliest_catchable_trip`] の arrive-by 版。パターン内位置 `pos` において、
/// `not_after` 以前に **到着** し、かつ `date` に運行している最も遅い便を探す
/// (便は始発出発昇順 = 追い越し無し前提で各位置の到着も昇順なので、後ろから最初に
/// 条件を満たす便 = 到着が最も遅い便を `rposition` で拾う)。到着を最遅にすることで、
/// その便が前方停留所を出発する時刻 (=最遅在線時刻) も最大化される。
fn latest_alightable_trip(pattern: &Pattern, pos: usize, not_after: i64, date: u32, tt: &Timetable) -> Option<usize> {
    pattern
        .trips
        .iter()
        .rposition(|t| t.arrivals[pos] as i64 <= not_after && tt.service_active(&t.service_id, date))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_dir() -> std::path::PathBuf {
        std::path::PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../gtfs/tests/fixtures/mini"))
    }

    fn load() -> Timetable {
        let feed = Feed::load_from_dir(&fixture_dir()).expect("mini fixture should load");
        Timetable::build(&[feed]).expect("timetable should build")
    }

    #[test]
    fn stop_zone_resolves_namespaced_zone_and_collapses_platforms_to_first_seen() {
        use otp_gtfs::{Stop, WheelchairBoarding};
        // 手組みの Feed: A駅 (zone無し) と、C駅 (parent_station=C, プラットフォーム
        // C1(zone=900)/C2(zone=901) を持つ) の2駅。実データはこのプラットフォーム分岐が
        // ほぼ発生しないため、Timetable::build の「最初に見つかった非空 zone_id を採用」
        // という単純化 (struct doc 参照) を明示的に検証する。
        let feed = Feed {
            stops: vec![
                Stop { id: StopId::new("A"), name: "A".into(), lat: 35.0, lng: 139.0, wheelchair_boarding: WheelchairBoarding::Unknown, parent_station: None, zone_id: None },
                Stop { id: StopId::new("C1"), name: "C1".into(), lat: 35.1, lng: 139.1, wheelchair_boarding: WheelchairBoarding::Unknown, parent_station: Some(StopId::new("C")), zone_id: Some("900".into()) },
                Stop { id: StopId::new("C2"), name: "C2".into(), lat: 35.1, lng: 139.1, wheelchair_boarding: WheelchairBoarding::Unknown, parent_station: Some(StopId::new("C")), zone_id: Some("901".into()) },
            ],
            ..Feed::default()
        };
        let tt = Timetable::build(&[feed]).expect("timetable should build");

        let a = tt.stop_idx(&StopId::new("A")).expect("A should be registered");
        assert_eq!(tt.stop_zone(a), None, "zone_idが無い駅はNone");

        let c = tt.stop_idx(&StopId::new("C")).expect("C (canonical) should be registered");
        assert_eq!(tt.stop_zone(c), Some("900"), "C1(900)が先に見つかるのでC1のzoneを採用");
    }

    #[test]
    fn weekday_index_matches_known_dates() {
        // 2026-07-13 は月曜日、2026-07-14 は火曜日 (実測: python datetime で確認)。
        assert_eq!(weekday_index(20260713), 0);
        assert_eq!(weekday_index(20260714), 1);
        assert_eq!(weekday_index(20260719), 6); // 日曜日
        assert_eq!(weekday_index(20260720), 0); // 月曜日
    }

    #[test]
    fn build_expands_frequency_trips_into_headway_spaced_departures() {
        use otp_gtfs::{Calendar, Frequency, Route, RouteType, Stop, StopTime, Trip, WheelchairBoarding};
        // 雛形 trip T1 (A発0秒→B着300秒) を frequencies で 08:00-09:00 の10分間隔運行にする。
        // (32400-28800)/600 = 6本に展開され、各先頭発は 28800,29400,...,31800 になるはず。
        let feed = Feed {
            stops: vec![
                Stop { id: StopId::new("A"), name: "A".into(), lat: 35.0, lng: 139.0, wheelchair_boarding: WheelchairBoarding::Unknown, parent_station: None, zone_id: None },
                Stop { id: StopId::new("B"), name: "B".into(), lat: 35.01, lng: 139.01, wheelchair_boarding: WheelchairBoarding::Unknown, parent_station: None, zone_id: None },
            ],
            routes: vec![Route { id: RouteId::new("R1"), agency_id: None, short_name: "R1".into(), long_name: "R1".into(), route_type: RouteType::Rail }],
            trips: vec![Trip { id: TripId::new("T1"), route_id: RouteId::new("R1"), service_id: otp_core::ServiceId::new("WD"), headsign: None, wheelchair_accessible: WheelchairBoarding::Unknown }],
            stop_times: vec![
                StopTime { trip_id: TripId::new("T1"), stop_id: StopId::new("A"), stop_sequence: 1, arrival: 0, departure: 0 },
                StopTime { trip_id: TripId::new("T1"), stop_id: StopId::new("B"), stop_sequence: 2, arrival: 300, departure: 300 },
            ],
            calendars: vec![Calendar { service_id: otp_core::ServiceId::new("WD"), weekdays: [true, true, true, true, true, false, false], start_date: 20260101, end_date: 20301231 }],
            frequencies: vec![Frequency { trip_id: TripId::new("T1"), start_time: 28800, end_time: 32400, headway_secs: 600, exact_times: 0 }],
            ..Feed::default()
        };
        let tt = Timetable::build(&[feed]).expect("timetable should build");
        assert_eq!(tt.patterns.len(), 1, "1パターン (A→B)");
        let trips = &tt.patterns[0].trips;
        assert_eq!(trips.len(), 6, "10分間隔で6本に展開されるはず");
        let firsts: Vec<i32> = trips.iter().map(|t| t.departures[0]).collect();
        assert_eq!(firsts, vec![28800, 29400, 30000, 30600, 31200, 31800]);
        // 相対 stop_times のシフト: 各便の B 着は先頭発+300 のはず。
        assert_eq!(trips[0].arrivals[1], 29100);
        assert_eq!(trips[5].arrivals[1], 32100);
    }

    #[test]
    fn build_excludes_bus_routes() {
        let tt = load();
        // R3 (bus, route_type=3) の停留所列を含むパターンは無いはず。
        for pattern in &tt.patterns {
            assert_ne!(pattern.route_id.as_str(), "R3", "bus route must be excluded from rail-only timetable");
        }
    }

    #[test]
    fn build_collapses_platforms_to_parent_station() {
        let tt = load();
        let c1 = StopId::new("C1");
        let c2 = StopId::new("C2");
        let canonical_c = StopId::new("C");
        assert_eq!(tt.stop_idx(&c1), tt.stop_idx(&canonical_c));
        assert_eq!(tt.stop_idx(&c2), tt.stop_idx(&canonical_c));
    }

    #[test]
    fn service_active_respects_calendar_and_calendar_dates() {
        let tt = load();
        let wd = otp_core::ServiceId::new("WD");
        let wd_extra = otp_core::ServiceId::new("WD_EXTRA");
        // WD: calendar.txt で月曜のみ運行。2026-07-13(月)はOK, 2026-07-20(月)はcalendar_datesで運休。
        assert!(tt.service_active(&wd, 20260713));
        assert!(!tt.service_active(&wd, 20260720));
        assert!(!tt.service_active(&wd, 20260714)); // 火曜なので運行なし
        // WD_EXTRA: calendar.txt に無く、calendar_datesの追加でのみ2026-07-13が運行。
        assert!(tt.service_active(&wd_extra, 20260713));
        assert!(!tt.service_active(&wd_extra, 20260714));
    }

    #[test]
    fn search_finds_known_shortest_path_with_one_transfer() {
        let tt = load();
        let a = tt.stop_idx(&StopId::new("A")).unwrap();
        let d = tt.stop_idx(&StopId::new("D")).unwrap();

        let query = RaptorQuery {
            access: vec![StreetLink { stop: a, duration_s: 0 }],
            egress: vec![StreetLink { stop: d, duration_s: 0 }],
            earliest_departure: 8 * 3600, // 08:00:00
            service_date: 20260713,       // 月曜 (WD, WD_EXTRA とも運行)
            max_rounds: 2,
            rail_only: false,
            arrive_by: false,
        };

        let journeys = tt.search(&query).expect("search should not error");
        assert!(!journeys.is_empty(), "expected at least one journey");

        let best = journeys.last().unwrap();
        // 手計算: A(08:00)→B(08:05)→C1(08:10) [T1] → 乗換 → C2(08:20)→D(08:30) [T2]
        // R3 (バス, A→D 08:00→08:05) は鉄道フィルタで除外されるため選ばれない。
        assert_eq!(best.arrival_s, 8 * 3600 + 30 * 60, "expected arrival 08:30:00, got {}", best.arrival_s);
        assert_eq!(best.transfers, 1);

        let transit_legs: Vec<_> = best
            .legs
            .iter()
            .filter_map(|l| match l {
                JourneyLeg::Transit { trip_id, board_s, alight_s, .. } => Some((trip_id.as_str().to_string(), *board_s, *alight_s)),
                _ => None,
            })
            .collect();
        assert_eq!(transit_legs.len(), 2);
        assert_eq!(transit_legs[0], ("T1".to_string(), 8 * 3600, 8 * 3600 + 600));
        assert_eq!(transit_legs[1], ("T2".to_string(), 8 * 3600 + 1200, 8 * 3600 + 1800));
    }

    #[test]
    fn nearby_stops_finds_close_stops_sorted_by_distance_and_excludes_far_ones() {
        let tt = load();
        // 実測 (haversine): A-B ≈ 1437m, A-C ≈ 2875m, A-D ≈ 4312m。
        let a_coord = tt.stop_coord(tt.stop_idx(&StopId::new("A")).unwrap());
        let found = tt.nearby_stops(a_coord, 2000.0);

        let ids: Vec<&str> = found.iter().map(|(idx, _)| tt.stop_ids[*idx as usize].as_str()).collect();
        assert_eq!(ids, vec!["A", "B"], "2km以内はA自身とBのみのはず (C以遠は除外)");
    }

    #[test]
    fn search_returns_empty_when_service_not_running_that_day() {
        let tt = load();
        let a = tt.stop_idx(&StopId::new("A")).unwrap();
        let d = tt.stop_idx(&StopId::new("D")).unwrap();
        let query = RaptorQuery {
            access: vec![StreetLink { stop: a, duration_s: 0 }],
            egress: vec![StreetLink { stop: d, duration_s: 0 }],
            earliest_departure: 8 * 3600,
            service_date: 20260714, // 火曜: WD (月曜のみ) も WD_EXTRA (calendar_datesは13日のみ) も運行なし
            max_rounds: 2,
            rail_only: false,
            arrive_by: false,
        };
        let journeys = tt.search(&query).expect("search should not error");
        assert!(journeys.is_empty(), "expected no journey when nothing is running, got {journeys:?}");
    }

    /// A→B の1路線に3便 (08:00→08:30 / 08:20→08:50 / 08:40→09:10) を持つ手組みフィクスチャ。
    /// arrive-by と前方探索の両方をこの同一時刻表で検証する。
    fn arrive_by_fixture() -> Timetable {
        use otp_gtfs::{Calendar, Route, RouteType, Stop, StopTime, Trip, WheelchairBoarding};
        let feed = Feed {
            stops: vec![
                Stop { id: StopId::new("A"), name: "A".into(), lat: 35.0, lng: 139.0, wheelchair_boarding: WheelchairBoarding::Unknown, parent_station: None, zone_id: None },
                Stop { id: StopId::new("B"), name: "B".into(), lat: 35.2, lng: 139.2, wheelchair_boarding: WheelchairBoarding::Unknown, parent_station: None, zone_id: None },
            ],
            routes: vec![Route { id: RouteId::new("R1"), agency_id: None, short_name: "R1".into(), long_name: "R1".into(), route_type: RouteType::Rail }],
            trips: vec![
                Trip { id: TripId::new("Tr1"), route_id: RouteId::new("R1"), service_id: otp_core::ServiceId::new("WD"), headsign: None, wheelchair_accessible: WheelchairBoarding::Unknown },
                Trip { id: TripId::new("Tr2"), route_id: RouteId::new("R1"), service_id: otp_core::ServiceId::new("WD"), headsign: None, wheelchair_accessible: WheelchairBoarding::Unknown },
                Trip { id: TripId::new("Tr3"), route_id: RouteId::new("R1"), service_id: otp_core::ServiceId::new("WD"), headsign: None, wheelchair_accessible: WheelchairBoarding::Unknown },
            ],
            stop_times: vec![
                // Tr1: A 08:00 → B 08:30
                StopTime { trip_id: TripId::new("Tr1"), stop_id: StopId::new("A"), stop_sequence: 1, arrival: 8 * 3600, departure: 8 * 3600 },
                StopTime { trip_id: TripId::new("Tr1"), stop_id: StopId::new("B"), stop_sequence: 2, arrival: 8 * 3600 + 1800, departure: 8 * 3600 + 1800 },
                // Tr2: A 08:20 → B 08:50
                StopTime { trip_id: TripId::new("Tr2"), stop_id: StopId::new("A"), stop_sequence: 1, arrival: 8 * 3600 + 1200, departure: 8 * 3600 + 1200 },
                StopTime { trip_id: TripId::new("Tr2"), stop_id: StopId::new("B"), stop_sequence: 2, arrival: 8 * 3600 + 3000, departure: 8 * 3600 + 3000 },
                // Tr3: A 08:40 → B 09:10
                StopTime { trip_id: TripId::new("Tr3"), stop_id: StopId::new("A"), stop_sequence: 1, arrival: 8 * 3600 + 2400, departure: 8 * 3600 + 2400 },
                StopTime { trip_id: TripId::new("Tr3"), stop_id: StopId::new("B"), stop_sequence: 2, arrival: 9 * 3600 + 600, departure: 9 * 3600 + 600 },
            ],
            calendars: vec![Calendar { service_id: otp_core::ServiceId::new("WD"), weekdays: [true, true, true, true, true, false, false], start_date: 20260101, end_date: 20301231 }],
            ..Feed::default()
        };
        Timetable::build(&[feed]).expect("timetable should build")
    }

    #[test]
    fn arrive_by_picks_latest_departure_arriving_by_deadline() {
        let tt = arrive_by_fixture();
        let a = tt.stop_idx(&StopId::new("A")).unwrap();
        let b = tt.stop_idx(&StopId::new("B")).unwrap();

        // 締切 09:00。09:00 までに B へ着ける便は Tr1(08:30着)/Tr2(08:50着)。Tr3(09:10着)は不可。
        // arrive-by は「締切内で最も遅く出発」= Tr2 (08:20発→08:50着) を選ぶはず。
        let query = RaptorQuery {
            access: vec![StreetLink { stop: a, duration_s: 0 }],
            egress: vec![StreetLink { stop: b, duration_s: 0 }],
            earliest_departure: 9 * 3600, // arrive-by では到着締切 T
            service_date: 20260713,
            max_rounds: 2,
            rail_only: false,
            arrive_by: true,
        };
        let journeys = tt.search(&query).expect("search should not error");
        assert!(!journeys.is_empty(), "arrive-by で経路が見つからなかった");
        let best = journeys.last().unwrap();

        // 実到着は締切 09:00 以下。
        assert!(best.arrival_s <= 9 * 3600, "到着 {} は締切 09:00 以下のはず", best.arrival_s);
        // 選ばれた便は Tr2: 08:20発 → 08:50着 (最遅出発)。
        let transit: Vec<_> = best
            .legs
            .iter()
            .filter_map(|l| match l {
                JourneyLeg::Transit { trip_id, board_s, alight_s, .. } => Some((trip_id.as_str().to_string(), *board_s, *alight_s)),
                _ => None,
            })
            .collect();
        assert_eq!(transit.len(), 1, "1便のはず: {transit:?}");
        assert_eq!(transit[0], ("Tr2".to_string(), 8 * 3600 + 1200, 8 * 3600 + 3000), "締切内で最遅出発の Tr2 が選ばれるはず");
        assert_eq!(best.arrival_s, 8 * 3600 + 3000, "到着は 08:50");
    }

    #[test]
    fn arrive_by_and_depart_at_are_consistent_on_same_fixture() {
        let tt = arrive_by_fixture();
        let a = tt.stop_idx(&StopId::new("A")).unwrap();
        let b = tt.stop_idx(&StopId::new("B")).unwrap();

        // 同一フィクスチャで前方探索 (08:00発) は最早の Tr1 (08:00発→08:30着) を返す — arrive-by
        // の Tr2 とは別便を選ぶ。前方探索が arrive_by=false で従来どおり動くことの確認。
        let forward = RaptorQuery {
            access: vec![StreetLink { stop: a, duration_s: 0 }],
            egress: vec![StreetLink { stop: b, duration_s: 0 }],
            earliest_departure: 8 * 3600,
            service_date: 20260713,
            max_rounds: 2,
            rail_only: false,
            arrive_by: false,
        };
        let journeys = tt.search(&forward).expect("search should not error");
        let best = journeys.last().unwrap();
        let transit: Vec<_> = best
            .legs
            .iter()
            .filter_map(|l| match l {
                JourneyLeg::Transit { trip_id, board_s, alight_s, .. } => Some((trip_id.as_str().to_string(), *board_s, *alight_s)),
                _ => None,
            })
            .collect();
        assert_eq!(transit[0], ("Tr1".to_string(), 8 * 3600, 8 * 3600 + 1800), "前方探索は最早の Tr1 を選ぶはず");
    }

    #[test]
    fn arrive_by_respects_egress_walk_when_choosing_trip() {
        // egress 徒歩 15分 を課すと、実到着 = B着 + 15分 が締切以下でなければならない。
        // 締切 09:00・egress 900秒 → B 着は 08:45 以下が必要。Tr2(08:50着)は 09:05 着で不可、
        // Tr1(08:30着 → 08:45着) が選ばれるはず (arrive-by が egress 徒歩を織り込む確認)。
        let tt = arrive_by_fixture();
        let a = tt.stop_idx(&StopId::new("A")).unwrap();
        let b = tt.stop_idx(&StopId::new("B")).unwrap();
        let query = RaptorQuery {
            access: vec![StreetLink { stop: a, duration_s: 0 }],
            egress: vec![StreetLink { stop: b, duration_s: 900 }],
            earliest_departure: 9 * 3600,
            service_date: 20260713,
            max_rounds: 2,
            rail_only: false,
            arrive_by: true,
        };
        let best = tt.search(&query).expect("search should not error").into_iter().last().expect("経路が見つからなかった");
        assert!(best.arrival_s <= 9 * 3600, "実到着 {} は締切以下", best.arrival_s);
        let trip = best
            .legs
            .iter()
            .find_map(|l| match l {
                JourneyLeg::Transit { trip_id, .. } => Some(trip_id.as_str().to_string()),
                _ => None,
            })
            .expect("Transit leg があるはず");
        assert_eq!(trip, "Tr1", "egress 徒歩15分ぶん早い Tr1 が選ばれるはず");
        assert_eq!(best.arrival_s, 8 * 3600 + 1800 + 900, "実到着 = B着08:30 + 徒歩15分 = 08:45");
    }

    #[test]
    fn rail_only_flag_skips_faster_bus_and_returns_slower_rail() {
        use otp_gtfs::{Calendar, Route, RouteType, Stop, StopTime, Trip, WheelchairBoarding};
        // 同じ A→D 区間に「速いバス (08:00→08:10)」と「遅い鉄道 (08:00→08:25)」を用意する。
        // 単一基準 RAPTOR は停留所ごとに最早ラベルしか残さないため、通常探索 (rail_only=false)
        // ではバスが鉄道を上書きして採用される。rail_only=true ではバスへの乗車が丸ごと
        // スキップされ、遅い鉄道が返るはず (バス経路に負けても鉄道の代替を拾える)。
        let feed = Feed {
            stops: vec![
                Stop { id: StopId::new("A"), name: "A".into(), lat: 35.0, lng: 139.0, wheelchair_boarding: WheelchairBoarding::Unknown, parent_station: None, zone_id: None },
                Stop { id: StopId::new("D"), name: "D".into(), lat: 35.05, lng: 139.05, wheelchair_boarding: WheelchairBoarding::Unknown, parent_station: None, zone_id: None },
            ],
            routes: vec![
                Route { id: RouteId::new("RBUS"), agency_id: None, short_name: "急行バス".into(), long_name: "急行バス".into(), route_type: RouteType::Bus },
                Route { id: RouteId::new("RRAIL"), agency_id: None, short_name: "各停".into(), long_name: "各停".into(), route_type: RouteType::Rail },
            ],
            trips: vec![
                Trip { id: TripId::new("TBUS"), route_id: RouteId::new("RBUS"), service_id: otp_core::ServiceId::new("WD"), headsign: None, wheelchair_accessible: WheelchairBoarding::Unknown },
                Trip { id: TripId::new("TRAIL"), route_id: RouteId::new("RRAIL"), service_id: otp_core::ServiceId::new("WD"), headsign: None, wheelchair_accessible: WheelchairBoarding::Unknown },
            ],
            stop_times: vec![
                // バス: A 08:00 → D 08:10 (速い)
                StopTime { trip_id: TripId::new("TBUS"), stop_id: StopId::new("A"), stop_sequence: 1, arrival: 8 * 3600, departure: 8 * 3600 },
                StopTime { trip_id: TripId::new("TBUS"), stop_id: StopId::new("D"), stop_sequence: 2, arrival: 8 * 3600 + 600, departure: 8 * 3600 + 600 },
                // 鉄道: A 08:00 → D 08:25 (遅い)
                StopTime { trip_id: TripId::new("TRAIL"), stop_id: StopId::new("A"), stop_sequence: 1, arrival: 8 * 3600, departure: 8 * 3600 },
                StopTime { trip_id: TripId::new("TRAIL"), stop_id: StopId::new("D"), stop_sequence: 2, arrival: 8 * 3600 + 1500, departure: 8 * 3600 + 1500 },
            ],
            calendars: vec![Calendar { service_id: otp_core::ServiceId::new("WD"), weekdays: [true, true, true, true, true, false, false], start_date: 20260101, end_date: 20301231 }],
            ..Feed::default()
        };
        // バスも含めた時刻表 (RailAndBus) で構築しないと、そもそもバスパターンが載らない。
        let tt = Timetable::build_with_modes(&[feed], ModeFilter::RailAndBus).expect("timetable should build");
        let a = tt.stop_idx(&StopId::new("A")).unwrap();
        let d = tt.stop_idx(&StopId::new("D")).unwrap();

        let base = RaptorQuery {
            access: vec![StreetLink { stop: a, duration_s: 0 }],
            egress: vec![StreetLink { stop: d, duration_s: 0 }],
            earliest_departure: 8 * 3600,
            service_date: 20260713, // 月曜 (WD 運行)
            max_rounds: 2,
            rail_only: false,
            arrive_by: false,
        };

        // rail_only=false: 速いバスが選ばれる。
        let fast = tt.search(&base).expect("search should not error");
        let fast_best = fast.last().expect("expected a fast journey");
        assert_eq!(fast_best.arrival_s, 8 * 3600 + 600, "全モードでは速いバス (08:10着) が選ばれるはず");
        let fast_route = fast_best.legs.iter().find_map(|l| match l {
            JourneyLeg::Transit { route_id, .. } => Some(route_id.as_str().to_string()),
            _ => None,
        });
        assert_eq!(fast_route.as_deref(), Some("RBUS"), "全モードの最速はバス路線のはず");

        // rail_only=true: バスはスキップされ、遅い鉄道が返る。
        let rail = tt.search(&RaptorQuery { rail_only: true, ..base.clone() }).expect("search should not error");
        let rail_best = rail.last().expect("expected a rail journey");
        assert_eq!(rail_best.arrival_s, 8 * 3600 + 1500, "鉄道限定では鉄道 (08:25着) が返るはず");
        let rail_route = rail_best.legs.iter().find_map(|l| match l {
            JourneyLeg::Transit { route_id, route_type, .. } => Some((route_id.as_str().to_string(), *route_type)),
            _ => None,
        });
        assert_eq!(rail_route, Some(("RRAIL".to_string(), RouteType::Rail)), "鉄道限定探索はバスに乗車してはいけない");
    }
}
