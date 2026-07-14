#!/usr/bin/env bash
# 本家 OTP (babymobi infra/otp のローカルコンテナ) と otp-rs の Engine::plan を
# 座標to座標・同一出発時刻で突き合わせる検証スクリプト。
# `scripts/compare_otp.sh` (駅to駅・RAPTORのみ) の座標to座標・engine統合版。
#
# 前提:
#   - babymobi/infra/otp で `docker compose up -d otp-serve` 済み (http://localhost:8080)
#   - babymobi/infra/otp/data/ に鉄道 GTFS 6本 + tokyo-central.osm.pbf が揃っている
#   - osmium-tool (brew install osmium-tool) がインストール済み
#
# 使い方:
#   cd otp-rs && ./scripts/compare_door2door.sh [bbox] [origin_lat,origin_lon] [dest_lat,dest_lon] [HH:MM] [YYYYMMDD]
#   (引数省略時は新宿駅前 -> 本郷三丁目駅前, 2026-07-13 07:50発 が既定)
#
# 何をするか:
#   1. `scripts/extract_osm_xml.sh` で bbox の街路 OSM XML を用意する
#      (既に data/ 配下にあれば再利用)
#   2. `cargo run -p otp-engine --example door_to_door` で solo/stroller/wheelchair
#      それぞれの Engine::plan 結果を出す
#   3. OTP の GraphQL (`/otp/gtfs/v1`, planConnection, coordinate指定) を
#      wheelchair 無効/有効の両方で叩く
#   4. 両者を並べて表示する (数値の突き合わせは目視 / 呼び出し側で行う)
#
# 実測 (2026-07-15, 新宿駅前 35.690,139.700 -> 本郷三丁目駅前 35.707,139.759,
# 2026-07-13 07:50発 (OTP問い合わせは08:00発, otp-rsは access徒歩の余裕を見て07:50発)):
#
#   otp-rs (solo,      07:50発): 総所要 1711s (28.5分) 乗換1回 [新宿線 6:301->6:307 → 丸ノ内線 3:222->3:224]
#   otp-rs (stroller,  07:50発): 総所要 1967s (32.8分) 乗換1回 [同上, access徒歩が階段回避で+91m]
#   otp-rs (wheelchair,07:50発): 総所要 2001s (33.4分) 乗換1回 [同上, 徒歩速度が遅い分さらに+34s]
#
#   OTP    (通常,      08:00発): 総所要 1773s (29.6分) 乗換0回 [大江戸線 新宿西口->本郷三丁目 直通]
#   OTP    (wheelchair,08:00発): 総所要 1977s (33.0分) 乗換0回 [同じく大江戸線直通, access徒歩が625m→765mに伸びる]
#
#   → 総所要は同オーダー (otp-rsのsoloはOTPの-1.1分、strollerはOTPのwheelchair版と
#     ほぼ同値=+0.2分の差)。乗換回数と使用路線は完全一致しない
#     (otp-rsは新宿線→丸ノ内線を1回乗換、OTPは大江戸線直通)。既知の要因:
#       - access/egress 候補駅の選び方の違い (otp-rsは直線距離近傍上位5駅、
#         OTPは実street routingで最良を選ぶ)
#       - otp-rsの近接駅乗換は直線距離近似 (otp_raptorモジュールdoc参照)、
#         バス非対応 (RAPTORは鉄道のみ)
#     いずれも既知の限界としてコード上に記録済み。桁 (30分前後) と
#     mobilityによる経路/所要の変化傾向は一致している。

set -euo pipefail
cd "$(dirname "$0")/.."

BBOX="${1:-139.680,35.670,139.780,35.720}"
ORIGIN="${2:-35.690,139.700}"
DEST="${3:-35.707,139.759}"
TIME="${4:-07:50}"
DATE="${5:-20260713}"
DATE_DASHED="${DATE:0:4}-${DATE:4:2}-${DATE:6:2}"
OTP_TIME="${6:-08:00}"

OTP_URL="${OTP_URL:-http://localhost:8080/otp/gtfs/v1}"
OSM_XML="${OTP_RS_OSM_XML:-data/door2door_extract.osm}"

ORIGIN_LAT="${ORIGIN%,*}"; ORIGIN_LON="${ORIGIN#*,}"
DEST_LAT="${DEST%,*}"; DEST_LON="${DEST#*,}"

if [ ! -f "$OSM_XML" ]; then
  echo "OSM XML が無いので抽出します: $OSM_XML (bbox=$BBOX)"
  ./scripts/extract_osm_xml.sh "$BBOX" "$OSM_XML"
fi

query_otp() {
  local wheelchair="$1"
  local body
  body=$(cat <<JSON
{
  "query": "query(\$origin: PlanLabeledLocationInput!, \$destination: PlanLabeledLocationInput!, \$dt: OffsetDateTime!, \$wc: Boolean!) { planConnection(origin: \$origin, destination: \$destination, dateTime: {earliestDeparture: \$dt}, first: 3, preferences: {accessibility: {wheelchair: {enabled: \$wc}}}) { edges { node { start end legs { mode duration distance route { longName shortName } from { name } to { name } } } } } }",
  "variables": {
    "origin": {"location": {"coordinate": {"latitude": ${ORIGIN_LAT}, "longitude": ${ORIGIN_LON}}}},
    "destination": {"location": {"coordinate": {"latitude": ${DEST_LAT}, "longitude": ${DEST_LON}}}},
    "dt": "${DATE_DASHED}T${OTP_TIME}:00+09:00",
    "wc": ${wheelchair}
  }
}
JSON
)
  curl -s -X POST "$OTP_URL" -H 'Content-Type: application/json' --data "$body"
}

echo "======================================================================"
echo "OD: ${ORIGIN} -> ${DEST}"
echo "otp-rs: ${DATE_DASHED} ${TIME} 発 / OTP: ${DATE_DASHED} ${OTP_TIME} 発 (access徒歩の余裕差)"
echo "======================================================================"

for mobility in solo stroller wheelchair; do
  echo "--- otp-rs Engine::plan (mobility=${mobility}) ---"
  cargo run -q -p otp-engine --example door_to_door -- "$OSM_XML" "$ORIGIN_LAT" "$ORIGIN_LON" "$DEST_LAT" "$DEST_LON" "$TIME" "$DATE" "$mobility"
  echo
done

echo "--- 本家 OTP (GraphQL planConnection, wheelchair無効) ---"
query_otp false | python3 -m json.tool
echo

echo "--- 本家 OTP (GraphQL planConnection, wheelchair有効) ---"
query_otp true | python3 -m json.tool
