use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    Seq(Vec<Value>),
    Map(BTreeMap<String, Value>),
}

impl Value {
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Self::Bool(v) => Some(*v),
            _ => None,
        }
    }

    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Self::Int(v) => Some(*v),
            _ => None,
        }
    }

    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Self::Float(v) => Some(*v),
            Self::Int(v) => Some(*v as f64),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::Str(v) => Some(v.as_str()),
            _ => None,
        }
    }

    pub fn as_seq(&self) -> Option<&[Value]> {
        match self {
            Self::Seq(v) => Some(v.as_slice()),
            _ => None,
        }
    }

    pub fn as_map(&self) -> Option<&BTreeMap<String, Value>> {
        match self {
            Self::Map(v) => Some(v),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    Syntax { line: usize, detail: String },
    DuplicateKey { line: usize, key: String },
}

impl Error {
    fn syntax(line: usize, detail: impl Into<String>) -> Self {
        Self::Syntax {
            line,
            detail: detail.into(),
        }
    }
}

#[derive(Clone, Copy)]
struct Line<'a> {
    number: usize,
    indent: usize,
    trimmed: &'a str,
}

pub fn parse(input: &str) -> Result<Value, Error> {
    let raw_lines: Vec<&str> = input
        .split('\n')
        .map(|line| line.strip_suffix('\r').unwrap_or(line))
        .collect();
    let mut parser = Parser {
        lines: &raw_lines,
        index: 0,
    };
    parser.skip_ignorable()?;
    if parser.index < parser.lines.len() {
        let trimmed = strip_line_comment(parser.lines[parser.index].trim_start());
        if trimmed == "---" {
            parser.index += 1;
            parser.skip_ignorable()?;
        }
    }
    if parser.index >= parser.lines.len() {
        return Ok(Value::Null);
    }
    let first = parser
        .peek_line()?
        .ok_or_else(|| Error::syntax(1, "empty"))?;
    let root = parser.parse_node(first.indent, true)?;
    parser.skip_ignorable()?;
    if parser.index < parser.lines.len() {
        let rest = strip_line_comment(parser.lines[parser.index].trim_start());
        if rest == "..." {
            parser.index += 1;
            parser.skip_ignorable()?;
        }
    }
    if let Some(extra) = parser.peek_line()? {
        return Err(Error::syntax(
            extra.number,
            "unexpected trailing YAML content",
        ));
    }
    Ok(root)
}

struct Parser<'a> {
    lines: &'a [&'a str],
    index: usize,
}

impl<'a> Parser<'a> {
    fn skip_ignorable(&mut self) -> Result<(), Error> {
        while self.index < self.lines.len() {
            let raw = self.lines[self.index];
            let number = self.index + 1;
            let spaces = raw.bytes().take_while(|b| *b == b' ').count();
            let rest = &raw[spaces..];
            if rest.is_empty() || rest.starts_with('#') {
                self.index += 1;
                continue;
            }
            if rest.starts_with('\t') {
                return Err(Error::syntax(
                    number,
                    "tabs are not permitted for indentation",
                ));
            }
            break;
        }
        Ok(())
    }

