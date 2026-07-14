//! GTFS 取り込みと交通モデル。運賃データ (GTFS-Fares v1) を含む。
//!
//! OTP の `gtfs` / `transit.model` 相当。ここでは「生の GTFS に近いドメイン型」を持ち、
//! RAPTOR 用のコンパクトな時刻表 (`otp-raptor`) はここから構築する。
//!
//! 実測 (babymobi infra/otp/data): 都営地下鉄・東京メトロ・りんかい線・京王・東武・都営バスの
//! GTFS は `fare_attributes.txt` + `fare_rules.txt` (GTFS-Fares v1) を持つ。自前頻度 GTFS
//! (JR) は運賃なし → JR は距離制運賃を別途モデル化する (otp-fares 参照)。

use otp_core::{AgencyId, FareId, Result, RouteId, SecondsSinceMidnight, ServiceId, StopId, TripId};

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
    /// TODO(移植): CSV パーサ (引用符・BOM・エンコーディング) を実装し各テーブルを埋める。
    /// まずは stops/routes/trips/stop_times/calendar と fare_attributes/fare_rules を対象にする。
    pub fn load_from_dir(_dir: &std::path::Path) -> Result<Feed> {
        Err(otp_core::Error::Unimplemented("Feed::load_from_dir"))
    }
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
