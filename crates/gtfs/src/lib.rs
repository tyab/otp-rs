//! GTFS 取り込みと交通モデル。運賃データ (GTFS-Fares v1) を含む。
//!
//! OTP の `gtfs` / `transit.model` 相当。ここでは「生の GTFS に近いドメイン型」を持ち、
//! RAPTOR 用のコンパクトな時刻表 (`otp-raptor`) はここから構築する。
//!
//! 実測 (babymobi infra/otp/data): 都営地下鉄・東京メトロ・りんかい線・京王・東武・都営バスの
//! GTFS は `fare_attributes.txt` + `fare_rules.txt` (GTFS-Fares v1) を持つ。自前頻度 GTFS
//! (JR) は運賃なし → JR は距離制運賃を別途モデル化する (otp-fares 参照)。

use std::collections::HashMap;

use otp_core::{AgencyId, Error, FareId, Result, RouteId, SecondsSinceMidnight, ServiceId, StopId, TripId};

mod csv;
use csv::{Row, Table};

/// バリアフリー乗車可否 (GTFS `wheelchair_boarding`)。
///
/// 東京の多くのフィードでは値が空 = `Unknown`。アクセシビリティ経路では
/// Unknown をペナルティ付きで通す (OTP の unknownCost 相当、otp-street 参照)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WheelchairBoarding {
    #[default]
    Unknown,
    Accessible,
    NotAccessible,
}

/// 停留所/駅。
#[derive(Debug, Clone)]
pub struct Stop {
    pub id: StopId,
    pub name: String,
    pub lat: f64,
    pub lng: f64,
    pub wheelchair_boarding: WheelchairBoarding,
    /// 親駅 (`parent_station`)。ホーム→駅の集約に使う。
    pub parent_station: Option<StopId>,
}

/// 路線種別 (GTFS `route_type` の必要分)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RouteType {
    Tram,
    Subway,
    Rail,
    Bus,
    Other(u16),
}

impl RouteType {
    pub fn from_gtfs(code: u16) -> Self {
        match code {
            0 => RouteType::Tram,
            1 => RouteType::Subway,
            2 => RouteType::Rail,
            3 => RouteType::Bus,
            other => RouteType::Other(other),
        }
    }

    /// otp-raptor が「鉄道のみ」の時刻表を組むときに使う判定。
    pub fn is_rail(self) -> bool {
        matches!(self, RouteType::Tram | RouteType::Subway | RouteType::Rail)
    }
}

/// 路線。
#[derive(Debug, Clone)]
pub struct Route {
    pub id: RouteId,
    pub agency_id: Option<AgencyId>,
    pub short_name: String,
    pub long_name: String,
    pub route_type: RouteType,
}

/// 便 (1本の運行)。
#[derive(Debug, Clone)]
pub struct Trip {
    pub id: TripId,
    pub route_id: RouteId,
    pub service_id: ServiceId,
    pub headsign: Option<String>,
    /// 車両バリアフリー可否 (`wheelchair_accessible`)。
    pub wheelchair_accessible: WheelchairBoarding,
}

/// 停車時刻 (`stop_times.txt` の1行)。
#[derive(Debug, Clone)]
pub struct StopTime {
    pub trip_id: TripId,
    pub stop_id: StopId,
    pub stop_sequence: u32,
    pub arrival: SecondsSinceMidnight,
    pub departure: SecondsSinceMidnight,
}

/// 運行日 (`calendar.txt`)。曜日ビットと有効期間。
#[derive(Debug, Clone)]
pub struct Calendar {
    pub service_id: ServiceId,
    /// [月,火,水,木,金,土,日]
    pub weekdays: [bool; 7],
    pub start_date: u32, // YYYYMMDD
    pub end_date: u32,
}

/// 例外日 (`calendar_dates.txt`)。
#[derive(Debug, Clone)]
pub struct CalendarDate {
    pub service_id: ServiceId,
    pub date: u32, // YYYYMMDD
    /// true=運行追加, false=運休。
    pub added: bool,
}