    fn peek_line(&mut self) -> Result<Option<Line<'a>>, Error> {
        self.skip_ignorable()?;
        if self.index >= self.lines.len() {
            return Ok(None);
        }
        let raw = self.lines[self.index];
        let number = self.index + 1;
        let indent = raw.bytes().take_while(|b| *b == b' ').count();
        let trimmed = strip_line_comment(&raw[indent..]).trim_end();
        if trimmed == "..." {
            return Ok(None);
        }
        Ok(Some(Line {
            number,
            indent,
            trimmed,
        }))
    }

    fn parse_node(&mut self, min_indent: usize, allow_equal_seq: bool) -> Result<Value, Error> {
        let Some(line) = self.peek_line()? else {
            return Ok(Value::Null);
        };
        if line.indent < min_indent {
            return Ok(Value::Null);
        }
        if is_seq_indicator(line.trimmed) {
            if !allow_equal_seq && line.indent == min_indent {
                return Err(Error::syntax(
                    line.number,
                    "sequence requires greater indentation here",
                ));
            }
            return self.parse_block_seq(line.indent);
        }
        if line.trimmed.starts_with('{') || line.trimmed.starts_with('[') {
            self.index += 1;
            return parse_flow_str(line.number, line.trimmed);
        }
        if split_mapping_entry(line.number, line.trimmed)?.is_some() {
            return self.parse_block_map(line.indent);
        }
        self.index += 1;
        if let Some(block) = parse_block_header(line.number, line.trimmed)? {
            return self.parse_block_scalar(line.indent, block);
        }
        parse_scalar(line.number, line.trimmed)
    }

    fn parse_block_seq(&mut self, seq_indent: usize) -> Result<Value, Error> {
        let mut items = Vec::new();
        while let Some(line) = self.peek_line()? {
            if line.indent != seq_indent || !is_seq_indicator(line.trimmed) {
                break;
            }
            self.index += 1;
            let after_dash = line.trimmed[1..].trim_start_matches(' ');
            let consumed = line.trimmed.len() - after_dash.len();
            let item_indent = line.indent + consumed;
            if after_dash.is_empty() {
                let next = self.peek_line()?;
                match next {
                    Some(nl) if nl.indent > seq_indent => {
                        items.push(self.parse_node(nl.indent, true)?);
                    }
                    _ => items.push(Value::Null),
                }
            } else if after_dash.starts_with('{') || after_dash.starts_with('[') {
                items.push(parse_flow_str(line.number, after_dash)?);
            } else if let Some(block) = parse_block_header(line.number, after_dash)? {
                items.push(self.parse_block_scalar(seq_indent, block)?);
            } else if let Some((key, rest)) = split_mapping_entry(line.number, after_dash)? {
                let mut map = BTreeMap::new();
                let first_val = self.parse_mapping_rhs(line.number, item_indent, rest)?;
                map.insert(key, first_val);
                self.parse_block_map_into(item_indent, &mut map)?;
                items.push(Value::Map(map));
            } else {
                items.push(parse_scalar(line.number, after_dash)?);
            }
        }
        Ok(Value::Seq(items))
    }

    fn parse_block_map(&mut self, map_indent: usize) -> Result<Value, Error> {
        let mut map = BTreeMap::new();
        self.parse_block_map_into(map_indent, &mut map)?;
        Ok(Value::Map(map))
    }

    fn parse_block_map_into(
        &mut self,
        map_indent: usize,
        map: &mut BTreeMap<String, Value>,
    ) -> Result<(), Error> {
        while let Some(line) = self.peek_line()? {
            if line.indent < map_indent {
                break;
            }
            if line.indent > map_indent {
                return Err(Error::syntax(
                    line.number,
                    "unexpected indentation inside mapping",
                ));
            }
            if is_seq_indicator(line.trimmed) {
                break;
            }
            let Some((key, rest)) = split_mapping_entry(line.number, line.trimmed)? else {
                return Err(Error::syntax(line.number, "expected mapping key: value"));
            };
            self.index += 1;
            let value = self.parse_mapping_rhs(line.number, map_indent, rest)?;
            if map.insert(key.clone(), value).is_some() {
                return Err(Error::DuplicateKey {
                    line: line.number,
                    key,
                });
            }
        }
        Ok(())
    }

    fn parse_mapping_rhs(
        &mut self,
        line_number: usize,
        key_indent: usize,
        rest: &str,
    ) -> Result<Value, Error> {
        let rest = rest.trim_start_matches(' ');
        if rest.is_empty() {
            let Some(next) = self.peek_line()? else {
                return Ok(Value::Null);
            };
            if next.indent > key_indent {
                return self.parse_node(next.indent, true);
            }
            if next.indent == key_indent && is_seq_indicator(next.trimmed) {
                return self.parse_block_seq(next.indent);
            }
            return Ok(Value::Null);
        }
        if rest.starts_with('{') || rest.starts_with('[') {
            return parse_flow_str(line_number, rest);
        }
        if let Some(block) = parse_block_header(line_number, rest)? {
            return self.parse_block_scalar(key_indent, block);
        }
        parse_scalar(line_number, rest)
    }

    fn parse_block_scalar(
        &mut self,
        parent_indent: usize,
        header: BlockHeader,
    ) -> Result<Value, Error> {
        let mut collected: Vec<(usize, &str)> = Vec::new();
        let mut content_indent = header.explicit_indent.map(|n| parent_indent + n);

        while self.index < self.lines.len() {
            let raw = self.lines[self.index];
            let spaces = raw.bytes().take_while(|b| *b == b' ').count();
            let tail = &raw[spaces..];
            if tail.is_empty() {
                collected.push((spaces, raw));
                self.index += 1;
                continue;
            }
            let min_req = content_indent.unwrap_or(parent_indent + 1);
            if spaces < min_req {
                break;
            }
            if content_indent.is_none() {
                content_indent = Some(spaces);
            }
            collected.push((spaces, raw));
            self.index += 1;
        }

        let Some(indent) = content_indent else {
            return Ok(Value::Str(String::new()));
        };
        let trailing_empty = collected
            .iter()
            .rev()
            .take_while(|(_, raw)| raw.trim_matches(' ').is_empty())
            .count();
        let body_end = collected.len() - trailing_empty;
        let body = &collected[..body_end];
        if body.is_empty() {
            let out = match header.chomp {
                Chomp::Keep => "\n".repeat(trailing_empty),
                Chomp::Clip | Chomp::Strip => String::new(),
            };
            return Ok(Value::Str(out));
        }

        let mut out = String::new();
        if header.folded {
            let mut prev_blank = false;
            let mut prev_more_indented = false;
            for (idx, (spaces, raw)) in body.iter().enumerate() {
                let text = if *spaces >= indent {
                    &raw[indent..]
                } else {
                    ""
                };
                let is_blank = text.is_empty();
                let more_indented = !is_blank && text.starts_with(' ');
                if idx > 0 {
                    if is_blank || prev_blank || more_indented || prev_more_indented {
                        out.push('\n');
                    } else {
                        out.push(' ');
                    }
                }
                out.push_str(text);
                prev_blank = is_blank;
                prev_more_indented = more_indented;
            }
        } else {
            for (idx, (spaces, raw)) in body.iter().enumerate() {
                if idx > 0 {
                    out.push('\n');
                }
                if *spaces >= indent {
                    out.push_str(&raw[indent..]);
                }
            }
        }
        match header.chomp {
            Chomp::Strip => {}
            Chomp::Clip => out.push('\n'),
            Chomp::Keep => {
                out.push('\n');
                for _ in 0..trailing_empty {
                    out.push('\n');
                }
            }
        }
        Ok(Value::Str(out))
    }
}

