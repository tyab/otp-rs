#!/usr/bin/env bash
# 本家 OTP (babymobi infra/otp のローカルコンテナ) と otp-rs の RAPTOR を
# 同一 OD・同一時刻で突き合わせる検証スクリプト。
#
# 前提:
#   - babymobi/infra/otp で `docker-compose up -d otp-serve` 済み (http://localhost:8080)
#   - babymobi/infra/otp/data/ に鉄道 GTFS 6本 (都営・メトロ・りんかい・京王・東武・
#     自前頻度JR) が揃っている
#
# 使い方:
#   cd otp-rs && ./scripts/compare_otp.sh
#
# 何をするか:
#   1. `cargo run -p otp-raptor --example plan` で RAPTOR (6フィード統合) の
#      駅to駅探索結果を出す。stop_id は本家 OTP の feedId と揃えた
#      "<feedId>:<rawStopId>" 形式 (plan.rs のモジュールdoc参照)。
#   2. OTP の GraphQL (`/otp/gtfs/v1`, planConnection, stopLocation 指定で
#      街路探索を介さない駅to駅比較にする) を curl で叩く (同じ stop_id 文字列を渡す)
#   3. 両者を並べて表示する (数値の突き合わせは目視 / 呼び出し側で行う)
#
# 実測 (2026-07-15, 名前空間化 + 既定乗換バッファ(120秒) + 近接駅徒歩乗換(150m以内)
# 導入後。都営大江戸線は 2026-07-13 08:00発、複数フィード横断ODは同時刻):
#
#   - OD1 新宿(6:428)→本郷三丁目(6:409) [都庁前で乗換1回, 都営単一フィード]:
#     導入前 RAPTOR 08:01→08:21 (都庁前で0分接続) /
#     導入後 RAPTOR 08:01→08:25 (都庁前で120秒接続) /
#     OTP    08:05→08:25 (都庁前で120秒=2分接続)
#     → 到着時刻の差: 4分 → 0分 (完全一致)。既定バッファ(120秒)がちょうど
#       都庁前の実測乗換時分(2分)と一致したため。
#
#   - OD2 新宿西口(6:402)→本郷三丁目(6:409) [乗換無し, 単一便]:
#     RAPTOR 08:03→08:18 (15分) / OTP 08:03→08:18 (15分) → 完全一致 (乗換が絡まないため無変化)。
#
#   - OD3 六本木一丁目(3:805, メトロ南北線)→三田(6:204, 都営三田線)
#     [白金高輪でメトロ↔都営の事業者跨ぎ乗換1回, 複数フィード統合]:
#     RAPTOR 08:01→08:14 (乗換1回, 白金高輪で133秒接続 = 直線16m/1.3m/s+120秒バッファ) /
#     OTP    08:05→08:18 (乗換1回, 白金高輪で253秒接続 = 実測の地下通路徒歩時間)
#     → 所要時間(13分)は一致。個別の乗換秒数は直線距離近似(133秒)が実測の地下通路
#       徒歩(253秒)より短く、完全一致はしない (地下通路長は街路グラフが無いと
#       再現できない既知の限界。otp-street 実装後の次スライスで縮められる)。
#     → 導入前は複数フィードのID衝突 (route_id/service_id が事業者間で"0"〜"4"を
#       再利用) のため、このODはそもそも解けなかった (機能自体が無かった)。

set -euo pipefail
cd "$(dirname "$0")/.."

OTP_URL="${OTP_URL:-http://localhost:8080/otp/gtfs/v1}"
DATE="${1:-2026-07-13}"
TIME="${2:-08:00}"
DATE_COMPACT="${DATE//-/}"

query_otp() {
  local origin_stop="$1" dest_stop="$2"
  local body
  body=$(cat <<JSON
{
  "query": "query(\$origin: PlanLabeledLocationInput!, \$destination: PlanLabeledLocationInput!, \$dt: OffsetDateTime!) { planConnection(origin: \$origin, destination: \$destination, dateTime: {earliestDeparture: \$dt}, first: 3) { edges { node { start end legs { mode duration route { longName } from { name } to { name } } } } } }",
  "variables": {
    "origin": {"location": {"stopLocation": {"stopLocationId": "${origin_stop}"}}},
    "destination": {"location": {"stopLocation": {"stopLocationId": "${dest_stop}"}}},
    "dt": "${DATE}T${TIME}:00+09:00"
  }
}
JSON
)
  curl -s -X POST "$OTP_URL" -H 'Content-Type: application/json' --data "$body"
}

# stop_id は本家 OTP の feedId 前置形式 ("6:428" 等) をそのまま両方に渡す
# (plan.rs のフィードID割り当てが本家 OTP と揃えてあるため)。
compare_od() {
  local name="$1" origin="$2" dest="$3"
  echo "======================================================================"
  echo "OD: $name  (${origin} -> ${dest}, ${DATE} ${TIME} 発)"
  echo "--- otp-rs (RAPTOR, 複数フィード統合) ---"
  cargo run -q -p otp-raptor --example plan -- "$origin" "$dest" "$TIME" "$DATE_COMPACT"
  echo "--- 本家 OTP (GraphQL planConnection) ---"
  query_otp "$origin" "$dest" | python3 -m json.tool
  echo
}

compare_od "新宿西口->本郷三丁目 (直通・乗換無しの想定ケース, 都営単一フィード)" "6:402" "6:409"
compare_od "新宿->本郷三丁目 (都庁前で乗換1回, 都営単一フィード内, 既知の4分差ケース)" "6:428" "6:409"
compare_od "六本木一丁目->三田 (白金高輪でメトロ→都営の事業者跨ぎ乗換1回)" "3:805" "6:204"