/// 運賃属性 (`fare_attributes.txt`, GTFS-Fares v1)。
#[derive(Debug, Clone)]
pub struct FareAttribute {
    pub fare_id: FareId,
    /// 運賃額 (通貨最小単位でなく `currency_type` に従う。日本は円)。
    pub price: f64,
    pub currency_type: String,
    /// 乗換許可回数 (None=無制限)。
    pub transfers: Option<u8>,
    /// 乗換有効時間 (秒)。
    pub transfer_duration: Option<u32>,
}

/// 運賃規則 (`fare_rules.txt`, GTFS-Fares v1)。
///
/// 日本の距離制運賃は主に `origin_id`/`destination_id` (運賃ゾーン間) で表現される。
#[derive(Debug, Clone)]
pub struct FareRule {
    pub fare_id: FareId,
    pub route_id: Option<RouteId>,
    pub origin_id: Option<String>,
    pub destination_id: Option<String>,
    pub contains_id: Option<String>,
}

/// 1事業者フィードを読み込んだ集約 (生 GTFS に近い形)。
#[derive(Debug, Default)]
pub struct Feed {
    pub stops: Vec<Stop>,
    pub routes: Vec<Route>,
    pub trips: Vec<Trip>,
    pub stop_times: Vec<StopTime>,
    pub calendars: Vec<Calendar>,
    pub calendar_dates: Vec<CalendarDate>,
    pub fare_attributes: Vec<FareAttribute>,
    pub fare_rules: Vec<FareRule>,
}

impl Feed {
    /// GTFS ディレクトリ (解凍済み) を読み込む。
    ///
    /// 欠損ファイルは空 Vec で許容する (`calendar.txt` / `calendar_dates.txt` は
    /// 片方だけあっても、両方無くても可)。列は見出し名で引くため列順には依存しない。
    pub fn load_from_dir(dir: &std::path::Path) -> Result<Feed> {
        let stops = load_table(dir, "stops.txt")?;
        let routes = load_table(dir, "routes.txt")?;
        let trips = load_table(dir, "trips.txt")?;
        let stop_times = load_table(dir, "stop_times.txt")?;
        let calendars = load_table(dir, "calendar.txt")?;
        let calendar_dates = load_table(dir, "calendar_dates.txt")?;
        let fare_attributes = load_table(dir, "fare_attributes.txt")?;
        let fare_rules = load_table(dir, "fare_rules.txt")?;

        Ok(Feed {
            stops: parse_stops(&stops)?,
            routes: parse_routes(&routes)?,
            trips: parse_trips(&trips)?,
            stop_times: parse_stop_times(&stop_times)?,
            calendars: parse_calendars(&calendars)?,
            calendar_dates: parse_calendar_dates(&calendar_dates)?,
            fare_attributes: parse_fare_attributes(&fare_attributes)?,
            fare_rules: parse_fare_rules(&fare_rules)?,
        })
    }
}

/// GTFS ディレクトリから1テーブルを読む。ファイルが存在しなければ空テーブル
/// (呼び出し側は自然に空 Vec を得る) を返す。
fn load_table(dir: &std::path::Path, filename: &str) -> Result<Table> {
    let path = dir.join(filename);
    match std::fs::read_to_string(&path) {
        Ok(content) => Ok(Table::parse(&content)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Table::empty()),
        Err(e) => Err(Error::Io(e)),
    }
}

fn required<'a>(row: &Row<'a>, file: &str, i: usize, col: &str) -> Result<&'a str> {
    row.get(col).ok_or_else(|| Error::Parse(format!("{file} row {i}: missing required column `{col}`")))
}

