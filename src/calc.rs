//! A small calculator for the search box: `24*18+7`, `2^10`, `sqrt(2)/2`,
//! `0xff + 1`, `15% of 80`... Recursive descent over the usual precedence:
//!
//! ```text
//! expr    = term   (('+' | '-') term)*
//! term    = unary  (('*' | '/' | '%' | 'mod') unary)*
//! unary   = ('-' | '+') unary | power
//! power   = postfix ('^' unary)?          right-associative: 2^3^2 = 2^9
//! postfix = primary '!'*                  factorial
//! primary = number | constant | function '(' args ')' | '(' expr ')'
//! ```
//!
//! `×`, `÷` and `**` are accepted too, and `x` works as multiplication
//! between numbers (`3x4`). Only input that does some math is treated as a
//! calculation: a bare number or a word is not.

#[derive(Clone, Copy, Debug, PartialEq)]
enum Tok {
    Num(f64),
    Ident(usize, usize), // byte range into the input
    Op(char),
    LParen,
    RParen,
    Comma,
}

fn lex(s: &str) -> Option<Vec<Tok>> {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        match c {
            b' ' | b'\t' => i += 1,
            b'0' if i + 1 < b.len() && matches!(b[i + 1], b'x' | b'X' | b'b' | b'B') => {
                let radix = if matches!(b[i + 1], b'x' | b'X') { 16 } else { 2 };
                let start = i + 2;
                let mut j = start;
                while j < b.len() && (b[j] as char).is_digit(radix) {
                    j += 1;
                }
                if j == start {
                    return None;
                }
                out.push(Tok::Num(u64::from_str_radix(&s[start..j], radix).ok()? as f64));
                i = j;
            }
            b'0'..=b'9' | b'.' => {
                let mut j = i;
                while j < b.len() && (b[j].is_ascii_digit() || b[j] == b'.' || b[j] == b'_') {
                    j += 1;
                }
                // Exponent: 1e3, 2.5E-4. A lone "e" after a number is not one.
                if j < b.len() && matches!(b[j], b'e' | b'E') {
                    let mut k = j + 1;
                    if k < b.len() && matches!(b[k], b'+' | b'-') {
                        k += 1;
                    }
                    if k < b.len() && b[k].is_ascii_digit() {
                        while k < b.len() && b[k].is_ascii_digit() {
                            k += 1;
                        }
                        j = k;
                    }
                }
                let text: String = s[i..j].chars().filter(|&c| c != '_').collect();
                out.push(Tok::Num(text.parse().ok()?));
                i = j;
            }
            b'a'..=b'z' | b'A'..=b'Z' => {
                let mut j = i;
                while j < b.len() && (b[j].is_ascii_alphanumeric()) {
                    j += 1;
                }
                out.push(Tok::Ident(i, j));
                i = j;
            }
            b'+' | b'-' | b'/' | b'%' | b'^' | b'!' => {
                out.push(Tok::Op(c as char));
                i += 1;
            }
            b'*' => {
                if b.get(i + 1) == Some(&b'*') {
                    out.push(Tok::Op('^'));
                    i += 2;
                } else {
                    out.push(Tok::Op('*'));
                    i += 1;
                }
            }
            b'(' => {
                out.push(Tok::LParen);
                i += 1;
            }
            b')' => {
                out.push(Tok::RParen);
                i += 1;
            }
            b',' => {
                out.push(Tok::Comma);
                i += 1;
            }
            _ => {
                let ch = s[i..].chars().next()?;
                match ch {
                    '×' | '·' => out.push(Tok::Op('*')),
                    '÷' => out.push(Tok::Op('/')),
                    '−' => out.push(Tok::Op('-')),
                    _ => return None,
                }
                i += ch.len_utf8();
            }
        }
    }
    Some(out)
}

struct Parser<'a> {
    src: &'a str,
    toks: Vec<Tok>,
    pos: usize,
    /// Operators, functions and constants seen: zero means "just a number".
    work: usize,
}

