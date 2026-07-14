//! 実データ (東京都心 OSM + 都営/メトロ/りんかい/京王/東武/自前頻度JR GTFS 6フィード)
//! での座標to座標ドアtoドア経路探索の統合テスト。
//!
//! 本家OTPとの詳細な数値突き合わせは `scripts/compare_door2door.sh` (人間可読出力) の
//! 役目。ここでは実データ規模でパニックせず「桁のあう」結果になることだけを検証する
//! (実データ/`osmium`が無い環境ではskip。`crates/raptor/tests/real_data_smoke.rs` /
//! `crates/street/tests/real_data_smoke.rs` と同じ発想)。
//!
//! 実測 (2026-07-15, 本家OTP `planConnection`, 新宿駅前 35.690,139.700 →
//! 本郷三丁目駅前 35.707,139.759、2026-07-13T08:00発、wheelchair無効。
//! curl -X POST :8080/otp/gtfs/v1 --data '{"query":"query($o:...){ planConnection(...) }"}'):
//!   最速itinerary: WALK 512.9m/564s(→新宿西口) → SUBWAY 大江戸線 900s(新宿西口→本郷三丁目)
//!   → WALK 219.5m/309s(本郷三丁目→目的地)。総所要 08:00:36→08:30:09 = 1773s(29.6分)、乗換0回。
//!   2番目の候補: WALK 180s(→新宿) → RAIL 中央線快速 570s(新宿→御茶ノ水) → WALK 217s
//!   (→御茶ノ水駅前) → BUS 東43 240s(→本郷三丁目駅前) → WALK 186s(→目的地)。
//!
//! otp-rs 側は鉄道のみ (バス非対応, RAPTOR側スコープ) かつ近接駅乗換が直線距離近似
//! (`otp_raptor` モジュールdoc参照) なので厳密一致はしない。「桁が合う」ことを見る。

use std::path::{Path, PathBuf};
use std::process::Command;

use otp_core::LatLng;
use otp_engine::{Engine, Leg, Mobility, RouteRequest};
use otp_fares::FareModel;
use otp_gtfs::Feed;
use otp_raptor::Timetable;
use otp_street::StreetGraph;

/// (フィードID, zipファイル名)。本家 OTP の feedId 割り当てと揃えてある
/// (`crates/raptor/examples/plan.rs` のモジュールdoc参照)。
const FEEDS: &[(&str, &str)] = &[
    ("1", "frequency-jr-gtfs.zip"),
    ("2", "twr-train-gtfs.zip"),
    ("3", "tokyometro-train-gtfs.zip"),
    ("4", "challenge-keio-train-gtfs.zip"),
    ("5", "challenge-tobu-train-gtfs.zip"),
    ("6", "toei-train-gtfs.zip"),
];

fn gtfs_data_dir() -> PathBuf {
    std::env::var("OTP_RS_GTFS_DATA_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../../../infra/otp/data")))
}

/// フィード展開先を毎回一意な一時ディレクトリにする。
///
/// このファイルには実データを使うテストが複数あり、既定のテストランナーは
/// それらを並列実行する。共有の固定ディレクトリ名 (feed_prefix のみで決まる名前) だと
/// 複数テストが同時に同じディレクトリへ `unzip` してレースする (実測: 一方の
/// `unzip` がもう一方の中間状態を踏んで `cannot create .../agency.txt: No such file
/// or directory` で失敗し、`load_all_feeds_or_skip` が None を返して当該テストが
/// 「データ無しでskip」と区別つかないまま何も検証せず通ってしまう、という
/// サイレントな穴があった)。zip 自体は小さい (数百KB)ので、テストごとに
/// 展開し直すコストは無視できる。
fn prepare_feed_dir(feed_prefix: &str, zip_path: &Path) -> Option<PathBuf> {
    if !zip_path.is_file() {
        eprintln!("GTFS zip が見つかりません: {} (feed={feed_prefix})。skip します。", zip_path.display());
        return None;
    }
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let out_dir = std::env::temp_dir().join(format!("otp-rs-engine-door2door-gtfs-{feed_prefix}.{}.{n}", std::process::id()));
    std::fs::create_dir_all(&out_dir).ok()?;
    let status = Command::new("unzip").arg("-o").arg("-q").arg(zip_path).arg("-d").arg(&out_dir).status().ok()?;
    if !status.success() {
        eprintln!("unzip 失敗 (feed={feed_prefix})。skip します。");
        return None;
    }
    Some(out_dir)
}

fn load_all_feeds_or_skip() -> Option<Vec<Feed>> {
    let dir = gtfs_data_dir();
    let mut feeds = Vec::with_capacity(FEEDS.len());
    for (prefix, filename) in FEEDS {
        let zip_path = dir.join(filename);
        let feed_dir = prepare_feed_dir(prefix, &zip_path)?;
        let feed = Feed::load_from_dir_namespaced(&feed_dir, prefix).unwrap_or_else(|e| panic!("feed {prefix} load failed: {e}"));
        feeds.push(feed);
    }
    Some(feeds)
}

fn default_pbf_path() -> PathBuf {
    PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../../../infra/otp/data/tokyo-central.osm.pbf"))
}

