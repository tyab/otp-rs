#!/usr/bin/env bash
# 本家 OTP (babymobi infra/otp のローカルコンテナ) と otp-rs の RAPTOR を
# 同一 OD・同一時刻で突き合わせる検証スクリプト (スライス3)。
#
# 前提:
#   - babymobi/infra/otp で `docker-compose up -d otp-serve` 済み (http://localhost:8080)
#   - babymobi/infra/otp/data/toei-train-gtfs.zip が存在する (都営地下鉄 GTFS)
#
# 使い方:
#   cd otp-rs && ./scripts/compare_otp.sh
#
# 何をするか:
#   1. `cargo run -p otp-raptor --example plan` で RAPTOR の駅to駅探索結果を出す
#   2. OTP の GraphQL (`/otp/gtfs/v1`, planConnection, stopLocation 指定で
#      街路探索を介さない駅to駅比較にする) を curl で叩く
#   3. 両者を並べて表示する (数値の突き合わせは目視 / 呼び出し側で行う)
#
# 実測 (2026-07-13, 都営大江戸線, 08:00発):
#   - OD2 新宿西口(402)→本郷三丁目(409) [乗換無し, 単一便]:
#     RAPTOR 08:03→08:18 (15分) / OTP 08:03→08:18 (15分) → 完全一致
#   - OD1 新宿(428)→本郷三丁目(409) [都庁前で乗換1回]:
#     RAPTOR 08:01→08:21 (乗換1回, 都庁前で0分接続) /
#     OTP    08:05→08:25 (乗換1回, 都庁前で2分接続) → 到着4分の差
#     原因: OTP は乗換地点で実測の徒歩時間 (最短でも数分) を要求するが、
#     このスライスの RAPTOR は「同一駅は0分乗換」という簡略モデルのため
#     (README/タスク仕様通りの既知の差分)。

set -euo pipefail
cd "$(dirname "$0")/.."

OTP_URL="${OTP_URL:-http://localhost:8080/otp/gtfs/v1}"
FEED="${OTP_FEED_ID:-6}" # infra/otp の feeds クエリで確認済み: 6=東京都交通局(都営)
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
    "origin": {"location": {"stopLocation": {"stopLocationId": "${FEED}:${origin_stop}"}}},
    "destination": {"location": {"stopLocation": {"stopLocationId": "${FEED}:${dest_stop}"}}},
    "dt": "${DATE}T${TIME}:00+09:00"
  }
}
JSON
)
  curl -s -X POST "$OTP_URL" -H 'Content-Type: application/json' --data "$body"
}

compare_od() {
  local name="$1" origin="$2" dest="$3"
  echo "======================================================================"
  echo "OD: $name  (${origin} -> ${dest}, ${DATE} ${TIME} 発)"
  echo "--- otp-rs (RAPTOR) ---"
  cargo run -q -p otp-raptor --example plan -- "$origin" "$dest" "$TIME" "$DATE_COMPACT"
  echo "--- 本家 OTP (GraphQL planConnection) ---"
  query_otp "$origin" "$dest" | python3 -m json.tool
  echo
}

compare_od "新宿西口->本郷三丁目 (直通・乗換無しの想定ケース)" 402 409
compare_od "新宿->本郷三丁目 (都庁前で乗換1回の想定ケース)" 428 409
