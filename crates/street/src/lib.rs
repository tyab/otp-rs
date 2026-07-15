//! OSM 街路グラフと歩行ルーティング。**アクセシビリティ・コスト**がこのアプリの核。
//!
//! OTP の `street` / `astar` / `WheelchairPreferences` 相当。段差・エレベーター・勾配を
//! エッジ属性として持ち、プロファイル (通常/ベビーカー/車いす) ごとにコストを変える。

use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap};
use std::path::Path;

use otp_core::LatLng;

mod osm_xml;

/// 街路グラフの頂点添字 (CSR 配列のインデックス)。
pub type NodeId = u32;

/// 街路の頂点 (交差点・ノード)。
#[derive(Debug, Clone, Copy)]
pub struct StreetNode {
    pub coord: LatLng,
}

/// 街路のエッジ (歩行可能な区間)。アクセシビリティ属性を保持する。
#[derive(Debug, Clone)]
pub struct StreetEdge {
    pub from: NodeId,
    pub to: NodeId,
    pub length_m: f32,
    /// 階段を含む (OSM `highway=steps`)。
    pub has_stairs: bool,
    /// エレベーターを含む/経由 (OSM `highway=elevator` / `elevator=yes`)。
    pub has_elevator: bool,
    /// 最大勾配 (%)。不明は None。
    pub max_slope_pct: Option<f32>,
    /// 車いす通行可否 (OSM `wheelchair`)。不明は None。
    pub wheelchair: Option<bool>,
    /// 描画用ジオメトリはグラフには載せない (探索に不要)。確定経路の分だけ
    /// 外部 (babymobi 側 R2 等) から取得する想定。ここでは参照キーのみ持つ。
    pub geometry_ref: Option<u32>,
}

/// 移動プロファイル。OTP の wheelchairAccessibility チューニングに対応。
#[derive(Debug, Clone)]
pub struct WalkProfile {
    /// 徒歩速度 (m/秒)。
    pub speed_mps: f32,
    /// 階段の忌避度 (コスト倍率)。車いす=極大, ベビーカー=中, 通常=1。
    pub stairs_reluctance: f32,
    /// 通行可否不明エッジへのペナルティ (OTP unknownCost 相当)。
    /// 不明を除外すると経路が出なくなるため「重み付きで通す」。
    pub unknown_cost: f32,
    /// 許容最大勾配 (%)。超過区間にペナルティ。
    pub max_slope_pct: f32,
}

impl WalkProfile {
    /// 通常徒歩。
    pub fn normal() -> Self {
        Self { speed_mps: 1.33, stairs_reluctance: 1.0, unknown_cost: 1.0, max_slope_pct: 100.0 }
    }
    /// ベビーカー: 階段は強く避けるが車いすよりは許容 (担いで数段は可)。
    pub fn stroller() -> Self {
        Self { speed_mps: 1.2, stairs_reluctance: 10.0, unknown_cost: 1.5, max_slope_pct: 12.0 }
    }
    /// 車いす: 階段は事実上不可、勾配厳格。
    pub fn wheelchair() -> Self {
        Self { speed_mps: 1.0, stairs_reluctance: 100.0, unknown_cost: 2.0, max_slope_pct: 8.0 }
    }
}

/// 街路グラフ (CSR 形式を想定した最小の器)。
#[derive(Debug, Default)]
pub struct StreetGraph {
    pub nodes: Vec<StreetNode>,
    pub edges: Vec<StreetEdge>,
    /// nodes[i] の出エッジは edges[adjacency_start[i]..adjacency_start[i+1]]。
    pub adjacency_start: Vec<u32>,
}

/// 歩行可能な `highway=*` 値。OTP の `StreetTraversalPermission` 判定の簡易版。
/// motorway/trunk 等の自動車専用道は含めない (歩道が別 way で分離マッピングされる前提)。
const WALKABLE_HIGHWAY: &[&str] = &[
    "footway",
    "path",
    "pedestrian",
    "steps",
    "living_street",
    "residential",
    "service",
    "track",
    "unclassified",
    "tertiary",
    "secondary",
    "primary",
    "elevator",
];

/// この way が歩行者にとって通行可能か。
///
/// - `highway` が [`WALKABLE_HIGHWAY`] のいずれかであること。
/// - `foot=no` (明示的な歩行者通行禁止) は highway の種別に関わらず除外する。
///   例: 実データの甲州街道/青梅街道 (`highway=trunk`/`primary` + `foot=no`)。
///
/// 対応範囲外 (将来の課題): `access=private/no` の一般規則、`sidewalk:*` タグに
/// よる歩道の分離指定、`foot=private` 等の細かい値。MVP では `foot=no` の
/// ハード除外のみで十分な精度が出ることを実データで確認済み。
fn is_walkable(way: &osm_xml::OsmWay) -> bool {
    let Some(highway) = way.tag("highway") else {
        return false;
    };
    if !WALKABLE_HIGHWAY.contains(&highway) {
        return false;
    }
    if way.tag("foot") == Some("no") {
        return false;
    }
    true
}