/// GTFS の "H:MM:SS" / "HH:MM:SS" 形式 (24時超も許容) を秒に変換する。
pub fn parse_gtfs_time(s: &str) -> Result<SecondsSinceMidnight> {
    let parts: Vec<&str> = s.trim().split(':').collect();
    if parts.len() != 3 {
        return Err(Error::Parse(format!("invalid GTFS time: {s:?}")));
    }
    let parse_part = |p: &str| -> Result<i32> {
        p.parse::<i32>().map_err(|_| Error::Parse(format!("invalid GTFS time component: {s:?}")))
    };
    let h = parse_part(parts[0])?;
    let m = parse_part(parts[1])?;
    let sec = parse_part(parts[2])?;
    if h < 0 || !(0..60).contains(&m) || !(0..60).contains(&sec) {
        return Err(Error::Parse(format!("GTFS time out of range: {s:?}")));
    }
    Ok(h * 3600 + m * 60 + sec)
}

fn parse_wheelchair(code: Option<&str>) -> WheelchairBoarding {
    match code {
        Some("1") => WheelchairBoarding::Accessible,
        Some("2") => WheelchairBoarding::NotAccessible,
        _ => WheelchairBoarding::Unknown,
    }
}

fn parse_stops(t: &Table) -> Result<Vec<Stop>> {
    t.iter()
        .enumerate()
        .map(|(i, row)| {
            let id = required(&row, "stops.txt", i, "stop_id")?;
            let lat = row
                .get("stop_lat")
                .map(|v| v.parse::<f64>().map_err(|_| Error::Parse(format!("stops.txt row {i}: bad stop_lat {v:?}"))))
                .transpose()?
                .unwrap_or(0.0);
            let lng = row
                .get("stop_lon")
                .map(|v| v.parse::<f64>().map_err(|_| Error::Parse(format!("stops.txt row {i}: bad stop_lon {v:?}"))))
                .transpose()?
                .unwrap_or(0.0);
            Ok(Stop {
                id: StopId::new(id),
                name: row.get("stop_name").unwrap_or("").to_string(),
                lat,
                lng,
                wheelchair_boarding: parse_wheelchair(row.get("wheelchair_boarding")),
                parent_station: row.get("parent_station").map(StopId::new),
            })
        })
        .collect()
}

fn parse_routes(t: &Table) -> Result<Vec<Route>> {
    t.iter()
        .enumerate()
        .map(|(i, row)| {
            let id = required(&row, "routes.txt", i, "route_id")?;
            let route_type_code: u16 = required(&row, "routes.txt", i, "route_type")?
                .parse()
                .map_err(|_| Error::Parse(format!("routes.txt row {i}: bad route_type")))?;
            Ok(Route {
                id: RouteId::new(id),
                agency_id: row.get("agency_id").map(AgencyId::new),
                short_name: row.get("route_short_name").unwrap_or("").to_string(),
                long_name: row.get("route_long_name").unwrap_or("").to_string(),
                route_type: RouteType::from_gtfs(route_type_code),
            })
        })
        .collect()
}

fn parse_trips(t: &Table) -> Result<Vec<Trip>> {
    t.iter()
        .enumerate()
        .map(|(i, row)| {
            let id = required(&row, "trips.txt", i, "trip_id")?;
            let route_id = required(&row, "trips.txt", i, "route_id")?;
            let service_id = required(&row, "trips.txt", i, "service_id")?;
            Ok(Trip {
                id: TripId::new(id),
                route_id: RouteId::new(route_id),
                service_id: ServiceId::new(service_id),
                headsign: row.get("trip_headsign").map(str::to_string),
                wheelchair_accessible: parse_wheelchair(row.get("wheelchair_accessible")),
            })
        })
        .collect()
}

/// 補間前の1行。GTFS は `timepoint=0` の中間停車を arrival/departure 空欄で表現でき
/// (実測: 都営地下鉄 GTFS で 540/122799 行が該当)、その場合は前後の確定時刻から
/// 均等割りで線形補間する (GTFS の一般的な読み込み慣習に倣う)。
struct RawStopTime {
    trip_id: TripId,
    stop_id: StopId,
    stop_sequence: u32,
    arrival: Option<SecondsSinceMidnight>,
    departure: Option<SecondsSinceMidnight>,
}

