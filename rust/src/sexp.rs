//! A small S-expression type, reader and writer, enough for KiCad files.

use serde_json::Value;

/// An S-expression.
///
/// `A` is a bare atom (written as it is unless the KiCad quoting rules say
/// otherwise), `Q` is a string that's always written in quotes, `N` is a
/// number and `L` is a list.
#[derive(Clone, Debug, PartialEq)]
pub enum Sexp {
    A(String),
    Q(String),
    N(f64),
    L(Vec<Sexp>),
}

impl From<&str> for Sexp {
    fn from(s: &str) -> Sexp {
        Sexp::A(s.to_string())
    }
}
impl From<String> for Sexp {
    fn from(s: String) -> Sexp {
        Sexp::A(s)
    }
}
impl From<&String> for Sexp {
    fn from(s: &String) -> Sexp {
        Sexp::A(s.clone())
    }
}
impl From<f64> for Sexp {
    fn from(n: f64) -> Sexp {
        Sexp::N(n)
    }
}
impl From<i64> for Sexp {
    fn from(n: i64) -> Sexp {
        Sexp::N(n as f64)
    }
}
impl From<i32> for Sexp {
    fn from(n: i32) -> Sexp {
        Sexp::N(n as f64)
    }
}
impl From<usize> for Sexp {
    fn from(n: usize) -> Sexp {
        Sexp::N(n as f64)
    }
}
impl From<Vec<Sexp>> for Sexp {
    fn from(v: Vec<Sexp>) -> Sexp {
        Sexp::L(v)
    }
}

/// Build a list S-expression: `sx!["at", x, y]`.
#[macro_export]
macro_rules! sx {
    ($($e:expr),* $(,)?) => {
        $crate::sexp::Sexp::L(vec![$($crate::sexp::Sexp::from($e)),*])
    };
}

/// A string that's always written quoted.
pub fn q(s: &str) -> Sexp {
    Sexp::Q(s.to_string())
}

impl Sexp {
    pub fn list(&self) -> Option<&Vec<Sexp>> {
        match self {
            Sexp::L(v) => Some(v),
            _ => None,
        }
    }

    pub fn list_mut(&mut self) -> Option<&mut Vec<Sexp>> {
        match self {
            Sexp::L(v) => Some(v),
            _ => None,
        }
    }

    pub fn is_list(&self) -> bool {
        matches!(self, Sexp::L(_))
    }

    /// The text of an atom or string.
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Sexp::A(s) | Sexp::Q(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Sexp::N(n) => Some(*n),
            Sexp::A(s) | Sexp::Q(s) => s.parse().ok(),
            _ => None,
        }
    }

    /// The text of anything that isn't a list, as Python's `str()` would give it.
    pub fn text(&self) -> String {
        match self {
            Sexp::A(s) | Sexp::Q(s) => s.clone(),
            Sexp::N(n) => fmt_num(*n),
            Sexp::L(_) => String::new(),
        }
    }

    /// The head (first element) of a list, if it's an atom.
    pub fn head(&self) -> Option<&str> {
        self.list().and_then(|v| v.first()).and_then(|h| h.as_str())
    }

    pub fn is(&self, head: &str) -> bool {
        self.head() == Some(head)
    }

    pub fn get(&self, i: usize) -> Option<&Sexp> {
        self.list().and_then(|v| v.get(i))
    }

    pub fn len(&self) -> usize {
        self.list().map_or(0, |v| v.len())
    }

    /// Element `i` as a number (0 if it's missing or isn't one).
    pub fn num(&self, i: usize) -> f64 {
        self.get(i).and_then(|e| e.as_f64()).unwrap_or(0.0)
    }

    /// Element `i` as text ("" if it's missing).
    pub fn str_at(&self, i: usize) -> &str {
        self.get(i).and_then(|e| e.as_str()).unwrap_or("")
    }

    pub fn push(&mut self, e: Sexp) {
        if let Sexp::L(v) = self {
            v.push(e);
        }
    }

    /// The first sub-list that starts with `head`.
    pub fn field(&self, head: &str) -> Option<&Sexp> {
        self.list()?.iter().find(|e| e.is(head))
    }

    pub fn field_mut(&mut self, head: &str) -> Option<&mut Sexp> {
        self.list_mut()?.iter_mut().find(|e| e.is(head))
    }

    /// All the sub-lists that start with `head`.
    pub fn fields<'a>(&'a self, head: &'a str) -> impl Iterator<Item = &'a Sexp> + 'a {
        self.list().into_iter().flatten().filter(move |e| e.is(head))
    }

    /// True if the list holds the bare atom `s`.
    pub fn has_atom(&self, s: &str) -> bool {
        self.list().is_some_and(|v| v.iter().any(|e| e.as_str() == Some(s)))
    }

    /// Convert a JSON value (nested lists of strings and numbers) into an S-expression.
    pub fn from_json(v: &Value) -> Sexp {
        match v {
            Value::Array(a) => Sexp::L(a.iter().map(Sexp::from_json).collect()),
            Value::String(s) => Sexp::A(s.clone()),
            Value::Number(n) => Sexp::N(n.as_f64().unwrap_or(0.0)),
            Value::Bool(b) => Sexp::A(if *b { "yes".into() } else { "no".into() }),
            Value::Null => Sexp::A(String::new()),
            Value::Object(_) => Sexp::L(vec![]),
        }
    }

    /// Parse the text of an S-expression.
    pub fn parse(text: &str) -> Option<Sexp> {
        let bytes = text.as_bytes();
        let mut pos = 0;
        let r = parse_item(bytes, &mut pos);
        r
    }

    /// Quote the strings KiCad wants quoted (see [`needs_quotes`]).
    pub fn add_kicad_quotes(&mut self) {
        if let Sexp::L(v) = self {
            let head = v.first().and_then(|h| h.as_str()).unwrap_or("").to_string();
            let stop = if needs_quotes(&head)
                || (head == "symbol" && v.len() > 1 && !v[1].is_list() && v[1].as_str().is_some())
            {
                v.len()
            } else if head == "alternate" {
                2.min(v.len())
            } else {
                0
            };
            for (i, e) in v.iter_mut().enumerate() {
                match e {
                    Sexp::A(s) if i >= 1 && i < stop => {
                        if !(s.len() >= 2 && s.starts_with('"') && s.ends_with('"')) {
                            *e = Sexp::Q(std::mem::take(s));
                        }
                    }
                    Sexp::L(_) => e.add_kicad_quotes(),
                    _ => {}
                }
            }
        }
    }

    /// Write the S-expression as text, one list per line, indented by depth.
    pub fn to_text(&self) -> String {
        let mut out = String::new();
        write_sexp(self, 0, &mut out);
        out.push('\n');
        out
    }
}

