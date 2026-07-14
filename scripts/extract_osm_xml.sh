#!/usr/bin/env bash
# .osm.pbf (Protocol Buffers + zlib) から otp-street が読める OSM XML (.osm) を
# 作る前処理スクリプト。
#
# なぜこの経路か (otp-street の std依存ゼロ方針との折り合い):
#   otp-rs は「std のみでコンパイルが通る」ことを守っている。.osm.pbf を自前で
#   パースするには Protocol Buffers + zlib 展開が要るが、これを std だけで
#   書くのはコストに見合わない (外部クレート osmpbf 等が要る)。一方 OSM XML は
#   単純な `<tag k=".." v=".."/>` の並びで、std の文字列処理だけで手書き
#   パーサが書ける (crates/street/src/osm_xml.rs)。そこで「重い変換は前処理
#   (osmium, 外部コマンド) に任せ、otp-street は軽い XML だけを読む」役割分担
#   にした。GTFS 側が zip 展開に `unzip` コマンドを使うのと同じ発想
#   (crates/gtfs/tests/load_real_data.rs 参照)。
#
# 前提: `osmium-tool` (brew install osmium-tool) がインストール済み。
#
# 使い方:
#   scripts/extract_osm_xml.sh <left,bottom,right,top> [output.osm] [input.pbf]
#
#   例 (新宿駅周辺 bbox, babymobi infra の東京都心抽出を入力にする):
#   ./scripts/extract_osm_xml.sh 139.690,35.685,139.710,35.700 /tmp/shinjuku.osm
#
# 何をするか (実測済みパイプライン, 2026-07-15):
#   1. osmium extract --bbox: 指定 bbox を完全な way ごと切り出す
#   2. osmium tags-filter w/highway: highway=* を持つ way (+ 参照ノード) だけに絞る
#      (`-R` は「参照先を含めない」オプションなので付けない。デフォルトで
#      参照ノードが補完される)
#   3. osmium cat -o *.osm: PBF → OSM XML に変換 (拡張子から自動判定)
#
# 実データでの検証 (infra/otp/data/tokyo-central.osm.pbf, 96MB → 新宿駅周辺
# bbox 抽出): 1.0MB (extract) → highway フィルタ後 XML 1.6MB, node 13,038 /
# way 4,056 (フィルタ後、除外前の highway=* 全種)。所要 3秒未満。
set -euo pipefail
cd "$(dirname "$0")/.."

BBOX="${1:?使い方: scripts/extract_osm_xml.sh <left,bottom,right,top> [output.osm] [input.pbf]}"
OUT="${2:-data/extract.osm}"
PBF="${3:-${OTP_RS_OSM_PBF:-../infra/otp/data/tokyo-central.osm.pbf}}"

if ! command -v osmium >/dev/null 2>&1; then
  echo "osmium が見つかりません。'brew install osmium-tool' 等でインストールしてください。" >&2
  exit 1
fi
if [ ! -f "$PBF" ]; then
  echo "入力 .osm.pbf が見つかりません: $PBF" >&2
  exit 1
fi

mkdir -p "$(dirname "$OUT")"
TMP_DIR="$(mktemp -d)"
trap 'rm -rf "$TMP_DIR"' EXIT

echo "1/3 bbox抽出: $BBOX <- $PBF"
osmium extract --bbox "$BBOX" -O -o "$TMP_DIR/extract.osm.pbf" "$PBF"

echo "2/3 highwayタグでフィルタ (参照ノード補完込み)"
osmium tags-filter -O -o "$TMP_DIR/filtered.osm.pbf" "$TMP_DIR/extract.osm.pbf" w/highway

echo "3/3 OSM XML に変換 -> $OUT"
osmium cat -O -o "$OUT" "$TMP_DIR/filtered.osm.pbf"

echo "done: $OUT ($(osmium fileinfo -e "$OUT" 2>/dev/null | grep -E 'Number of (nodes|ways):' || true))"