fn parse_stop_times(t: &Table) -> Result<Vec<StopTime>> {
    let mut raw: Vec<RawStopTime> = t
        .iter()
        .enumerate()
        .map(|(i, row)| {
            let trip_id = required(&row, "stop_times.txt", i, "trip_id")?;
            let stop_id = required(&row, "stop_times.txt", i, "stop_id")?;
            let stop_sequence: u32 = required(&row, "stop_times.txt", i, "stop_sequence")?
                .parse()
                .map_err(|_| Error::Parse(format!("stop_times.txt row {i}: bad stop_sequence")))?;
            let arrival = row.get("arrival_time").map(parse_gtfs_time).transpose()?;
            let departure = row.get("departure_time").map(parse_gtfs_time).transpose()?;
            // 片方だけ空欄なら、確定している方をもう片方にも写す (両方 None の場合だけ
            // 後段の補間対象として残す)。
            let (arrival, departure) = match (arrival, departure) {
                (Some(a), None) => (Some(a), Some(a)),
                (None, Some(d)) => (Some(d), Some(d)),
                other => other,
            };
            Ok(RawStopTime { trip_id: TripId::new(trip_id), stop_id: StopId::new(stop_id), stop_sequence, arrival, departure })
        })
        .collect::<Result<Vec<_>>>()?;

    let mut by_trip: HashMap<String, Vec<usize>> = HashMap::new();
    for (idx, r) in raw.iter().enumerate() {
        by_trip.entry(r.trip_id.0.clone()).or_default().push(idx);
    }
    for idxs in by_trip.values_mut() {
        idxs.sort_by_key(|&i| raw[i].stop_sequence);
        interpolate_missing_times(&mut raw, idxs)?;
    }

    Ok(raw
        .into_iter()
        .map(|r| {
            let arrival = r.arrival.expect("interpolate_missing_times fills all entries");
            let departure = r.departure.expect("interpolate_missing_times fills all entries");
            StopTime { trip_id: r.trip_id, stop_id: r.stop_id, stop_sequence: r.stop_sequence, arrival, departure }
        })
        .collect())
}

/// `idxs` (同一 trip、stop_sequence 昇順) に沿って、両方 None の行を前後の確定値から
/// 均等割り線形補間で埋める。先頭/末尾が未確定のままだと補間できないためエラーにする。
fn interpolate_missing_times(raw: &mut [RawStopTime], idxs: &[usize]) -> Result<()> {
    let n = idxs.len();
    let mut i = 0;
    while i < n {
        if raw[idxs[i]].arrival.is_some() {
            i += 1;
            continue;
        }
        let mut j = i;
        while j < n && raw[idxs[j]].arrival.is_none() {
            j += 1;
        }
        if i == 0 || j == n {
            return Err(Error::Parse(format!(
                "stop_times.txt: trip {} の先頭または末尾の停車時刻が未確定で補間できない",
                raw[idxs[i]].trip_id
            )));
        }
        let t0 = raw[idxs[i - 1]].departure.expect("known anchor");
        let t1 = raw[idxs[j]].arrival.expect("known anchor");
        let gap = (j - i) as i64 + 1;
        for (k, &m) in idxs[i..j].iter().enumerate() {
            let frac = (k as i64 + 1) * (t1 - t0) as i64 / gap;
            let v = (t0 as i64 + frac) as SecondsSinceMidnight;
            raw[m].arrival = Some(v);
            raw[m].departure = Some(v);
        }
        i = j;
    }
    Ok(())
}

