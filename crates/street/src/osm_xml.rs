//! 最小の OSM XML (`.osm`) パーサ (std のみ)。
//!
//! `.osm.pbf` (Protocol Buffers + zlib) を自前パースするのはコストが高いため、
//! otp-street は **前処理済みの OSM XML** を入力にする方針にした
//! (`../../../scripts/extract_osm_xml.sh` が `osmium` で bbox 抽出 + highway
//! タグフィルタ + XML 変換を行う。理由・代替案は README/コミットメッセージ参照)。
//!
//! OSM XML は要素のネストが node/way/relation → tag/nd/member の2階層までしかなく、
//! 属性値以外に生の `<` は現れない (エスケープされる) ので、フルスペックの XML
//! パーサは不要。`<...>` 区切りでタグを1個ずつ読むだけの手書きスキャナで十分。
//!
//! 対応範囲: `<node>` (id/lat/lon + 子 `<tag>`)、`<way>` (id + 子 `<nd>`/`<tag>`)。
//! `<relation>` は丸ごとスキップする (歩行グラフの構築には不要)。

/// OSM ノード (交差点・頂点)。
#[derive(Debug, Clone, PartialEq)]
pub struct OsmNode {
    pub id: i64,
    pub lat: f64,
    pub lon: f64,
}

/// OSM ウェイ (道路・歩道等の折れ線)。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct OsmWay {
    pub id: i64,
    pub nodes: Vec<i64>,
    pub tags: Vec<(String, String)>,
}

impl OsmWay {
    /// タグ値を引く (無ければ None)。
    pub fn tag(&self, key: &str) -> Option<&str> {
        self.tags
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }
}

/// パース結果。
#[derive(Debug, Default)]
pub struct OsmDoc {
    pub nodes: Vec<OsmNode>,
    pub ways: Vec<OsmWay>,
}

/// 開始タグの中身 (`node id="1" lat="2" lon="3"` のような、`<`/`>`/末尾 `/` を
/// 除いた文字列) を解析した結果。
struct StartTag<'a> {
    name: &'a str,
    self_closing: bool,
    closing: bool,
    attrs: Vec<(&'a str, String)>,
}

fn parse_start_tag(raw: &str) -> StartTag<'_> {
    let raw = raw.trim();
    let self_closing = raw.ends_with('/');
    let body = if self_closing {
        raw[..raw.len() - 1].trim_end()
    } else {
        raw
    };
    let closing = body.starts_with('/');
    let body = if closing { &body[1..] } else { body };

    let name_end = body.find(char::is_whitespace).unwrap_or(body.len());
    let name = &body[..name_end];
    let attrs = parse_attrs(&body[name_end..]);

    StartTag {
        name,
        self_closing,
        closing,
        attrs,
    }
}

/// `key="value"` (二重/単一引用符いずれも許容) の並びを読む。
fn parse_attrs(s: &str) -> Vec<(&str, String)> {
    let mut out = Vec::new();
    let bytes = s.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        let key_start = i;
        while i < bytes.len() && bytes[i] != b'=' && !bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if key_start == i {
            break; // これ以上 key= が無い
        }
        let key = &s[key_start..i];
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= bytes.len() || bytes[i] != b'=' {
            break; // 壊れた属性列。ここで打ち切る
        }
        i += 1; // '='
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= bytes.len() || (bytes[i] != b'"' && bytes[i] != b'\'') {
            break;
        }
        let quote = bytes[i];
        i += 1;
        let val_start = i;
        while i < bytes.len() && bytes[i] != quote {
            i += 1;
        }
        let raw_val = &s[val_start..i.min(s.len())];
        if i < bytes.len() {
            i += 1; // 閉じ引用符
        }
        out.push((key, unescape(raw_val)));
    }
    out
}