/// アクセシビリティ属性を way タグから読み取る。
fn accessibility_attrs(way: &osm_xml::OsmWay) -> (bool, bool, Option<bool>, Option<f32>) {
    let has_stairs = way.tag("highway") == Some("steps");
    let has_elevator = way.tag("highway") == Some("elevator") || way.tag("elevator") == Some("yes");
    let wheelchair = match way.tag("wheelchair") {
        Some("yes") => Some(true),
        Some("no") => Some(false),
        // "limited" 等の中間値は Option<bool> で表現できないため、素性が
        // 不明な場合と同様に扱う (profile.unknown_cost が緩めに効く)。
        _ => None,
    };
    // incline は "8%" / "-10%" のような百分率表記、または "up"/"down" のような
    // 定性的表記がある。数値表記のみ解釈し、符号は無視 (上りも下りも勾配としては
    // 同じペナルティ対象)。
    let max_slope_pct = way
        .tag("incline")
        .and_then(|s| s.strip_suffix('%'))
        .and_then(|s| s.parse::<f32>().ok())
        .map(|v| v.abs());
    (has_stairs, has_elevator, wheelchair, max_slope_pct)
}

impl StreetGraph {
    /// OSM XML (`.osm`) から歩行グラフを構築する。
    ///
    /// 入力は `.osm.pbf` ではなく前処理済みの OSM XML を想定する
    /// (`scripts/extract_osm_xml.sh` で `osmium` により bbox 抽出 + `highway`
    /// タグフィルタ + XML 変換したもの)。理由: `.osm.pbf` は Protocol Buffers +
    /// zlib で、std のみで自前パースするのはコストに見合わない。std の
    /// テキスト処理だけで読める OSM XML を選び、外部クレート依存ゼロを維持する。
    pub fn build_from_osm_xml(path: &Path) -> otp_core::Result<StreetGraph> {
        let content = std::fs::read_to_string(path)?;
        Ok(Self::build_from_osm_xml_str(&content))
    }

    /// [`build_from_osm_xml`] のファイル非依存版 (テスト用に公開)。
    pub fn build_from_osm_xml_str(xml: &str) -> StreetGraph {
        let doc = osm_xml::parse(xml);
        let coord_by_id: HashMap<i64, LatLng> = doc
            .nodes
            .iter()
            .map(|n| (n.id, LatLng::new(n.lat, n.lon)))
            .collect();

        // 使う way だけ抽出し、参照される node に NodeId (CSR インデックス) を
        // 初出順で割り当てる (決定的な結果にするため)。
        let mut node_index: HashMap<i64, NodeId> = HashMap::new();
        let mut nodes: Vec<StreetNode> = Vec::new();
        let mut raw_edges: Vec<StreetEdge> = Vec::new();

        for way in &doc.ways {
            if !is_walkable(way) || way.nodes.len() < 2 {
                continue;
            }
            let (has_stairs, has_elevator, wheelchair, max_slope_pct) = accessibility_attrs(way);

            let mut resolved: Vec<NodeId> = Vec::with_capacity(way.nodes.len());
            for &osm_id in &way.nodes {
                let Some(coord) = coord_by_id.get(&osm_id).copied() else {
                    // 参照ノードが見つからない (抽出範囲の境界等)。この way は諦める。
                    resolved.clear();
                    break;
                };
                let id = *node_index.entry(osm_id).or_insert_with(|| {
                    let id = nodes.len() as NodeId;
                    nodes.push(StreetNode { coord });
                    id
                });
                resolved.push(id);
            }
            if resolved.len() < 2 {
                continue;
            }

            for pair in resolved.windows(2) {
                let (a, b) = (pair[0], pair[1]);
                let length_m = nodes[a as usize]
                    .coord
                    .haversine_m(&nodes[b as usize].coord) as f32;
                // 歩行は基本双方向 (oneway の車両規制は歩行者に及ばない前提)。
                raw_edges.push(StreetEdge {
                    from: a,
                    to: b,
                    length_m,
                    has_stairs,
                    has_elevator,
                    max_slope_pct,
                    wheelchair,
                    geometry_ref: None,
                });
                raw_edges.push(StreetEdge {
                    from: b,
                    to: a,
                    length_m,
                    has_stairs,
                    has_elevator,
                    max_slope_pct,
                    wheelchair,
                    geometry_ref: None,
                });
            }
        }

        // CSR 化: `from` でソートし、adjacency_start で範囲を引けるようにする。
        raw_edges.sort_by_key(|e| e.from);
        let mut adjacency_start = vec![0u32; nodes.len() + 1];
        for edge in &raw_edges {
            adjacency_start[edge.from as usize + 1] += 1;
        }
        for i in 1..adjacency_start.len() {
            adjacency_start[i] += adjacency_start[i - 1];
        }

        StreetGraph {
            nodes,
            edges: raw_edges,
            adjacency_start,
        }
    }