fn parse_calendars(t: &Table) -> Result<Vec<Calendar>> {
    t.iter()
        .enumerate()
        .map(|(i, row)| {
            let service_id = required(&row, "calendar.txt", i, "service_id")?;
            let day = |col: &str| -> Result<bool> {
                match row.get(col) {
                    Some("1") => Ok(true),
                    Some("0") | None => Ok(false),
                    Some(other) => Err(Error::Parse(format!("calendar.txt row {i}: bad {col} value {other:?}"))),
                }
            };
            let weekdays = [
                day("monday")?,
                day("tuesday")?,
                day("wednesday")?,
                day("thursday")?,
                day("friday")?,
                day("saturday")?,
                day("sunday")?,
            ];
            let start_date: u32 = required(&row, "calendar.txt", i, "start_date")?
                .parse()
                .map_err(|_| Error::Parse(format!("calendar.txt row {i}: bad start_date")))?;
            let end_date: u32 = required(&row, "calendar.txt", i, "end_date")?
                .parse()
                .map_err(|_| Error::Parse(format!("calendar.txt row {i}: bad end_date")))?;
            Ok(Calendar { service_id: ServiceId::new(service_id), weekdays, start_date, end_date })
        })
        .collect()
}

fn parse_calendar_dates(t: &Table) -> Result<Vec<CalendarDate>> {
    t.iter()
        .enumerate()
        .map(|(i, row)| {
            let service_id = required(&row, "calendar_dates.txt", i, "service_id")?;
            let date: u32 = required(&row, "calendar_dates.txt", i, "date")?
                .parse()
                .map_err(|_| Error::Parse(format!("calendar_dates.txt row {i}: bad date")))?;
            let added = match required(&row, "calendar_dates.txt", i, "exception_type")? {
                "1" => true,
                "2" => false,
                other => {
                    return Err(Error::Parse(format!(
                        "calendar_dates.txt row {i}: bad exception_type {other:?}"
                    )))
                }
            };
            Ok(CalendarDate { service_id: ServiceId::new(service_id), date, added })
        })
        .collect()
}

fn parse_fare_attributes(t: &Table) -> Result<Vec<FareAttribute>> {
    t.iter()
        .enumerate()
        .map(|(i, row)| {
            let fare_id = required(&row, "fare_attributes.txt", i, "fare_id")?;
            let price: f64 = required(&row, "fare_attributes.txt", i, "price")?
                .parse()
                .map_err(|_| Error::Parse(format!("fare_attributes.txt row {i}: bad price")))?;
            let transfers = row
                .get("transfers")
                .map(|v| v.parse::<u8>().map_err(|_| Error::Parse(format!("fare_attributes.txt row {i}: bad transfers"))))
                .transpose()?;
            let transfer_duration = row
                .get("transfer_duration")
                .map(|v| {
                    v.parse::<u32>()
                        .map_err(|_| Error::Parse(format!("fare_attributes.txt row {i}: bad transfer_duration")))
                })
                .transpose()?;
            Ok(FareAttribute {
                fare_id: FareId::new(fare_id),
                price,
                currency_type: row.get("currency_type").unwrap_or("").to_string(),
                transfers,
                transfer_duration,
            })
        })
        .collect()
}

fn parse_fare_rules(t: &Table) -> Result<Vec<FareRule>> {
    t.iter()
        .enumerate()
        .map(|(i, row)| {
            let fare_id = required(&row, "fare_rules.txt", i, "fare_id")?;
            Ok(FareRule {
                fare_id: FareId::new(fare_id),
                route_id: row.get("route_id").map(RouteId::new),
                origin_id: row.get("origin_id").map(str::to_string),
                destination_id: row.get("destination_id").map(str::to_string),
                contains_id: row.get("contains_id").map(str::to_string),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn route_type_mapping() {
        assert_eq!(RouteType::from_gtfs(1), RouteType::Subway);
        assert_eq!(RouteType::from_gtfs(2), RouteType::Rail);
        assert_eq!(RouteType::from_gtfs(3), RouteType::Bus);
        assert_eq!(RouteType::from_gtfs(11), RouteType::Other(11));
    }

    #[test]
    fn wheelchair_default_is_unknown() {
        assert_eq!(WheelchairBoarding::default(), WheelchairBoarding::Unknown);
    }
}
