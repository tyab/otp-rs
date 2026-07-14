//! 運賃計算。まず GTFS-Fares v1 (`fare_attributes` + `fare_rules`) に対応する。
//!
//! OTP の `ext.fares` 相当。日本の鉄道運賃は距離制で、GTFS では主に
//! `fare_rules` の `origin_id`/`destination_id` (運賃ゾーン間) で表現される。
//! 自前頻度 GTFS の JR は運賃データを持たないため、距離制運賃表を別途与える
//! 拡張ポイント (`FareModel`) を用意する。
//!
//! 実測 (babymobi infra/otp/data, 2026-07-15): 都営/メトロ/りんかい/京王/東武の
//! 鉄道 GTFS はいずれも `fare_rules` が `origin_id`/`destination_id` のみを使い
//! (`route_id`/`contains_id` は全行空)、駅ペアごとに一意な `fare_id` を持つ
//! (同一 origin/destination ペアが複数 fare_id にまたがる例は0件)。都営バス GTFS は
//! 逆に `route_id` のみを使い (1 fare_id が複数 route_id 行を共有 = 均一運賃)、
//! `contains_id` はどのフィードにも使用例が無い (未検証の分岐、下記参照)。
//!
//! ## 本家 OTP との突き合わせ (2026-07-15, ローカル OTP2 `:8080`, `planConnection`
//! の `legs.fareProducts`)
//! - 都営単一区間 (新宿西口→本郷三丁目, 乗換無し): OTP `6:220` = 220円。
//!   otp-rs: `fare_rules` (origin=402,destination=409) → `fare_id=220` → 220円。**一致**。
//! - 都営単一事業者・乗換1回 (新宿→都庁前→本郷三丁目, 大江戸線ループ⇄放射区間):
//!   OTP は2つの leg 両方に **同一の** `fareProductUse` (id `c1e31cc5-...`,
//!   product `6:220`) を紐付けている (2つのlegに220円ずつ課金されるのではなく、
//!   全体で1個の220円運賃)。otp-rs: `total_fare` が同一フィードの連続する乗車 Leg を
//!   1グループにまとめ、先頭の乗車駅 (428) と末尾の降車駅 (409) の運賃ゾーンで
//!   1回だけ引く → 220円。**一致**。
//! - 事業者跨ぎ (六本木一丁目→白金高輪→三田, メトロ南北線→都営三田線):
//!   OTP は2つの異なる `fareProductUse` (`3:178`, `6:178`) を返す → 合計356円。
//!   otp-rs: フィードが変わったところでグループを区切り、メトロ側 (178円) + 都営側
//!   (178円) を合算 → 356円。**一致**。

use std::collections::HashMap;

use otp_core::{Error, FareId, Result};
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