    /// nodes[i] の出エッジのスライス。
    fn out_edges(&self, node: NodeId) -> &[StreetEdge] {
        let start = self.adjacency_start[node as usize] as usize;
        let end = self.adjacency_start[node as usize + 1] as usize;
        &self.edges[start..end]
    }

    /// 座標に最も近い頂点を探す (線形探索)。
    ///
    /// 小領域 (駅周辺 bbox 程度) なら十分高速。広域グラフではグリッド索引
    /// (緯度経度をバケット分割したハッシュ索引) への切り替えを検討する
    /// (将来課題。今のスライスのスコープ外)。
    fn nearest_node(&self, coord: LatLng) -> Option<NodeId> {
        self.nodes
            .iter()
            .enumerate()
            .min_by(|(_, a), (_, b)| {
                a.coord
                    .haversine_m(&coord)
                    .partial_cmp(&b.coord.haversine_m(&coord))
                    .unwrap_or(Ordering::Equal)
            })
            .map(|(i, _)| i as NodeId)
    }

    /// プロファイルに応じたエッジの一般化コスト (秒相当)。探索の重み。
    pub fn edge_cost(&self, edge: &StreetEdge, profile: &WalkProfile) -> f32 {
        let base = edge.length_m / profile.speed_mps;
        let mut cost = base;
        if edge.has_stairs {
            cost *= profile.stairs_reluctance;
        }
        if edge.wheelchair == Some(false) {
            cost *= profile.stairs_reluctance.max(2.0);
        } else if edge.wheelchair.is_none() {
            cost *= profile.unknown_cost;
        }
        if let Some(slope) = edge.max_slope_pct {
            if slope > profile.max_slope_pct {
                cost *= 1.0 + (slope - profile.max_slope_pct) / 10.0;
            }
        }
        cost
    }

    /// 2点間の歩行経路探索 (A*)。
    ///
    /// `from`/`to` をそれぞれ最近傍ノードへスナップし、`edge_cost` (秒相当) を
    /// 重みにした A* で最小コスト経路を求める。ヒューリスティックは残り区間の
    /// 直線距離 / `profile.speed_mps` (= 段差・不明ペナルティ無しの理想コスト)。
    /// `edge_cost` の乗数は全て 1.0 以上 (階段忌避・不明ペナルティ・勾配ペナルティは
    /// コストを増やす方向にしか働かない) なので、このヒューリスティックは
    /// admissible かつ consistent。
    pub fn route(
        &self,
        from: LatLng,
        to: LatLng,
        profile: &WalkProfile,
    ) -> otp_core::Result<WalkPath> {
        let start = self.nearest_node(from).ok_or(otp_core::Error::NotFound(
            "no street node near origin".to_string(),
        ))?;
        let goal = self.nearest_node(to).ok_or(otp_core::Error::NotFound(
            "no street node near destination".to_string(),
        ))?;
        let goal_coord = self.nodes[goal as usize].coord;

        if start == goal {
            return Ok(WalkPath {
                nodes: vec![start],
                distance_m: 0.0,
                duration_s: 0.0,
                physical_duration_s: 0.0,
                has_stairs: false,
            });
        }

        let n = self.nodes.len();
        let mut g_score = vec![f32::INFINITY; n];
        let mut came_from: Vec<Option<(NodeId, usize)>> = vec![None; n]; // (親ノード, edges中のインデックス)
        let mut open = BinaryHeap::new();
        let mut closed = vec![false; n];

        g_score[start as usize] = 0.0;
        open.push(HeapItem {
            f: heuristic(self.nodes[start as usize].coord, goal_coord, profile),
            node: start,
        });

        while let Some(HeapItem { node: current, .. }) = open.pop() {
            if current == goal {
                let mut path = self.reconstruct_path(goal, &came_from, &g_score);
                path.physical_duration_s = path.distance_m / profile.speed_mps;
                return Ok(path);
            }
            if closed[current as usize] {
                continue;
            }
            closed[current as usize] = true;

            let start_idx = self.adjacency_start[current as usize] as usize;
            for (offset, edge) in self.out_edges(current).iter().enumerate() {
                let neighbor = edge.to;
                if closed[neighbor as usize] {
                    continue;
                }
                let tentative = g_score[current as usize] + self.edge_cost(edge, profile);
                if tentative < g_score[neighbor as usize] {
                    g_score[neighbor as usize] = tentative;
                    came_from[neighbor as usize] = Some((current, start_idx + offset));
                    let f = tentative
                        + heuristic(self.nodes[neighbor as usize].coord, goal_coord, profile);
                    open.push(HeapItem { f, node: neighbor });
                }
            }
        }

        Err(otp_core::Error::NotFound(
            "no walking path between origin and destination".to_string(),
        ))
    }