/// XML エンティティの最小デコード。
fn unescape(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        rest = &rest[amp..];
        if let Some(semi) = rest.find(';') {
            let entity = &rest[1..semi];
            let decoded = match entity {
                "amp" => Some('&'),
                "lt" => Some('<'),
                "gt" => Some('>'),
                "quot" => Some('"'),
                "apos" => Some('\''),
                _ if entity.starts_with('#') => entity[1..]
                    .trim_start_matches('x')
                    .parse::<u32>()
                    .ok()
                    .or_else(|| u32::from_str_radix(entity.trim_start_matches("#x"), 16).ok())
                    .and_then(char::from_u32),
                _ => None,
            };
            match decoded {
                Some(c) => {
                    out.push(c);
                    rest = &rest[semi + 1..];
                }
                None => {
                    // 未知のエンティティはそのまま出力して1文字だけ読み飛ばす
                    out.push('&');
                    rest = &rest[1..];
                }
            }
        } else {
            out.push_str(rest);
            rest = "";
            break;
        }
    }
    out.push_str(rest);
    out
}

fn attr<'a>(attrs: &'a [(&str, String)], key: &str) -> Option<&'a str> {
    attrs
        .iter()
        .find(|(k, _)| *k == key)
        .map(|(_, v)| v.as_str())
}

enum Context {
    None,
    Node(OsmNode, Vec<(String, String)>),
    Way(OsmWay),
    /// `<relation>` 等、興味の無い要素の中にいる (深さ1固定と仮定し、対応する
    /// 終了タグが来るまで無視する)。
    Skip(String),
}

