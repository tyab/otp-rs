//! OSM 街路グラフと歩行ルーティング。**アクセシビリティ・コスト**がこのアプリの核。
//!
//! OTP の `street` / `astar` / `WheelchairPreferences` 相当。段差・エレベーター・勾配を
//! エッジ属性として持ち、プロファイル (通常/ベビーカー/車いす) ごとにコストを変える。

use otp_core::LatLng;

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

impl StreetGraph {
    /// OSM (.osm.pbf) から歩行グラフを構築する。
    ///
    /// TODO(移植): pbf パース → highway 抽出 → 頂点/エッジ化 → アクセシビリティ属性
    /// (steps/elevator/wheelchair/incline) 付与 → CSR 化。まずは東京都心 bbox。
    pub fn build_from_osm(_pbf: &std::path::Path) -> otp_core::Result<StreetGraph> {
        Err(otp_core::Error::Unimplemented("StreetGraph::build_from_osm"))
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
    /// TODO(移植): 最近傍ノードへのスナップ + `edge_cost` を重みにした A*。
    pub fn route(&self, _from: LatLng, _to: LatLng, _profile: &WalkProfile) -> otp_core::Result<WalkPath> {
        Err(otp_core::Error::Unimplemented("StreetGraph::route"))
    }
}

/// 歩行経路の結果。
#[derive(Debug, Clone)]
pub struct WalkPath {
    pub nodes: Vec<NodeId>,
    pub distance_m: f32,
    pub duration_s: f32,
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