/// GTFS-Fares v1 の運賃エンジン。フィード単位で保持する (otp-engine のモジュールdoc、
/// および `otp_gtfs::Feed::namespace` のコメント参照: `zone_id`/`fare_rules` の
/// origin_id/destination_id/route_id は全て同じ名前空間規約で前置されているため、
/// 1つの `FareModel` が扱う `zone_id` 文字列は常にその1フィード内で閉じている)。
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

    /// 1区間に適用される運賃を規則から探す。
    ///
    /// ## 一致規則 (GTFS-Fares v1 仕様に基づく採用ルール)
    /// `fare_rules.txt` の行を `fare_id` ごとにグルーピングし、行の種類で2通りに扱う:
    ///
    /// 1. **OD/route 行** (`route_id`/`origin_id`/`destination_id` のいずれかを持つ行。
    ///    `contains_id` のみの行を除く): 行内は AND (未指定フィールドはワイルドカード)、
    ///    同一 `fare_id` の複数行は OR。都営バスの「1 fare_id に複数 route_id 行」
    ///    (実測) はこの OR で表現される。鉄道各社の「1 fare_id に1 (origin,destination)
    ///    行」もこの分岐で処理する。
    /// 2. **contains 行** (`route_id`/`origin_id`/`destination_id` を持たず
    ///    `contains_id` のみの行): 同一 `fare_id` の行の `contains_id` を集めた集合を
    ///    その fare_id の「ゾーン集合」とし、`leg.contains_zones` が空でなくその集合の
    ///    部分集合なら一致とする (GTFS 仕様の記述「trip has to take place within the
    ///    listed zones」に基づく解釈)。**実データに `contains_id` の使用例が無いため
    ///    未検証** (下記テストはこの分岐用の手組みフィクスチャで検証済みだが、本家 OTP
    ///    との突き合わせはできていない)。
    ///
    /// 複数 `fare_id` が一致した場合の優先順位は GTFS 仕様に明記が無いため、保守的に
    /// **最安値を採用する** (実データでは origin/destination ペアが一意に1 `fare_id` へ
    /// 写るため, 都営/メトロ/りんかい/京王/東武いずれもこの分岐は発生しない。実測で確認済み)。
    pub fn fare_for_leg(&self, leg: &FareLeg) -> Option<&FareAttribute> {
        if self.rules.is_empty() {
            return None;
        }
        let mut by_fare: HashMap<&FareId, Vec<&FareRule>> = HashMap::new();
        for r in &self.rules {
            by_fare.entry(&r.fare_id).or_default().push(r);
        }

        let mut best: Option<&FareAttribute> = None;
        for (fare_id, rows) in &by_fare {
            if !rule_group_matches(rows, leg) {
                continue;
            }
            let Some(attr) = self.attributes.iter().find(|a| &a.fare_id == *fare_id) else {
                continue; // fare_attributes に対応行が無い (データ不整合、素直に無視)
            };
            best = Some(match best {
                None => attr,
                Some(cur) if attr.price < cur.price => attr,
                Some(cur) => cur,
            });
        }
        best
    }

    /// 経路全体 (複数区間) の合計運賃を計算する。
    ///
    /// ## 設計: 「フィードごとに1回の通し運賃」
    /// この `FareModel` が扱う `legs` は、呼び出し側 (otp-engine) が**同一フィード
    /// (同一事業者) の連続する乗車区間だけ**を渡す前提とする (otp-engine モジュールdoc
    /// 参照)。日本の鉄道運賃は「乗車駅→降車駅」の1枚の切符で事業者内の乗換を含む距離制
    /// 運賃であり (本家 OTP の実測でも、同一事業者内の乗換を挟む2 leg に対して
    /// **同一の1個の運賃product** が紐付く。モジュールdoc の「都営単一事業者・乗換1回」
    /// 参照)、区間ごとに `fare_for_leg` を呼んで単純合算すると多重課金になる。
    /// そのため、`legs` 全体を「先頭の乗車ゾーン→末尾の降車ゾーン」の1区間に潰して
    /// 1回だけ `fare_for_leg` を呼ぶ。事業者を跨ぐ場合の合算は otp-engine 側の責務
    /// (フィードごとに `total_fare` を呼んで結果を合算する)。
    ///
    /// `origin_zone`/`destination_zone` のいずれかが無い (`zone_id` を持たないフィード、
    /// または otp-engine 側でゾーンを解決できなかった区間) 場合は運賃算出不可として
    /// 明示的に `Err` を返す (ワイルドカード一致で誤った運賃を返すことを避ける)。
    pub fn total_fare(&self, legs: &[FareLeg]) -> Result<Money> {
        if legs.is_empty() {
            return Ok(Money { amount: 0.0 });
        }
        let origin = legs[0].origin_zone.clone();
        let destination = legs[legs.len() - 1].destination_zone.clone();
        let (Some(origin), Some(destination)) = (origin, destination) else {
            return Err(Error::NotFound(
                "fare zone (zone_id) missing for leg; cannot compute through-fare".to_string(),
            ));
        };

        // contains_id ベースの一致 (未検証, fare_for_leg のdoc参照) 用に、区間が
        // 通過する全ゾーン (各legのorigin/destination + 明示的なcontains_zones) を集める。
        let contains_zones: Vec<String> = legs
            .iter()
            .flat_map(|l| {
                let mut zs = l.contains_zones.clone();
                zs.extend(l.origin_zone.clone());
                zs.extend(l.destination_zone.clone());
                zs
            })
            .collect();

        let combined = FareLeg {
            // 通し運賃は route_id 非依存 (複数路線をまたぎうる) なのでワイルドカード。
            route_id: None,
            origin_zone: Some(origin),
            destination_zone: Some(destination),
            contains_zones,
        };

        self.fare_for_leg(&combined).map(|attr| Money { amount: attr.price }).ok_or_else(|| {
            Error::NotFound(format!(
                "no matching GTFS-Fares v1 rule for origin={:?} destination={:?}",
                combined.origin_zone, combined.destination_zone
            ))
        })
    }
}