#[derive(Clone, Copy)]
enum Chomp {
    Clip,
    Strip,
    Keep,
}

#[derive(Clone, Copy)]
struct BlockHeader {
    folded: bool,
    chomp: Chomp,
    explicit_indent: Option<usize>,
}

fn parse_block_header(line: usize, s: &str) -> Result<Option<BlockHeader>, Error> {
    let mut chars = s.chars();
    let folded = match chars.next() {
        Some('|') => false,
        Some('>') => true,
        _ => return Ok(None),
    };
    let mut chomp = Chomp::Clip;
    let mut explicit_indent = None;
    for c in chars {
        match c {
            '-' => chomp = Chomp::Strip,
            '+' => chomp = Chomp::Keep,
            '1'..='9' if explicit_indent.is_none() => {
                explicit_indent = Some((c as u8 - b'0') as usize);
            }
            ' ' | '\t' => break,
            _ => {
                return Err(Error::syntax(line, "invalid block scalar header"));
            }
        }
    }
    Ok(Some(BlockHeader {
        folded,
        chomp,
        explicit_indent,
    }))
}

fn is_seq_indicator(trimmed: &str) -> bool {
    trimmed == "-" || trimmed.starts_with("- ")
}

fn strip_line_comment(s: &str) -> &str {
    let mut in_single = false;
    let mut in_double = false;
    let mut escaped = false;
    let mut prev_space = true;
    for (idx, ch) in s.char_indices() {
        if in_single {
            if ch == '\'' {
                in_single = false;
            }
            prev_space = false;
            continue;
        }
        if in_double {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_double = false;
            }
            prev_space = false;
            continue;
        }
        match ch {
            '\'' => {
                in_single = true;
                prev_space = false;
            }
            '"' => {
                in_double = true;
                prev_space = false;
            }
            '#' if prev_space => return &s[..idx],
            ' ' | '\t' => prev_space = true,
            _ => prev_space = false,
        }
    }
    s
}

