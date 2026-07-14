//! RAPTOR による時刻表ベースのマルチモーダル乗換探索。
//!
//! OTP の `raptor` パッケージ相当。`otp-gtfs::Feed` 群からコンパクトな時刻表
//! (`Timetable`) を構築し、ラウンドごとに到達可能停留所を更新する標準 RAPTOR を回す。
//!
//! ## このスライスのスコープ
//! - **鉄道のみ** (`RouteType::is_rail()` = Tram/Subway/Rail)。バスは除外し時刻表を小さく保つ。
//! - 乗換は「同一 `parent_station` または同一 `stop_id`」の0分乗換のみ。近接駅の徒歩乗換
//!   (別駅間) は otp-street 実装後の次スライスで対応する。
//! - access/egress は駅指定 (`StreetLink`) で与える。徒歩ルーティングは otp-street 側の役割。
//! - 複数フィードをまたぐ ID 衝突 (例: 事業者ごとに `service_id="0"` が別サービスを指す) は
//!   未対応。今スライスの実データ検証は単一フィード (都営地下鉄) のみで行う。

use std::collections::HashMap;

use otp_core::{Result, RouteId, SecondsSinceMidnight, StopId, TripId};
use otp_gtfs::Feed;

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

/// RAPTOR 用に詰めた時刻表。
#[derive(Debug, Default)]
pub struct Timetable {
    /// StopIdx → 正規化された StopId (`parent_station` があればそれ、無ければ自身)。
    pub stop_ids: Vec<StopId>,
    /// 生の stop_id (プラットフォーム含む) と正規化後 StopId の両方から引ける索引。
    stop_lookup: HashMap<StopId, StopIdx>,
    pub patterns: Vec<Pattern>,
    /// StopIdx → そのパターン内での出現位置一覧。
    stop_patterns: Vec<Vec<(PatternIdx, u32)>>,
    calendars: HashMap<otp_core::ServiceId, ServiceCalendar>,
}

impl Timetable {
    /// `otp-gtfs::Feed` 群から鉄道のみの時刻表を構築する。
    pub fn build(feeds: &[Feed]) -> Result<Timetable> {
        // 1. 停留所の正規化 (parent_station への集約) と StopIdx 割り当て。
        let mut stop_ids: Vec<StopId> = Vec::new();
        let mut stop_lookup: HashMap<StopId, StopIdx> = HashMap::new();
        let mut canonical_of: HashMap<StopId, StopId> = HashMap::new();

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
            }
        }

        // 2. 鉄道路線のみ抽出 (route_id → route の索引をフィードごとに作る)。
        let mut rail_routes: HashMap<RouteId, &otp_gtfs::Route> = HashMap::new();
        for feed in feeds {
            for route in &feed.routes {
                if route.route_type.is_rail() {
                    rail_routes.insert(route.id.clone(), route);
                }
            }
        }

        // 3. trip_id → route の索引、service_id の索引。
        let mut trip_route: HashMap<TripId, &otp_gtfs::Trip> = HashMap::new();
        for feed in feeds {
            for trip in &feed.trips {
                if rail_routes.contains_key(&trip.route_id) {
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
                let route = rail_routes[&trip.route_id];
                patterns.push(Pattern {
                    route_id: trip.route_id.clone(),
                    route_short_name: if route.short_name.is_empty() { route.long_name.clone() } else { route.short_name.clone() },
                    stops,
                    trips: Vec::new(),
                });
                (patterns.len() - 1) as PatternIdx
            });
            patterns[pattern_idx as usize].trips.push(PatternTrip {
                trip_id: trip_id.clone(),
                service_id: trip.service_id.clone(),
                arrivals,
                departures,
            });
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

        Ok(Timetable { stop_ids, stop_lookup, patterns, stop_patterns, calendars })
    }

    /// GTFS 生 stop_id (プラットフォーム含む) または正規化済み StopId から StopIdx を引く。
    pub fn stop_idx(&self, stop_id: &StopId) -> Option<StopIdx> {
        self.stop_lookup.get(stop_id).copied()
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
}

/// 探索結果の1区間。
#[derive(Debug, Clone)]
pub enum JourneyLeg {
    Walk { stop: StopIdx, duration_s: u32 },
    Transit {
        route_id: RouteId,
        route_short_name: String,
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
    /// パターン `pattern` の便に `board_stop` (パターン内位置 `board_pos`) で乗車し、
    /// `alight_pos` で降りた。乗車時に参照した「1ラウンド前のラベル」が属していたラウンドを
    /// `prev_round` に保持し、遡上時にどのラウンドを見ればよいか自己完結させる。
    Board { pattern: PatternIdx, trip_idx: usize, board_stop: StopIdx, board_pos: usize, alight_pos: usize, prev_round: usize },
}

const INF: i64 = i64::MAX;

impl Timetable {
    /// RAPTOR 探索本体。到着時刻と乗換回数で Pareto 最適な複数経路を返す。
    /// 見つからなければ空 Vec (エラーではない: 「その日は運行が無い」等の正常系)。
    pub fn search(&self, query: &RaptorQuery) -> Result<Vec<Journey>> {
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

                    let prev_arrival = prev[stop as usize].arrival;
                    if prev_arrival != INF {
                        if let Some(candidate) = earliest_catchable_trip(pattern, pos, prev_arrival, query.service_date, self) {
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
                        trip_id: trip.trip_id.clone(),
                        from: board_stop,
                        to: cur_stop,
                        board_s: trip.departures[board_pos],
                        alight_s: trip.arrivals[alight_pos],
                    });
                    cur_stop = board_stop;
                    cur_round = prev_round;
                }
                Some(Parent::Access) => {
                    if let Some(link) = query.access.iter().find(|l| l.stop == cur_stop) {
                        if link.duration_s > 0 {
                            legs_rev.push(JourneyLeg::Walk { stop: cur_stop, duration_s: link.duration_s });
                        }
                    }
                    break;
                }
                None => break, // 到達不能ラベルの遡上 (通常発生しない防御的分岐)
            }
        }

        legs_rev.reverse();
        legs_rev
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
    fn weekday_index_matches_known_dates() {
        // 2026-07-13 は月曜日、2026-07-14 は火曜日 (実測: python datetime で確認)。
        assert_eq!(weekday_index(20260713), 0);
        assert_eq!(weekday_index(20260714), 1);
        assert_eq!(weekday_index(20260719), 6); // 日曜日
        assert_eq!(weekday_index(20260720), 0); // 月曜日
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
        };
        let journeys = tt.search(&query).expect("search should not error");
        assert!(journeys.is_empty(), "expected no journey when nothing is running, got {journeys:?}");
    }
}