/// True for the list heads whose strings KiCad wants in quotes.
pub fn needs_quotes(tag: &str) -> bool {
    matches!(
        tag,
        "title"
            | "date"
            | "company"
            | "comment"
            | "path"
            | "project"
            | "property"
            | "name"
            | "number"
            | "lib_id"
            | "reference"
            | "label"
            | "global_label"
            | "hierarchical_label"
            | "generator"
            | "generator_version"
            | "paper"
            | "page"
            | "text"
    )
}

/// Format a number the short way: integers without a decimal point.
pub fn fmt_num(n: f64) -> String {
    if n == 0.0 {
        return "0".to_string();
    }
    if n.fract() == 0.0 && n.abs() < 1e15 {
        format!("{}", n as i64)
    } else {
        format!("{}", n)
    }
}

fn write_atom(s: &str, out: &mut String) {
    let bare_ok = !s.is_empty()
        && !s.chars().any(|c| c.is_whitespace() || c == '(' || c == ')')
        && (!s.contains('"') || (s.len() >= 2 && s.starts_with('"') && s.ends_with('"')));
    if bare_ok {
        out.push_str(s);
    } else {
        write_quoted(s, out);
    }
}

fn write_quoted(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            _ => out.push(c),
        }
    }
    out.push('"');
}

fn write_sexp(e: &Sexp, level: usize, out: &mut String) {
    match e {
        Sexp::A(s) => write_atom(s, out),
        Sexp::Q(s) => write_quoted(s, out),
        Sexp::N(n) => out.push_str(&fmt_num(*n)),
        Sexp::L(v) => {
            if level > 0 {
                out.push('\n');
                for _ in 0..level {
                    out.push_str("  ");
                }
            }
            out.push('(');
            for (i, item) in v.iter().enumerate() {
                if i > 0 && !item.is_list() {
                    out.push(' ');
                }
                if i == 0 {
                    // The head of a list is never quoted.
                    match item {
                        Sexp::A(s) | Sexp::Q(s) => out.push_str(s),
                        _ => write_sexp(item, level + 1, out),
                    }
                } else {
                    write_sexp(item, level + 1, out);
                }
            }
            out.push(')');
        }
    }
}

fn skip_ws(b: &[u8], pos: &mut usize) {
    while *pos < b.len() && (b[*pos] as char).is_whitespace() {
        *pos += 1;
    }
}

fn parse_item(b: &[u8], pos: &mut usize) -> Option<Sexp> {
    skip_ws(b, pos);
    if *pos >= b.len() {
        return None;
    }
    match b[*pos] {
        b'(' => {
            *pos += 1;
            let mut items = Vec::new();
            loop {
                skip_ws(b, pos);
                if *pos >= b.len() {
                    return Some(Sexp::L(items));
                }
                if b[*pos] == b')' {
                    *pos += 1;
                    return Some(Sexp::L(items));
                }
                items.push(parse_item(b, pos)?);
            }
        }
        b'"' => {
            *pos += 1;
            let mut s = Vec::new();
            while *pos < b.len() && b[*pos] != b'"' {
                if b[*pos] == b'\\' && *pos + 1 < b.len() {
                    *pos += 1;
                    match b[*pos] {
                        b'n' => s.push(b'\n'),
                        c => s.push(c),
                    }
                } else {
                    s.push(b[*pos]);
                }
                *pos += 1;
            }
            *pos += 1;
            Some(Sexp::Q(String::from_utf8_lossy(&s).into_owned()))
        }
        _ => {
            let start = *pos;
            while *pos < b.len() && !(b[*pos] as char).is_whitespace() && b[*pos] != b'(' && b[*pos] != b')' {
                *pos += 1;
            }
            let tok = std::str::from_utf8(&b[start..*pos]).ok()?;
            let numeric = tok
                .bytes()
                .next()
                .is_some_and(|c| c.is_ascii_digit() || c == b'-' || c == b'+' || c == b'.')
                && tok.bytes().any(|c| c.is_ascii_digit());
            if numeric {
                if let Ok(n) = tok.parse::<f64>() {
                    return Some(Sexp::N(n));
                }
            }
            Some(Sexp::A(tok.to_string()))
        }
    }
}