fn split_mapping_entry(line: usize, s: &str) -> Result<Option<(String, &str)>, Error> {
    if s.starts_with('\'') {
        let (key, rest) = parse_single_quoted_prefix(line, s)?;
        let rest = rest.trim_start_matches(' ');
        if let Some(after) = rest.strip_prefix(':')
            && (after.is_empty() || after.starts_with(' ') || after.starts_with('\t'))
        {
            return Ok(Some((key, after)));
        }
        return Ok(None);
    }
    if s.starts_with('"') {
        let (key, rest) = parse_double_quoted_prefix(line, s)?;
        let rest = rest.trim_start_matches(' ');
        if let Some(after) = rest.strip_prefix(':')
            && (after.is_empty() || after.starts_with(' ') || after.starts_with('\t'))
        {
            return Ok(Some((key, after)));
        }
        return Ok(None);
    }
    let bytes = s.as_bytes();
    for i in 0..bytes.len() {
        if bytes[i] == b':' && (i + 1 == bytes.len() || matches!(bytes[i + 1], b' ' | b'\t')) {
            let key = s[..i].trim_end_matches([' ', '\t']);
            if key.is_empty() {
                return Err(Error::syntax(line, "mapping key must not be empty"));
            }
            return Ok(Some((key.to_owned(), &s[i + 1..])));
        }
    }
    Ok(None)
}

fn parse_single_quoted_prefix(line: usize, s: &str) -> Result<(String, &str), Error> {
    let rest = s
        .strip_prefix('\'')
        .ok_or_else(|| Error::syntax(line, "expected single quote"))?;
    let mut out = String::new();
    let mut chars = rest.char_indices().peekable();
    while let Some((idx, ch)) = chars.next() {
        if ch == '\'' {
            if let Some((_, '\'')) = chars.peek().copied() {
                chars.next();
                out.push('\'');
            } else {
                return Ok((out, &rest[idx + 1..]));
            }
        } else {
            out.push(ch);
        }
    }
    Err(Error::syntax(line, "unterminated single-quoted string"))
}

fn parse_double_quoted_prefix(line: usize, s: &str) -> Result<(String, &str), Error> {
    let rest = s
        .strip_prefix('"')
        .ok_or_else(|| Error::syntax(line, "expected double quote"))?;
    let mut out = String::new();
    let mut chars = rest.char_indices();
    while let Some((idx, ch)) = chars.next() {
        match ch {
            '"' => return Ok((out, &rest[idx + 1..])),
            '\\' => {
                let (_, esc) = chars
                    .next()
                    .ok_or_else(|| Error::syntax(line, "unfinished escape in string"))?;
                match esc {
                    '\\' => out.push('\\'),
                    '"' => out.push('"'),
                    '/' => out.push('/'),
                    '0' => out.push('\0'),
                    'b' => out.push('\u{0008}'),
                    'f' => out.push('\u{000C}'),
                    'n' => out.push('\n'),
                    'r' => out.push('\r'),
                    't' => out.push('\t'),
                    'x' => out.push(read_hex_char(line, &mut chars, 2)?),
                    'u' => out.push(read_hex_char(line, &mut chars, 4)?),
                    'U' => out.push(read_hex_char(line, &mut chars, 8)?),
                    other => {
                        return Err(Error::syntax(
                            line,
                            format!("unsupported escape '\\{other}'"),
                        ));
                    }
                }
            }
            other => out.push(other),
        }
    }
    Err(Error::syntax(line, "unterminated double-quoted string"))
}

fn read_hex_char(
    line: usize,
    chars: &mut std::str::CharIndices<'_>,
    digits: usize,
) -> Result<char, Error> {
    let mut buf = String::with_capacity(digits);
    for _ in 0..digits {
        let (_, c) = chars
            .next()
            .ok_or_else(|| Error::syntax(line, "incomplete hex escape"))?;
        buf.push(c);
    }
    let code =
        u32::from_str_radix(&buf, 16).map_err(|_| Error::syntax(line, "invalid hex escape"))?;
    char::from_u32(code).ok_or_else(|| Error::syntax(line, "invalid Unicode scalar value"))
}