    /// A* の `came_from` チェーンを辿って [`WalkPath`] を組み立てる。
    /// `WalkPath` のノード列を座標列に変換する (地図表示用の折れ線ジオメトリ)。
    /// 始点→終点の順。ノードが1点以下なら空/単点になる。
    pub fn path_coords(&self, path: &WalkPath) -> Vec<LatLng> {
        path.nodes.iter().map(|&n| self.nodes[n as usize].coord).collect()
    }

    fn reconstruct_path(
        &self,
        goal: NodeId,
        came_from: &[Option<(NodeId, usize)>],
        g_score: &[f32],
    ) -> WalkPath {
        let mut nodes = vec![goal];
        let mut distance_m = 0.0f32;
        let mut has_stairs = false;
        let mut cur = goal;
        while let Some((parent, edge_idx)) = came_from[cur as usize] {
            let edge = &self.edges[edge_idx];
            distance_m += edge.length_m;
            has_stairs |= edge.has_stairs;
            nodes.push(parent);
            cur = parent;
        }
        nodes.reverse();
        WalkPath {
            nodes,
            distance_m,
            duration_s: g_score[goal as usize],
            physical_duration_s: 0.0, // route() が呼び出し後に埋める (ここでは profile を持たない)
            has_stairs,
        }
    }
}

/// A* の優先度キュー要素。f 値が小さいほど優先 (`BinaryHeap` は max-heap なので反転)。
struct HeapItem {
    f: f32,
    node: NodeId,
}

impl PartialEq for HeapItem {
    fn eq(&self, other: &Self) -> bool {
        self.f == other.f
    }
}
impl Eq for HeapItem {}
impl PartialOrd for HeapItem {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for HeapItem {
    fn cmp(&self, other: &Self) -> Ordering {
        other.f.total_cmp(&self.f) // 反転 = 最小値が pop される
    }
}

/// 残り区間の直線距離ヒューリスティック (秒相当)。
fn heuristic(from: LatLng, goal: LatLng, profile: &WalkProfile) -> f32 {
    (from.haversine_m(&goal) as f32) / profile.speed_mps
}

/// 歩行経路の結果。
#[derive(Debug, Clone)]
pub struct WalkPath {
    pub nodes: Vec<NodeId>,
    pub distance_m: f32,
    /// 探索用の一般化コスト (秒相当)。階段忌避・不明ペナルティ・勾配ペナルティの
    /// 乗数が織り込まれており、実際の壁時計時間ではない (`edge_cost` 参照)。
    /// A* の最適性判定・経路選択にはこちらを使う。
    pub duration_s: f32,
    /// 実際の壁時計所要時間 (秒) = `distance_m / profile.speed_mps`。
    /// UI表示や otp-engine の RAPTOR access/egress 秒数 (`StreetLink::duration_s`)
    /// にはこちらを使う (`duration_s` はペナルティ込みで実時間より長く出るため
    /// そのまま使うと乗り遅れ判定等がずれる)。
    pub physical_duration_s: f32,
    /// 経路に階段が含まれるか (UI の警告用)。
    pub has_stairs: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edge(stairs: bool, wheelchair: Option<bool>) -> StreetEdge {
        StreetEdge {
            from: 0,
            to: 1,
            length_m: 100.0,
            has_stairs: stairs,
            has_elevator: false,
            max_slope_pct: None,
            wheelchair,
            geometry_ref: None,
        }
    }

    #[test]
    fn stroller_avoids_stairs_more_than_normal() {
        let g = StreetGraph::default();
        let stairs = edge(true, None);
        let normal_cost = g.edge_cost(&stairs, &WalkProfile::normal());
        let stroller_cost = g.edge_cost(&stairs, &WalkProfile::stroller());
        assert!(stroller_cost > normal_cost * 5.0, "stroller should heavily avoid stairs");
    }

    #[test]
    fn wheelchair_penalizes_more_than_stroller_on_stairs() {
        let g = StreetGraph::default();
        let stairs = edge(true, None);
        let stroller = g.edge_cost(&stairs, &WalkProfile::stroller());
        let wheelchair = g.edge_cost(&stairs, &WalkProfile::wheelchair());
        assert!(wheelchair > stroller);
    }
}