impl Parser<'_> {
    fn peek(&self) -> Option<Tok> {
        self.toks.get(self.pos).copied()
    }

    fn ident(&self, t: Tok) -> Option<String> {
        match t {
            Tok::Ident(a, b) => Some(self.src[a..b].to_ascii_lowercase()),
            _ => None,
        }
    }

    fn is_word(&self, word: &str) -> bool {
        self.peek().and_then(|t| self.ident(t)).as_deref() == Some(word)
    }

    fn expr(&mut self) -> Option<f64> {
        let mut v = self.term()?;
        while let Some(Tok::Op(op @ ('+' | '-'))) = self.peek() {
            self.pos += 1;
            self.work += 1;
            let rhs = self.term()?;
            v = if op == '+' { v + rhs } else { v - rhs };
        }
        Some(v)
    }

    fn term(&mut self) -> Option<f64> {
        let mut v = self.unary()?;
        loop {
            let op = match self.peek() {
                Some(Tok::Op(op @ ('*' | '/' | '%'))) => op,
                _ if self.is_word("mod") => 'm',
                // "3x4": x between numbers multiplies.
                _ if self.is_word("x") => '*',
                Some(Tok::Ident(a, b)) if self.src[a..b].starts_with(['x', 'X']) && self.src[a + 1..b].bytes().all(|c| c.is_ascii_digit()) && b > a + 1 => {
                    // "3x4" lexes as Num(3), Ident("x4").
                    self.pos += 1;
                    self.work += 1;
                    v *= self.src[a + 1..b].parse::<f64>().ok()?;
                    continue;
                }
                _ => break,
            };
            self.pos += 1;
            self.work += 1;
            // "15% of 80" and "15% * 80" read % as percent, not modulo.
            if op == '%' && (self.is_word("of") || self.at_term_end()) {
                if self.is_word("of") {
                    self.pos += 1;
                    v = v / 100.0 * self.unary()?;
                } else {
                    v /= 100.0;
                }
                continue;
            }
            let rhs = self.unary()?;
            v = match op {
                '*' => v * rhs,
                '/' => v / rhs,
                _ => v % rhs,
            };
        }
        Some(v)
    }

    /// True when nothing that could be a right-hand operand follows.
    fn at_term_end(&self) -> bool {
        matches!(self.peek(), None | Some(Tok::RParen) | Some(Tok::Comma) | Some(Tok::Op('+' | '-' | '*' | '/')))
    }

    fn unary(&mut self) -> Option<f64> {
        match self.peek() {
            Some(Tok::Op('-')) => {
                self.pos += 1;
                Some(-self.unary()?)
            }
            Some(Tok::Op('+')) => {
                self.pos += 1;
                self.unary()
            }
            _ => self.power(),
        }
    }

    fn power(&mut self) -> Option<f64> {
        let base = self.postfix()?;
        if let Some(Tok::Op('^')) = self.peek() {
            self.pos += 1;
            self.work += 1;
            return Some(base.powf(self.unary()?));
        }
        Some(base)
    }

    fn postfix(&mut self) -> Option<f64> {
        let mut v = self.primary()?;
        while let Some(Tok::Op('!')) = self.peek() {
            self.pos += 1;
            self.work += 1;
            if v < 0.0 || v.fract() != 0.0 || v > 170.0 {
                return None;
            }
            v = (1..=v as u64).map(|k| k as f64).product();
        }
        Some(v)
    }

    fn primary(&mut self) -> Option<f64> {
        let t = self.peek()?;
        self.pos += 1;
        match t {
            Tok::Num(v) => Some(v),
            Tok::LParen => {
                let v = self.expr()?;
                if self.peek() != Some(Tok::RParen) {
                    return None;
                }
                self.pos += 1;
                self.work += 1;
                Some(v)
            }
            Tok::Ident(..) => {
                let name = self.ident(t)?;
                self.work += 1;
                match name.as_str() {
                    "pi" => return Some(std::f64::consts::PI),
                    "e" => return Some(std::f64::consts::E),
                    "tau" => return Some(std::f64::consts::TAU),
                    _ => {}
                }
                if self.peek() != Some(Tok::LParen) {
                    return None;
                }
                self.pos += 1;
                let mut args = vec![self.expr()?];
                while self.peek() == Some(Tok::Comma) {
                    self.pos += 1;
                    args.push(self.expr()?);
                }
                if self.peek() != Some(Tok::RParen) {
                    return None;
                }
                self.pos += 1;
                call(&name, &args)
            }
            _ => None,
        }
    }
}

fn call(name: &str, a: &[f64]) -> Option<f64> {
    let one = |f: fn(f64) -> f64| (a.len() == 1).then(|| f(a[0]));
    match name {
        "sqrt" => one(f64::sqrt),
        "cbrt" => one(f64::cbrt),
        "abs" => one(f64::abs),
        "sin" => one(f64::sin),
        "cos" => one(f64::cos),
        "tan" => one(f64::tan),
        "asin" => one(f64::asin),
        "acos" => one(f64::acos),
        "atan" => one(f64::atan),
        "ln" => one(f64::ln),
        "log" | "log10" => one(f64::log10),
        "log2" => one(f64::log2),
        "exp" => one(f64::exp),
        "floor" => one(f64::floor),
        "ceil" => one(f64::ceil),
        "round" => one(f64::round),
        "min" => a.iter().copied().reduce(f64::min),
        "max" => a.iter().copied().reduce(f64::max),
        _ => None,
    }
}