fn parse_scalar(line: usize, s: &str) -> Result<Value, Error> {
    let s = s.trim();
    if s.is_empty() {
        return Ok(Value::Null);
    }
    if s.starts_with('\'') {
        let (val, rem) = parse_single_quoted_prefix(line, s)?;
        if !rem.trim().is_empty() {
            return Err(Error::syntax(
                line,
                "trailing characters after quoted string",
            ));
        }
        return Ok(Value::Str(val));
    }
    if s.starts_with('"') {
        let (val, rem) = parse_double_quoted_prefix(line, s)?;
        if !rem.trim().is_empty() {
            return Err(Error::syntax(
                line,
                "trailing characters after quoted string",
            ));
        }
        return Ok(Value::Str(val));
    }
    match s {
        "~" | "null" | "Null" | "NULL" => Ok(Value::Null),
        "true" | "True" | "TRUE" => Ok(Value::Bool(true)),
        "false" | "False" | "FALSE" => Ok(Value::Bool(false)),
        _ => {
            if is_plain_decimal_int(s)
                && let Ok(v) = s.parse::<i64>()
            {
                return Ok(Value::Int(v));
            }
            if is_plain_float(s)
                && let Ok(v) = s.parse::<f64>()
                && v.is_finite()
            {
                return Ok(Value::Float(v));
            }
            Ok(Value::Str(s.to_owned()))
        }
    }
}

fn is_plain_decimal_int(s: &str) -> bool {
    let digits = s.strip_prefix(['+', '-']).unwrap_or(s);
    digits == "0"
        || (!digits.is_empty()
            && !digits.starts_with('0')
            && digits.bytes().all(|b| b.is_ascii_digit()))
}

fn is_plain_float(s: &str) -> bool {
    let body = s.strip_prefix(['+', '-']).unwrap_or(s);
    !body.is_empty()
        && (body.contains('.') || body.contains('e') || body.contains('E'))
        && body
            .bytes()
            .all(|b| b.is_ascii_digit() || matches!(b, b'.' | b'e' | b'E' | b'+' | b'-'))
}

fn parse_flow_str(line: usize, s: &str) -> Result<Value, Error> {
    let (val, rest) = parse_flow_value(line, s.trim())?;
    if !rest.trim().is_empty() {
        return Err(Error::syntax(line, "trailing characters after flow value"));
    }
    Ok(val)
}

fn parse_flow_value(line: usize, s: &str) -> Result<(Value, &str), Error> {
    let s = s.trim_start();
    if let Some(mut rest) = s.strip_prefix('[') {
        let mut items = Vec::new();
        rest = rest.trim_start();
        if let Some(after) = rest.strip_prefix(']') {
            return Ok((Value::Seq(items), after));
        }
        loop {
            let (item, after) = parse_flow_value(line, rest)?;
            items.push(item);
            rest = after.trim_start();
            if let Some(after_close) = rest.strip_prefix(']') {
                return Ok((Value::Seq(items), after_close));
            }
            rest = rest
                .strip_prefix(',')
                .ok_or_else(|| Error::syntax(line, "expected ',' or ']' in flow sequence"))?
                .trim_start();
            if let Some(after_close) = rest.strip_prefix(']') {
                return Ok((Value::Seq(items), after_close));
            }
        }
    }
    if let Some(mut rest) = s.strip_prefix('{') {
        let mut map = BTreeMap::new();
        rest = rest.trim_start();
        if let Some(after) = rest.strip_prefix('}') {
            return Ok((Value::Map(map), after));
        }
        loop {
            let (key, after_key) = parse_flow_key(line, rest)?;
            rest = after_key.trim_start();
            rest = rest
                .strip_prefix(':')
                .ok_or_else(|| Error::syntax(line, "expected ':' in flow mapping"))?
                .trim_start();
            let (val, after_val) = parse_flow_value(line, rest)?;
            if map.insert(key.clone(), val).is_some() {
                return Err(Error::DuplicateKey { line, key });
            }
            rest = after_val.trim_start();
            if let Some(after_close) = rest.strip_prefix('}') {
                return Ok((Value::Map(map), after_close));
            }
            rest = rest
                .strip_prefix(',')
                .ok_or_else(|| Error::syntax(line, "expected ',' or '}' in flow mapping"))?
                .trim_start();
            if let Some(after_close) = rest.strip_prefix('}') {
                return Ok((Value::Map(map), after_close));
            }
        }
    }
    if s.starts_with('\'') {
        let (val, rest) = parse_single_quoted_prefix(line, s)?;
        return Ok((Value::Str(val), rest));
    }
    if s.starts_with('"') {
        let (val, rest) = parse_double_quoted_prefix(line, s)?;
        return Ok((Value::Str(val), rest));
    }
    let end = s.find([',', ']', '}']).unwrap_or(s.len());
    let token = s[..end].trim();
    if token.is_empty() {
        return Err(Error::syntax(line, "empty value in flow collection"));
    }
    Ok((parse_scalar(line, token)?, &s[end..]))
}