/// 同一 `fare_id` に属す `fare_rules` 行 (`rows`) が `leg` に一致するか判定する。
/// `fare_for_leg` のdoc参照。
fn rule_group_matches(rows: &[&FareRule], leg: &FareLeg) -> bool {
    let mut any_od_row = false;
    let mut od_matched = false;
    let mut any_contains_row = false;
    let mut contains_set: Vec<&str> = Vec::new();

    for r in rows {
        let is_contains_only = r.contains_id.is_some() && r.route_id.is_none() && r.origin_id.is_none() && r.destination_id.is_none();
        if is_contains_only {
            any_contains_row = true;
            contains_set.push(r.contains_id.as_deref().expect("checked Some above"));
        } else {
            any_od_row = true;
            let route_ok = r.route_id.as_ref().is_none_or(|rid| leg.route_id.as_ref() == Some(rid));
            let origin_ok = r.origin_id.as_deref().is_none_or(|o| leg.origin_zone.as_deref() == Some(o));
            let dest_ok = r.destination_id.as_deref().is_none_or(|d| leg.destination_zone.as_deref() == Some(d));
            if route_ok && origin_ok && dest_ok {
                od_matched = true;
            }
        }
    }

    let contains_ok =
        any_contains_row && !leg.contains_zones.is_empty() && leg.contains_zones.iter().all(|z| contains_set.contains(&z.as_str()));

    (any_od_row && od_matched) || contains_ok
}

/// JR のような GTFS 運賃を持たない事業者向けの距離制運賃フック (完全移植で本実装)。
pub trait DistanceFare {
    fn fare_for_distance(&self, km: f64) -> Money;
}

#[cfg(test)]
mod tests {
    use super::*;
    use otp_core::RouteId;

    fn attr(fare_id: &str, price: f64) -> FareAttribute {
        FareAttribute { fare_id: FareId::new(fare_id), price, currency_type: "JPY".to_string(), transfers: None, transfer_duration: None }
    }

    fn od_rule(fare_id: &str, origin: &str, destination: &str) -> FareRule {
        FareRule { fare_id: FareId::new(fare_id), route_id: None, origin_id: Some(origin.to_string()), destination_id: Some(destination.to_string()), contains_id: None }
    }

    fn route_rule(fare_id: &str, route: &str) -> FareRule {
        FareRule { fare_id: FareId::new(fare_id), route_id: Some(RouteId::new(route)), origin_id: None, destination_id: None, contains_id: None }
    }

    fn contains_rule(fare_id: &str, zone: &str) -> FareRule {
        FareRule { fare_id: FareId::new(fare_id), route_id: None, origin_id: None, destination_id: None, contains_id: Some(zone.to_string()) }
    }

    /// 都営実データを模した小フィクスチャ: 駅ペア (origin,destination) ごとに
    /// 一意な fare_id を持つ距離制運賃。
    fn zone_fare_model() -> FareModel {
        FareModel {
            attributes: vec![attr("220", 220.0), attr("178", 178.0), attr("168", 168.0)],
            rules: vec![od_rule("220", "402", "409"), od_rule("220", "428", "409"), od_rule("178", "203", "204"), od_rule("168", "600", "600")],
        }
    }

    #[test]
    fn fare_for_leg_matches_by_origin_destination_zone() {
        let model = zone_fare_model();
        let leg = FareLeg { route_id: None, origin_zone: Some("402".into()), destination_zone: Some("409".into()), contains_zones: vec![] };
        let fare = model.fare_for_leg(&leg).expect("should match 220円 rule");
        assert_eq!(fare.price, 220.0);
    }

    #[test]
    fn fare_for_leg_returns_none_when_no_rule_matches() {
        let model = zone_fare_model();
        let leg = FareLeg { route_id: None, origin_zone: Some("999".into()), destination_zone: Some("998".into()), contains_zones: vec![] };
        assert!(model.fare_for_leg(&leg).is_none());
    }