/// Evaluates `s` if it is a calculation. Returns `None` for text that is not
/// math, for a bare number, and for results that are not finite.
pub fn eval(s: &str) -> Option<f64> {
    let s = s.trim().trim_start_matches('=').trim_end_matches('=').trim();
    if s.is_empty() || !s.bytes().any(|c| c.is_ascii_digit()) && !s.to_ascii_lowercase().contains("pi") {
        return None;
    }
    let toks = lex(s)?;
    let mut p = Parser { src: s, toks, pos: 0, work: 0 };
    let v = p.expr()?;
    if p.pos != p.toks.len() || p.work == 0 || !v.is_finite() {
        return None;
    }
    Some(if v == 0.0 { 0.0 } else { v }) // no "-0"
}

/// The value as you'd type it back: no grouping, up to 10 decimals, trailing
/// zeros dropped, scientific notation for very large or tiny values.
pub fn plain(v: f64) -> String {
    let a = v.abs();
    if a != 0.0 && !(1e-6..1e15).contains(&a) {
        return format!("{v:e}");
    }
    let s = format!("{v:.10}");
    let s = s.trim_end_matches('0').trim_end_matches('.');
    if s == "-0" { "0".to_string() } else { s.to_string() }
}

/// The value for display, with thousands separators: `1,234,567.5`.
pub fn pretty(v: f64) -> String {
    let p = plain(v);
    if p.contains('e') {
        return p;
    }
    let (sign, rest) = p.strip_prefix('-').map_or(("", p.as_str()), |r| ("-", r));
    let (int, frac) = rest.split_once('.').map_or((rest, None), |(i, f)| (i, Some(f)));
    let mut out = String::from(sign);
    for (i, c) in int.chars().enumerate() {
        if i > 0 && (int.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    if let Some(f) = frac {
        out.push('.');
        out.push_str(f);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn e(s: &str) -> Option<f64> {
        eval(s)
    }

    #[test]
    fn precedence_and_associativity() {
        assert_eq!(e("24*18+7"), Some(439.0));
        assert_eq!(e("2+3*4"), Some(14.0));
        assert_eq!(e("(2+3)*4"), Some(20.0));
        assert_eq!(e("2^3^2"), Some(512.0));
        assert_eq!(e("-2^2"), Some(-4.0));
        assert_eq!(e("2**10"), Some(1024.0));
        assert_eq!(e("10 - 2 - 3"), Some(5.0));
        assert_eq!(e("7 mod 3"), Some(1.0));
    }

    #[test]
    fn functions_constants_and_literals() {
        assert_eq!(e("sqrt(16)"), Some(4.0));
        assert_eq!(e("max(1, 9, 3)"), Some(9.0));
        assert_eq!(e("0xff + 1"), Some(256.0));
        assert_eq!(e("0b101"), None); // a bare literal is not a calculation
        assert_eq!(e("0b101 + 0"), Some(5.0));
        assert_eq!(e("1e3 / 4"), Some(250.0));
        assert_eq!(e("5!"), Some(120.0));
        assert!((e("2*pi").unwrap() - std::f64::consts::TAU).abs() < 1e-12);
        assert!((e("pi").unwrap() - std::f64::consts::PI).abs() < 1e-12);
    }

    #[test]
    fn percent_and_multiply_aliases() {
        assert_eq!(e("15% of 80"), Some(12.0));
        assert_eq!(e("50%"), Some(0.5));
        assert_eq!(e("200 * 15%"), Some(30.0));
        assert_eq!(e("10 % 4"), Some(2.0));
        assert_eq!(e("3x4"), Some(12.0));
        assert_eq!(e("3 x 4"), Some(12.0));
        assert_eq!(e("6 ÷ 3 × 2"), Some(4.0));
    }

    #[test]
    fn rejects_non_math() {
        for s in ["", "42", "chrome", "notepad++", "2024 report", "1/0", "sqrt(-1)", "(1+2", "1+", "foo(2)", "v2.1"] {
            assert_eq!(e(s), None, "{s}");
        }
    }

    #[test]
    fn formatting() {
        assert_eq!(plain(439.0), "439");
        assert_eq!(plain(0.1 + 0.2), "0.3");
        assert_eq!(plain(-2.5), "-2.5");
        assert_eq!(pretty(1234567.25), "1,234,567.25");
        assert_eq!(pretty(-1000.0), "-1,000");
        assert_eq!(plain(1e20), "1e20");
    }
}