fn parse_flow_key(line: usize, s: &str) -> Result<(String, &str), Error> {
    let s = s.trim_start();
    if s.starts_with('\'') {
        return parse_single_quoted_prefix(line, s);
    }
    if s.starts_with('"') {
        return parse_double_quoted_prefix(line, s);
    }
    let end = s
        .find([':', ',', '}'])
        .ok_or_else(|| Error::syntax(line, "unterminated flow key"))?;
    let key = s[..end].trim();
    if key.is_empty() {
        return Err(Error::syntax(line, "empty flow key"));
    }
    Ok((key.to_owned(), &s[end..]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parses_remarshal_workers_and_perms_yaml() {
        let workers = "\
eval-worker:
  args:
  - description: ''
    name: checkpoint
    type: string
  description: Evaluate a checkpoint
  workflowType: EvalWorkflow
train-worker:
  args:
  - choices:
    - fast
    - accurate
    default: fast
    description: \"it's \\\"quoted\\\"\\nand Élan\"
    name: mode
    type: enum
  - default: 10
    max: 500
    min: 1
    name: epochs
    type: int
  - default: 0.05
    max: 1.0
    min: 0.0
    name: lr
    type: float
  - default: false
    name: dry_run
    type: bool
  description: 'it''s fine'
  workflowType: TrainWorkflow
";
        let doc = parse(workers).unwrap();
        let root = doc.as_map().unwrap();
        let train = root["train-worker"].as_map().unwrap();
        assert_eq!(train["workflowType"].as_str(), Some("TrainWorkflow"));
        assert_eq!(train["description"].as_str(), Some("it's fine"));
        let args = train["args"].as_seq().unwrap();
        assert_eq!(args.len(), 4);
        assert_eq!(
            args[0].as_map().unwrap()["description"].as_str(),
            Some("it's \"quoted\"\nand Élan")
        );
        assert_eq!(args[1].as_map().unwrap()["default"].as_i64(), Some(10));
        assert_eq!(args[2].as_map().unwrap()["default"].as_f64(), Some(0.05));
        assert_eq!(args[3].as_map().unwrap()["default"].as_bool(), Some(false));

        let perms = "\
permissions:
  read: true
  write: false
user: alice
workers: {}
";
        let perms_doc = parse(perms).unwrap();
        let perms_map = perms_doc.as_map().unwrap();
        assert_eq!(perms_map["user"].as_str(), Some("alice"));
        assert!(perms_map["workers"].as_map().unwrap().is_empty());
    }

    #[test]
    fn test_parses_flow_block_scalars_and_rejects_invalid_yaml() {
        let doc = parse("---\nlist: [1, 'true', \"\\u00e9\"]\nnote: |-\n  line one\n  line two\n")
            .unwrap();
        let map = doc.as_map().unwrap();
        let seq = map["list"].as_seq().unwrap();
        assert_eq!(seq[0].as_i64(), Some(1));
        assert_eq!(seq[1].as_str(), Some("true"));
        assert_eq!(seq[2].as_str(), Some("é"));
        assert_eq!(map["note"].as_str(), Some("line one\nline two"));

        for bad in ["a: 1\na: 2\n", "\tbad: 1\n", "x: 'unclosed\n", "x: [1, 2\n"] {
            assert!(parse(bad).is_err(), "{bad:?}");
        }
    }
}
