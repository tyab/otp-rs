//! RFC4180 風の最小 CSV パーサ (std のみ)。
//!
//! GTFS の実データを読むために必要な範囲だけ対応する:
//! - ダブルクォート引用フィールド、`""` エスケープ
//! - フィールド内の `,` / 改行の許容 (引用フィールド内に限る)
//! - レコード区切りは `\r\n` / `\n` の両方
//! - 先頭 UTF-8 BOM の除去
//! - 列順に依存しないヘッダ名引き (`Row::get`)
//! - 末尾の空行 (trailing blank line) は無視
//!
//! 対応範囲外 (GTFS では通常出現しない): マルチバイトの区切り文字、`\r` 単独改行
//! (古い Mac 形式)。

/// 生の CSV テキストをレコード (行 = フィールド列) に分解する。
/// 先頭行はヘッダとして扱わず、そのまま records[0] に入れる (呼び出し側で分離する)。
fn parse_records(input: &str) -> Vec<Vec<String>> {
    // UTF-8 BOM (U+FEFF) は `str` としてデコードされた時点で先頭の1文字になる。
    let input = input.strip_prefix('\u{feff}').unwrap_or(input);

    let mut records = Vec::new();
    let mut record: Vec<String> = Vec::new();
    let mut field = String::new();
    let mut in_quotes = false;

    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        if in_quotes {
            match c {
                '"' => {
                    if chars.peek() == Some(&'"') {
                        field.push('"');
                        chars.next();
                    } else {
                        in_quotes = false;
                    }
                }
                _ => field.push(c),
            }
        } else {
            match c {
                '"' => in_quotes = true,
                ',' => record.push(std::mem::take(&mut field)),
                '\r' => {
                    if chars.peek() == Some(&'\n') {
                        chars.next();
                    }
                    record.push(std::mem::take(&mut field));
                    records.push(std::mem::take(&mut record));
                }
                '\n' => {
                    record.push(std::mem::take(&mut field));
                    records.push(std::mem::take(&mut record));
                }
                _ => field.push(c),
            }
        }
    }
    // 末尾に改行が無いファイルの最終レコードを回収する。
    if !field.is_empty() || !record.is_empty() {
        record.push(field);
        records.push(record);
    }
    // 完全な空行 (フィールド1個・中身が空) は捨てる (末尾の trailing blank line 対策)。
    records.retain(|r| !(r.len() == 1 && r[0].is_empty()));
    records
}

/// ヘッダ名で列を引ける CSV テーブル。
#[derive(Debug, Default)]
pub struct Table {
    header: Vec<String>,
    rows: Vec<Vec<String>>,
}

impl Table {
    /// ファイル内容全体をパースする。
    pub fn parse(input: &str) -> Table {
        let mut records = parse_records(input);
        if records.is_empty() {
            return Table::default();
        }
        let header = records.remove(0);
        Table { header, rows: records }
    }

    /// 空テーブル (ファイルが存在しない場合に使う)。
    pub fn empty() -> Table {
        Table::default()
    }

    fn col_index(&self, name: &str) -> Option<usize> {
        self.header.iter().position(|h| h == name)
    }

    pub fn iter(&self) -> impl Iterator<Item = Row<'_>> {
        self.rows.iter().map(move |cells| Row { table: self, cells })
    }
}

/// 1行分のビュー。列名で値を引く。
pub struct Row<'a> {
    table: &'a Table,
    cells: &'a [String],
}

impl<'a> Row<'a> {
    /// 列名で値を引く。列が無い/行が短い/値が空文字のいずれでも `None`。
    /// GTFS の「optional かつ空なら未指定」という慣習に合わせる。
    pub fn get(&self, col: &str) -> Option<&'a str> {
        let idx = self.table.col_index(col)?;
        self.cells.get(idx).map(|s| s.as_str()).filter(|s| !s.is_empty())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_simple_csv() {
        let t = Table::parse("a,b,c\n1,2,3\n4,5,6\n");
        let rows: Vec<_> = t.iter().collect();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].get("b"), Some("2"));
        assert_eq!(rows[1].get("a"), Some("4"));
    }

    #[test]
    fn strips_bom_and_handles_crlf() {
        let t = Table::parse("\u{feff}a,b\r\n1,2\r\n3,4\r\n");
        let rows: Vec<_> = t.iter().collect();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].get("a"), Some("1"));
    }

    #[test]
    fn handles_quoted_field_with_comma_and_escaped_quote() {
        let t = Table::parse("id,name\n1,\"Rapid \"\"Local\"\" Line, North\"\n");
        let rows: Vec<_> = t.iter().collect();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].get("name"), Some("Rapid \"Local\" Line, North"));
    }

    #[test]
    fn missing_column_and_empty_value_are_none() {
        let t = Table::parse("a,b\n1,\n");
        let rows: Vec<_> = t.iter().collect();
        assert_eq!(rows[0].get("b"), None); // 空文字は None 扱い
        assert_eq!(rows[0].get("c"), None); // 列自体が無い
    }

    #[test]
    fn ignores_trailing_blank_line() {
        let t = Table::parse("a,b\n1,2\n\n");
        assert_eq!(t.iter().count(), 1);
    }

    #[test]
    fn empty_input_yields_empty_table() {
        let t = Table::parse("");
        assert_eq!(t.iter().count(), 0);
    }
}