    #[test]
    fn fare_for_leg_matches_route_only_bus_style_rules_via_or_across_rows() {
        // 都営バス実データを模した小フィクスチャ: 1 fare_id が複数 route_id 行を共有
        // (均一運賃)。行間は OR で判定される (fare_for_leg のdoc参照)。
        let model = FareModel {
            attributes: vec![attr("F_FLAT", 210.0)],
            rules: vec![route_rule("F_FLAT", "R1"), route_rule("F_FLAT", "R2"), route_rule("F_FLAT", "R3")],
        };
        let leg_r2 = FareLeg { route_id: Some(RouteId::new("R2")), origin_zone: None, destination_zone: None, contains_zones: vec![] };
        assert_eq!(model.fare_for_leg(&leg_r2).map(|a| a.price), Some(210.0));

        let leg_other = FareLeg { route_id: Some(RouteId::new("R9")), origin_zone: None, destination_zone: None, contains_zones: vec![] };
        assert!(model.fare_for_leg(&leg_other).is_none());
    }

    #[test]
    fn fare_for_leg_matches_contains_id_group_as_subset_of_leg_zones() {
        // contains_id のみの行: 同一fare_idの行を集めた「ゾーン集合」の部分集合なら一致
        // (実データに使用例が無い未検証の分岐。fare_for_leg のdoc参照)。
        let model = FareModel {
            attributes: vec![attr("ZONE_PASS", 500.0)],
            rules: vec![contains_rule("ZONE_PASS", "z1"), contains_rule("ZONE_PASS", "z2"), contains_rule("ZONE_PASS", "z3")],
        };
        let leg_subset =
            FareLeg { route_id: None, origin_zone: None, destination_zone: None, contains_zones: vec!["z1".into(), "z2".into()] };
        assert_eq!(model.fare_for_leg(&leg_subset).map(|a| a.price), Some(500.0));

        let leg_outside =
            FareLeg { route_id: None, origin_zone: None, destination_zone: None, contains_zones: vec!["z1".into(), "z9".into()] };
        assert!(model.fare_for_leg(&leg_outside).is_none(), "集合外のゾーンを含む場合は不一致");
    }

    #[test]
    fn fare_for_leg_picks_cheapest_when_multiple_fare_ids_match() {
        // 仕様に優先順位の明記が無いための保守的な選択 (fare_for_leg のdoc参照)。
        // 実データではこの分岐は発生しない (origin/destinationペアの重複無しを実測済み)。
        let model = FareModel {
            attributes: vec![attr("EXPENSIVE", 500.0), attr("CHEAP", 300.0)],
            rules: vec![od_rule("EXPENSIVE", "1", "2"), od_rule("CHEAP", "1", "2")],
        };
        let leg = FareLeg { route_id: None, origin_zone: Some("1".into()), destination_zone: Some("2".into()), contains_zones: vec![] };
        assert_eq!(model.fare_for_leg(&leg).map(|a| a.price), Some(300.0));
    }

    #[test]
    fn total_fare_collapses_multi_leg_same_feed_journey_into_one_through_fare() {
        // 本家OTP実測 (モジュールdoc「都営単一事業者・乗換1回」): 新宿(428)→都庁前→
        // 本郷三丁目(409) の2 leg 乗換journeyに、同一の1個の運賃product (220円) が
        // 紐付く。都庁前の中間ゾーンを無視し、先頭origin+末尾destinationで1回引く。
        let model = zone_fare_model();
        let legs = vec![
            FareLeg { route_id: None, origin_zone: Some("428".into()), destination_zone: Some("600".into()), contains_zones: vec![] },
            FareLeg { route_id: None, origin_zone: Some("600".into()), destination_zone: Some("409".into()), contains_zones: vec![] },
        ];
        let fare = model.total_fare(&legs).expect("should resolve through-fare");
        assert_eq!(fare.amount, 220.0, "都庁前の中間ゾーンではなく428->409の通し運賃が採用されるはず");
    }

    #[test]
    fn total_fare_errs_when_zone_id_is_missing() {
        // zone_id を持たないフィード (例: 自前頻度JR) の区間は運賃算出不可として
        // 明示的にエラーにする (ワイルドカード一致による誤った運賃提示を避ける)。
        let model = zone_fare_model();
        let legs = vec![FareLeg { route_id: None, origin_zone: None, destination_zone: Some("409".into()), contains_zones: vec![] }];
        assert!(model.total_fare(&legs).is_err());
    }

    #[test]
    fn total_fare_of_empty_legs_is_zero() {
        let model = zone_fare_model();
        let fare = model.total_fare(&[]).expect("empty legs should not error");
        assert_eq!(fare.amount, 0.0);
    }
}