/// 新宿〜本郷三丁目を覆う bbox (`139.680,35.670,139.780,35.720`) の OSM XML を用意する。
fn prepare_osm_xml() -> Option<PathBuf> {
    if Command::new("osmium").arg("--version").output().is_err() {
        eprintln!("osmium が見つかりません。skip します。");
        return None;
    }
    let pbf = std::env::var("OTP_RS_OSM_PBF").map(PathBuf::from).unwrap_or_else(|_| default_pbf_path());
    if !pbf.is_file() {
        eprintln!("実データ .osm.pbf が見つかりません ({}). skip します。", pbf.display());
        return None;
    }

    let out = std::env::temp_dir().join("otp-rs-engine-shinjuku-hongo.osm");
    if !out.is_file() {
        let script = PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../../scripts/extract_osm_xml.sh"));
        // 実データ street テストと同じレース対策 (並列テスト実行から一意な一時ファイルに
        // 書いてから rename): `crates/street/tests/real_data_smoke.rs` 参照。
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let unique_tmp = std::env::temp_dir().join(format!("otp-rs-engine-shinjuku-hongo.{}.{}.osm", std::process::id(), n));
        let status = Command::new("bash").arg(&script).arg("139.680,35.670,139.780,35.720").arg(&unique_tmp).arg(&pbf).status();
        match status {
            Ok(s) if s.success() => {
                let _ = std::fs::rename(&unique_tmp, &out);
            }
            _ => {
                eprintln!("extract_osm_xml.sh に失敗しました。skip します。");
                let _ = std::fs::remove_file(&unique_tmp);
                return None;
            }
        }
    }
    out.is_file().then_some(out)
}

fn build_engine_or_skip() -> Option<Engine> {
    let feeds = load_all_feeds_or_skip()?;
    let timetable = Timetable::build(&feeds).expect("timetable should build from real feeds");
    let osm_xml = prepare_osm_xml()?;
    let street = StreetGraph::build_from_osm_xml(&osm_xml).expect("street graph should build from real OSM XML");
    Some(Engine::new(street, timetable, FareModel::default()))
}

const SHINJUKU: LatLng = LatLng::new(35.690, 139.700);
const HONGO_SANCHOME: LatLng = LatLng::new(35.707, 139.759);

fn request(mobility: Mobility) -> RouteRequest {
    RouteRequest {
        origin: SHINJUKU,
        destination: HONGO_SANCHOME,
        // 07:50発。本家OTP実測でも最寄駅までの徒歩は9分強かかる (モジュールdoc参照)。
        // 08:00台の列車に間に合うよう、access徒歩ぶんの余裕を見て早めに出発させる。
        depart_at: 7 * 3600 + 50 * 60,
        service_date: 20260713, // 月曜
        mobility,
    }
}

#[test]
fn shinjuku_to_hongo_sanchome_door_to_door_is_sane_for_solo_and_stroller() {
    let Some(engine) = build_engine_or_skip() else { return };

    for mobility in [Mobility::Solo, Mobility::Stroller] {
        let itineraries = engine.plan(&request(mobility)).expect("plan should not error");
        assert!(!itineraries.is_empty(), "{mobility:?}: 経路が1つも見つからなかった");

        let best = &itineraries[0];
        eprintln!("=== {mobility:?}: total_duration_s={} ({:.1}分) transfers={} ===", best.total_duration_s, best.total_duration_s as f64 / 60.0, best.transfers);
        for leg in &best.legs {
            eprintln!("  {leg:?}");
        }

        assert!(!best.legs.is_empty());
        assert!(matches!(best.legs.first(), Some(Leg::Walk { .. })), "{mobility:?}: 先頭はaccess徒歩のはず: {:?}", best.legs.first());
        assert!(matches!(best.legs.last(), Some(Leg::Walk { .. })), "{mobility:?}: 末尾はegress徒歩のはず: {:?}", best.legs.last());
        assert!(best.legs.iter().any(|l| matches!(l, Leg::Transit { .. })), "{mobility:?}: 鉄道を1本は使うはず");

        // 本家OTP実測 (モジュールdoc): 最速29.6分・乗換0回。otp-rsは鉄道のみ・乗換の
        // 直線距離近似という既知の差分があるため厳密一致は求めず、「桁が合う」ことを見る
        // (既存のraptor/street実データテストと同水準、15分〜80分に収まれば妥当)。
        assert!(
            (900..4800).contains(&best.total_duration_s),
            "{mobility:?}: total_duration_s={} は想定レンジ外",
            best.total_duration_s
        );
        assert!(best.transfers <= 3, "{mobility:?}: transfers={} が多すぎる", best.transfers);
        assert!(best.fare_yen.is_none(), "運賃は今スライスでは常に None のはず");
    }
}

#[test]
fn stroller_never_produces_a_shorter_total_duration_than_solo_on_real_data() {
    let Some(engine) = build_engine_or_skip() else { return };

    let solo = engine.plan(&request(Mobility::Solo)).expect("solo plan should not error");
    let stroller = engine.plan(&request(Mobility::Stroller)).expect("stroller plan should not error");
    if solo.is_empty() || stroller.is_empty() {
        return; // 経路が無い場合の検知は別テストの役目
    }

    assert!(
        stroller[0].total_duration_s >= solo[0].total_duration_s,
        "stroller ({}) が solo ({}) より速くなるのはおかしい (ベビーカーは速度が遅く階段回避コストも高い)",
        stroller[0].total_duration_s,
        solo[0].total_duration_s
    );
}