/// OSM XML 全文をパースする。壊れた/対象外の要素は黙って無視する
/// (歩行グラフ構築に必要な最小限だけを拾えればよい)。
pub fn parse(input: &str) -> OsmDoc {
    let mut doc = OsmDoc::default();
    let mut ctx = Context::None;

    let mut rest = input;
    while let Some(lt) = rest.find('<') {
        rest = &rest[lt + 1..];
        let Some(gt) = rest.find('>') else { break };
        let raw = &rest[..gt];
        rest = &rest[gt + 1..];

        if raw.starts_with('?') || raw.starts_with('!') {
            continue; // XML宣言 / コメント / DOCTYPE
        }
        let tag = parse_start_tag(raw);

        if let Context::Skip(name) = &ctx {
            if tag.closing && tag.name == name {
                ctx = Context::None;
            }
            continue;
        }

        match (tag.name, tag.closing, tag.self_closing) {
            ("node", false, true) => {
                if let (Some(id), Some(lat), Some(lon)) = (
                    attr(&tag.attrs, "id"),
                    attr(&tag.attrs, "lat"),
                    attr(&tag.attrs, "lon"),
                ) {
                    if let (Ok(id), Ok(lat), Ok(lon)) = (id.parse(), lat.parse(), lon.parse()) {
                        doc.nodes.push(OsmNode { id, lat, lon });
                    }
                }
            }
            ("node", false, false) => {
                let node = match (
                    attr(&tag.attrs, "id"),
                    attr(&tag.attrs, "lat"),
                    attr(&tag.attrs, "lon"),
                ) {
                    (Some(id), Some(lat), Some(lon)) => {
                        match (id.parse(), lat.parse(), lon.parse()) {
                            (Ok(id), Ok(lat), Ok(lon)) => Some(OsmNode { id, lat, lon }),
                            _ => None,
                        }
                    }
                    _ => None,
                };
                match node {
                    Some(n) => ctx = Context::Node(n, Vec::new()),
                    None => ctx = Context::Skip("node".to_string()),
                }
            }
            ("node", true, _) => {
                if let Context::Node(n, tags) = std::mem::replace(&mut ctx, Context::None) {
                    let _ = tags; // ノードのタグは歩行グラフでは未使用 (将来: エレベーター等の点属性)
                    doc.nodes.push(n);
                }
            }
            ("way", false, false) => {
                let id = attr(&tag.attrs, "id")
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0);
                ctx = Context::Way(OsmWay {
                    id,
                    nodes: Vec::new(),
                    tags: Vec::new(),
                });
            }
            ("way", false, true) => {
                let id = attr(&tag.attrs, "id")
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0);
                doc.ways.push(OsmWay {
                    id,
                    nodes: Vec::new(),
                    tags: Vec::new(),
                });
            }
            ("way", true, _) => {
                if let Context::Way(w) = std::mem::replace(&mut ctx, Context::None) {
                    doc.ways.push(w);
                }
            }
            ("nd", false, true) => {
                if let Context::Way(w) = &mut ctx {
                    if let Some(r) = attr(&tag.attrs, "ref").and_then(|s| s.parse().ok()) {
                        w.nodes.push(r);
                    }
                }
            }
            ("tag", false, true) => {
                let (Some(k), Some(v)) = (attr(&tag.attrs, "k"), attr(&tag.attrs, "v")) else {
                    continue;
                };
                match &mut ctx {
                    Context::Way(w) => w.tags.push((k.to_string(), v.to_string())),
                    Context::Node(_, tags) => tags.push((k.to_string(), v.to_string())),
                    _ => {}
                }
            }
            ("relation", false, self_closing) => {
                // relation は歩行グラフに不要。self-closing (空 relation) でなければ
                // 対応する </relation> までスキップする。
                if !self_closing {
                    ctx = Context::Skip("relation".to_string());
                }
            }
            (_, false, _) => {
                // `<osm>` (ルート要素) や `<bounds/>` 等、node/way/nd/tag/relation
                // 以外の未知要素はそのまま無視する (コンテキストは変えない)。
            }
            (_, true, _) => {}
        }
    }
    doc
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_self_closing_node_without_tags() {
        let xml = r#"<osm><node id="1" lat="35.5" lon="139.5"/></osm>"#;
        let doc = parse(xml);
        assert_eq!(
            doc.nodes,
            vec![OsmNode {
                id: 1,
                lat: 35.5,
                lon: 139.5
            }]
        );
    }

    #[test]
    fn parses_node_with_child_tags() {
        let xml = r#"<osm><node id="2" lat="1.0" lon="2.0"><tag k="highway" v="traffic_signals"/></node></osm>"#;
        let doc = parse(xml);
        assert_eq!(
            doc.nodes,
            vec![OsmNode {
                id: 2,
                lat: 1.0,
                lon: 2.0
            }]
        );
    }

    #[test]
    fn parses_way_with_nodes_and_tags() {
        let xml = r#"<osm>
            <way id="10">
                <nd ref="1"/>
                <nd ref="2"/>
                <tag k="highway" v="footway"/>
                <tag k="wheelchair" v="yes"/>
            </way>
        </osm>"#;
        let doc = parse(xml);
        assert_eq!(doc.ways.len(), 1);
        let w = &doc.ways[0];
        assert_eq!(w.id, 10);
        assert_eq!(w.nodes, vec![1, 2]);
        assert_eq!(w.tag("highway"), Some("footway"));
        assert_eq!(w.tag("wheelchair"), Some("yes"));
        assert_eq!(w.tag("nonexistent"), None);
    }

    #[test]
    fn skips_relations_entirely() {
        let xml = r#"<osm>
            <relation id="1">
                <member type="way" ref="10" role="outer"/>
                <tag k="type" v="multipolygon"/>
            </relation>
            <way id="20"><nd ref="1"/><nd ref="2"/><tag k="highway" v="path"/></way>
        </osm>"#;
        let doc = parse(xml);
        assert_eq!(doc.ways.len(), 1);
        assert_eq!(doc.ways[0].id, 20);
    }

    #[test]
    fn unescapes_entities_in_tag_values() {
        let xml = r#"<osm><way id="1"><tag k="name" v="A &amp; B &quot;C&quot;"/></way></osm>"#;
        let doc = parse(xml);
        assert_eq!(doc.ways[0].tag("name"), Some("A & B \"C\""));
    }

    #[test]
    fn ignores_xml_declaration_and_comments() {
        let xml = "<?xml version='1.0' encoding='UTF-8'?>\n<!-- comment -->\n<osm><node id=\"1\" lat=\"0\" lon=\"0\"/></osm>";
        let doc = parse(xml);
        assert_eq!(doc.nodes.len(), 1);
    }
}
