//! Category: MATHX — extended math beyond plain arithmetic: safe expression
//! evaluation, percentages, descriptive statistics, gcd/lcm, prime checks +
//! factorization, factorial, combinations/permutations, quadratic solving, and
//! rounding modes. Pure + deterministic, with explicit overflow handling (a
//! result that would overflow is a friendly error, never a wrong wrapped value).
//!
//! Every skill is a total function of its JSON args: no network, no clock, no
//! randomness, no I/O. Algorithms are the real thing (Euclid's gcd, trial
//! division up to sqrt, the quadratic formula) — never an approximation that
//! lies — and bad/over-bounds input returns a friendly `Err`, never a panic.

use anyhow::{anyhow, Result};
use serde_json::Value;

use super::{Category, SkillDef};

/// The mathx catalog. The Library phase appends `SkillDef::new(...)` entries to
/// THIS vec (and nothing in mod.rs changes).
pub fn skills() -> Vec<SkillDef> {
    vec![
        SkillDef::new(
            "eval_expression",
            Category::Mathx,
            "Safely evaluate an arithmetic expression with + - * / % ^ and parentheses (e.g. \"2 + 3 * (4 - 1)\"). Use for a one-off calculation; pure, no external math engine.",
            &["calculate", "evaluate", "what is 2+2", "do the math", "compute"],
            eval_expression,
        ),
        SkillDef::new(
            "percentage",
            Category::Mathx,
            "Percentage helpers: percent-of, what-percent, percent-change, plus tip/discount/markup on an amount. Use for 'what is 15% of 80', 'X is what % of Y', '% change', or a tip/discount.",
            &["percent", "percentage", "tip", "discount", "markup", "percent change", "what percent"],
            percentage,
        ),
        SkillDef::new(
            "stats_summary",
            Category::Mathx,
            "Descriptive statistics for a list of numbers: count, sum, min, max, mean, median, mode, variance, and standard deviation. Use to summarize a dataset.",
            &["mean", "average", "median", "mode", "standard deviation", "stddev", "variance", "statistics"],
            stats_summary,
        ),
        SkillDef::new(
            "gcd_lcm",
            Category::Mathx,
            "Greatest common divisor and least common multiple of two or more integers. Use to reduce a fraction or find a common denominator/multiple.",
            &["gcd", "greatest common divisor", "lcm", "least common multiple", "common denominator"],
            gcd_lcm,
        ),
        SkillDef::new(
            "is_prime",
            Category::Mathx,
            "Test whether an integer is prime (trial division up to its square root). Use to check primality of a single number.",
            &["is prime", "prime check", "primality", "is it prime"],
            is_prime,
        ),
        SkillDef::new(
            "factorize",
            Category::Mathx,
            "Prime-factorize a positive integer into its prime power factors (e.g. 360 = 2^3 * 3^2 * 5). Use to break a number into primes.",
            &["factorize", "prime factors", "factor", "factorization", "break into primes"],
            factorize,
        ),
        SkillDef::new(
            "factorial",
            Category::Mathx,
            "Compute n! (the factorial of a non-negative integer) with overflow guarded. Use for a factorial; results that overflow 64-bit are a friendly error, never wrong.",
            &["factorial", "n!", "5 factorial"],
            factorial,
        ),
        SkillDef::new(
            "combinations",
            Category::Mathx,
            "Combinations nCr and permutations nPr — choose/arrange r items from n. Use for 'how many ways to choose', lottery odds, or arrangements.",
            &["nCr", "nPr", "combinations", "permutations", "choose", "how many ways"],
            combinations,
        ),
        SkillDef::new(
            "quadratic",
            Category::Mathx,
            "Solve a quadratic a*x^2 + b*x + c = 0, reporting real or complex roots via the discriminant. Use to find the roots of a quadratic equation.",
            &["quadratic", "solve quadratic", "roots", "ax^2+bx+c", "discriminant"],
            quadratic,
        ),
        SkillDef::new(
            "round_number",
            Category::Mathx,
            "Round a number to N decimal places using a chosen mode: half_up, half_even (banker's), floor, ceil, or trunc. Use when the rounding rule matters.",
            &["round", "round to", "decimal places", "floor", "ceil", "truncate", "banker's rounding"],
            round_number,
        ),
        SkillDef::new(
            "number_base",
            Category::Mathx,
            "Convert a non-negative integer between bases 2..=36 (e.g. 255 from base 10 to base 16 = ff). Use for binary/hex/octal/base conversions.",
            &["base conversion", "to binary", "to hex", "to octal", "convert base", "radix"],
            number_base,
        ),
    ]
}

// ----------------------------------------------------------------------------
// eval_expression — a safe recursive-descent arithmetic evaluator.
// ----------------------------------------------------------------------------

/// `eval_expression {expr}` -> the numeric result of a +-*/%^ expression with
/// parentheses and unary minus. No `eval`, no external engine: a tiny hand-rolled
/// tokenizer + Pratt-ish recursive-descent parser over f64. Pure + total; a
/// malformed expression or a divide-by-zero is a friendly `Err`.
fn eval_expression(args: &Value) -> Result<String> {
    let expr = args
        .get("expr")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("eval_expression needs an 'expr' string argument"))?;
    let tokens = tokenize(expr)?;
    let mut p = Parser { tokens: &tokens, pos: 0 };
    let v = p.parse_expr(0)?;
    if p.pos != p.tokens.len() {
        return Err(anyhow!("eval_expression: unexpected trailing input in '{expr}'"));
    }
    if !v.is_finite() {
        return Err(anyhow!("eval_expression: result is not finite (overflow or 0^negative?)"));
    }
    Ok(fmt_num(v))
}

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Num(f64),
    Plus,
    Minus,
    Star,
    Slash,
    Percent,
    Caret,
    LParen,
    RParen,
}

/// Tokenize an arithmetic string. Whitespace is ignored; numbers may have a
/// decimal point. Any other character is a friendly error (no silent skipping).
fn tokenize(s: &str) -> Result<Vec<Tok>> {
    let mut out = Vec::new();
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        match c {
            ' ' | '\t' | '\n' | '\r' => {
                i += 1;
            }
            '+' => {
                out.push(Tok::Plus);
                i += 1;
            }
            '-' => {
                out.push(Tok::Minus);
                i += 1;
            }
            '*' => {
                out.push(Tok::Star);
                i += 1;
            }
            '/' => {
                out.push(Tok::Slash);
                i += 1;
            }
            '%' => {
                out.push(Tok::Percent);
                i += 1;
            }
            '^' => {
                out.push(Tok::Caret);
                i += 1;
            }
            '(' => {
                out.push(Tok::LParen);
                i += 1;
            }
            ')' => {
                out.push(Tok::RParen);
                i += 1;
            }
            '0'..='9' | '.' => {
                let start = i;
                let mut seen_dot = false;
                while i < chars.len() && (chars[i].is_ascii_digit() || chars[i] == '.') {
                    if chars[i] == '.' {
                        if seen_dot {
                            return Err(anyhow!("eval_expression: malformed number with two dots"));
                        }
                        seen_dot = true;
                    }
                    i += 1;
                }
                let lit: String = chars[start..i].iter().collect();
                let n: f64 = lit
                    .parse()
                    .map_err(|_| anyhow!("eval_expression: bad number '{lit}'"))?;
                out.push(Tok::Num(n));
            }
            other => {
                return Err(anyhow!("eval_expression: unexpected character '{other}'"));
            }
        }
    }
    if out.is_empty() {
        return Err(anyhow!("eval_expression: empty expression"));
    }
    Ok(out)
}

struct Parser<'a> {
    tokens: &'a [Tok],
    pos: usize,
}

impl Parser<'_> {
    fn peek(&self) -> Option<&Tok> {
        self.tokens.get(self.pos)
    }

    /// Binding power for a binary operator. `^` is right-associative (handled in
    /// the recursion), `* / %` bind tighter than `+ -`.
    fn binding_power(tok: &Tok) -> Option<(u8, u8)> {
        match tok {
            Tok::Plus | Tok::Minus => Some((1, 2)),
            Tok::Star | Tok::Slash | Tok::Percent => Some((3, 4)),
            // Right-assoc: left bp > right bp so `2^3^2` = 2^(3^2).
            Tok::Caret => Some((6, 5)),
            _ => None,
        }
    }

    /// Pratt parser: parse expressions whose operators bind at least `min_bp`.
    fn parse_expr(&mut self, min_bp: u8) -> Result<f64> {
        let mut lhs = self.parse_atom()?;
        while let Some(tok) = self.peek() {
            let (l_bp, r_bp) = match Self::binding_power(tok) {
                Some(bp) => bp,
                None => break,
            };
            if l_bp < min_bp {
                break;
            }
            let op = tok.clone();
            self.pos += 1;
            let rhs = self.parse_expr(r_bp)?;
            lhs = apply_op(&op, lhs, rhs)?;
        }
        Ok(lhs)
    }

    /// Parse a number, a parenthesized sub-expression, or a unary +/-.
    fn parse_atom(&mut self) -> Result<f64> {
        match self.peek() {
            Some(Tok::Num(n)) => {
                let v = *n;
                self.pos += 1;
                Ok(v)
            }
            Some(Tok::Minus) => {
                self.pos += 1;
                // Unary minus binds tighter than * and /, looser than ^.
                Ok(-self.parse_expr(5)?)
            }
            Some(Tok::Plus) => {
                self.pos += 1;
                self.parse_expr(5)
            }
            Some(Tok::LParen) => {
                self.pos += 1;
                let v = self.parse_expr(0)?;
                match self.peek() {
                    Some(Tok::RParen) => {
                        self.pos += 1;
                        Ok(v)
                    }
                    _ => Err(anyhow!("eval_expression: missing closing parenthesis")),
                }
            }
            Some(other) => Err(anyhow!("eval_expression: unexpected token {other:?}")),
            None => Err(anyhow!("eval_expression: unexpected end of expression")),
        }
    }
}

/// Apply a binary operator, guarding divide/modulo by zero.
fn apply_op(op: &Tok, a: f64, b: f64) -> Result<f64> {
    Ok(match op {
        Tok::Plus => a + b,
        Tok::Minus => a - b,
        Tok::Star => a * b,
        Tok::Slash => {
            if b == 0.0 {
                return Err(anyhow!("eval_expression: division by zero"));
            }
            a / b
        }
        Tok::Percent => {
            if b == 0.0 {
                return Err(anyhow!("eval_expression: modulo by zero"));
            }
            a % b
        }
        Tok::Caret => a.powf(b),
        _ => return Err(anyhow!("eval_expression: not a binary operator")),
    })
}

/// Format an f64 result tersely: an integral value prints without a trailing
/// `.0`, otherwise a trimmed decimal. Deterministic.
fn fmt_num(v: f64) -> String {
    if v == v.trunc() && v.abs() < 1e15 {
        format!("{}", v as i64)
    } else {
        // Up to 10 significant decimals, trailing zeros trimmed.
        let s = format!("{v:.10}");
        let s = s.trim_end_matches('0').trim_end_matches('.');
        s.to_string()
    }
}

// ----------------------------------------------------------------------------
// percentage — percent-of, what-percent, percent-change, tip/discount/markup.
// ----------------------------------------------------------------------------

/// `percentage {op, ...}` -> a percentage computation. `op` selects:
///   - `of`        : `percent` of `value`            (15 of 80 -> 12)
///   - `what`      : `part` is what % of `whole`     (12 of 80 -> 15%)
///   - `change`    : % change from `from` to `to`
///   - `tip`       : add `percent` tip to `value`    (reports tip + total)
///   - `discount`  : subtract `percent` off `value`
///   - `markup`    : add `percent` markup to `value`
/// Pure + total; an unknown op or missing field is a friendly error.
fn percentage(args: &Value) -> Result<String> {
    let op = args
        .get("op")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("percentage needs an 'op' (of|what|change|tip|discount|markup)"))?;
    let num = |k: &str| -> Result<f64> {
        args.get(k)
            .and_then(Value::as_f64)
            .ok_or_else(|| anyhow!("percentage '{op}' needs a numeric '{k}'"))
    };
    match op {
        "of" => {
            let percent = num("percent")?;
            let value = num("value")?;
            Ok(format!("{} is {}% of {}", fmt_num(percent / 100.0 * value), fmt_num(percent), fmt_num(value)))
        }
        "what" => {
            let part = num("part")?;
            let whole = num("whole")?;
            if whole == 0.0 {
                return Err(anyhow!("percentage 'what' needs a non-zero 'whole'"));
            }
            Ok(format!("{} is {}% of {}", fmt_num(part), fmt_num(part / whole * 100.0), fmt_num(whole)))
        }
        "change" => {
            let from = num("from")?;
            let to = num("to")?;
            if from == 0.0 {
                return Err(anyhow!("percentage 'change' needs a non-zero 'from'"));
            }
            let pct = (to - from) / from * 100.0;
            let dir = if pct >= 0.0 { "increase" } else { "decrease" };
            Ok(format!("{}% {} (from {} to {})", fmt_num(pct.abs()), dir, fmt_num(from), fmt_num(to)))
        }
        "tip" => {
            let percent = num("percent")?;
            let value = num("value")?;
            let tip = percent / 100.0 * value;
            Ok(format!("tip {} + bill {} = {} ({}% tip)", fmt_num(tip), fmt_num(value), fmt_num(value + tip), fmt_num(percent)))
        }
        "discount" => {
            let percent = num("percent")?;
            let value = num("value")?;
            let off = percent / 100.0 * value;
            Ok(format!("{} after {}% off (save {})", fmt_num(value - off), fmt_num(percent), fmt_num(off)))
        }
        "markup" => {
            let percent = num("percent")?;
            let value = num("value")?;
            let add = percent / 100.0 * value;
            Ok(format!("{} after {}% markup (add {})", fmt_num(value + add), fmt_num(percent), fmt_num(add)))
        }
        other => Err(anyhow!("percentage: unknown op '{other}' (use of|what|change|tip|discount|markup)")),
    }
}

// ----------------------------------------------------------------------------
// stats_summary — count/sum/min/max/mean/median/mode/variance/stddev.
// ----------------------------------------------------------------------------

/// `stats_summary {values, sample?}` -> descriptive statistics over a numeric
/// array. `sample` (default false) selects sample variance/stddev (n-1 divisor)
/// vs. population (n). The mode is the most frequent value(s) by exact bit
/// pattern; ties report every modal value. Pure + total.
fn stats_summary(args: &Value) -> Result<String> {
    let arr = args
        .get("values")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("stats_summary needs a 'values' array of numbers"))?;
    if arr.is_empty() {
        return Err(anyhow!("stats_summary needs at least one value"));
    }
    let mut xs: Vec<f64> = Vec::with_capacity(arr.len());
    for v in arr {
        let n = v
            .as_f64()
            .ok_or_else(|| anyhow!("stats_summary: every value must be a number"))?;
        if !n.is_finite() {
            return Err(anyhow!("stats_summary: values must be finite"));
        }
        xs.push(n);
    }
    let sample = args.get("sample").and_then(Value::as_bool).unwrap_or(false);
    let n = xs.len();
    let sum: f64 = xs.iter().sum();
    let mean = sum / n as f64;

    let mut sorted = xs.clone();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let min = sorted[0];
    let max = sorted[n - 1];
    let median = if n % 2 == 1 {
        sorted[n / 2]
    } else {
        (sorted[n / 2 - 1] + sorted[n / 2]) / 2.0
    };

    // Mode: highest exact-value frequency. Group by canonical bit pattern so NaN
    // can't sneak in (already excluded) and -0.0/0.0 group together.
    let mut counts: std::collections::HashMap<u64, (f64, usize)> = std::collections::HashMap::new();
    for &x in &xs {
        let key = (x + 0.0).to_bits(); // +0.0 normalizes -0.0 to 0.0
        let e = counts.entry(key).or_insert((x, 0));
        e.1 += 1;
    }
    let max_freq = counts.values().map(|(_, c)| *c).max().unwrap();
    let mode_str = if max_freq <= 1 {
        "none (all unique)".to_string()
    } else {
        let mut modes: Vec<f64> = counts
            .values()
            .filter(|(_, c)| *c == max_freq)
            .map(|(v, _)| *v)
            .collect();
        modes.sort_by(|a, b| a.partial_cmp(b).unwrap());
        modes.iter().map(|m| fmt_num(*m)).collect::<Vec<_>>().join(", ")
    };

    // Variance: population (÷n) or sample (÷(n-1)).
    let ss: f64 = xs.iter().map(|x| (x - mean).powi(2)).sum();
    let (variance, var_label) = if sample {
        if n < 2 {
            return Err(anyhow!("stats_summary: sample variance needs at least 2 values"));
        }
        (ss / (n as f64 - 1.0), "sample")
    } else {
        (ss / n as f64, "population")
    };
    let stddev = variance.sqrt();

    Ok(format!(
        "n={n}, sum={}, min={}, max={}, mean={}, median={}, mode={}, variance={} ({var_label}), stddev={}",
        fmt_num(sum),
        fmt_num(min),
        fmt_num(max),
        fmt_num(mean),
        fmt_num(median),
        mode_str,
        fmt_num(variance),
        fmt_num(stddev),
    ))
}

// ----------------------------------------------------------------------------
// gcd_lcm — Euclid's algorithm over a list of integers.
// ----------------------------------------------------------------------------

/// `gcd_lcm {values}` -> the gcd and lcm of two-or-more integers. Uses Euclid's
/// algorithm on absolute values; lcm is computed via gcd with an overflow guard
/// (an lcm that exceeds i64 is a friendly error, never a wrapped wrong value).
fn gcd_lcm(args: &Value) -> Result<String> {
    let arr = args
        .get("values")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("gcd_lcm needs a 'values' array of integers"))?;
    if arr.len() < 2 {
        return Err(anyhow!("gcd_lcm needs at least two integers"));
    }
    let mut nums: Vec<i64> = Vec::with_capacity(arr.len());
    for v in arr {
        let n = v
            .as_i64()
            .ok_or_else(|| anyhow!("gcd_lcm: every value must be an integer"))?;
        nums.push(n);
    }
    let mut g = nums[0].unsigned_abs();
    for &x in &nums[1..] {
        g = gcd_u64(g, x.unsigned_abs());
    }

    // lcm over all, guarding overflow. lcm(a,b) = |a*b| / gcd(a,b) = (a/gcd)*b.
    let mut l: u64 = nums[0].unsigned_abs();
    for &x in &nums[1..] {
        let xa = x.unsigned_abs();
        if l == 0 || xa == 0 {
            l = 0; // lcm with 0 is 0 by convention here
            continue;
        }
        let g2 = gcd_u64(l, xa);
        l = (l / g2)
            .checked_mul(xa)
            .ok_or_else(|| anyhow!("gcd_lcm: lcm overflows 64-bit"))?;
    }
    Ok(format!("gcd = {g}, lcm = {l}"))
}

/// Euclid's gcd on unsigned integers. `gcd(0,0) = 0`. Pure + total.
fn gcd_u64(mut a: u64, mut b: u64) -> u64 {
    while b != 0 {
        let t = b;
        b = a % b;
        a = t;
    }
    a
}

// ----------------------------------------------------------------------------
// is_prime — trial division up to sqrt(n).
// ----------------------------------------------------------------------------

/// `is_prime {n}` -> whether `n` is prime. Trial division by 2, 3, then 6k±1 up
/// to sqrt(n). Negative, 0, and 1 are not prime. Pure + total.
fn is_prime(args: &Value) -> Result<String> {
    let n = args
        .get("n")
        .and_then(Value::as_i64)
        .ok_or_else(|| anyhow!("is_prime needs an integer 'n'"))?;
    if n < 0 {
        return Ok(format!("{n} is not prime (negative)"));
    }
    let prime = is_prime_u64(n as u64);
    Ok(format!("{n} is {}prime", if prime { "" } else { "not " }))
}

/// Deterministic primality by trial division. Pure + total.
fn is_prime_u64(n: u64) -> bool {
    if n < 2 {
        return false;
    }
    if n % 2 == 0 {
        return n == 2;
    }
    if n % 3 == 0 {
        return n == 3;
    }
    let mut i: u64 = 5;
    while i.saturating_mul(i) <= n {
        if n % i == 0 || n % (i + 2) == 0 {
            return false;
        }
        i += 6;
    }
    true
}

// ----------------------------------------------------------------------------
// factorize — prime power factorization.
// ----------------------------------------------------------------------------

/// `factorize {n}` -> prime factorization of a positive integer, formatted as
/// prime powers (e.g. `360 = 2^3 * 3^2 * 5`). Bounded to positive integers; 1 is
/// reported as having no prime factors. Pure + total.
fn factorize(args: &Value) -> Result<String> {
    let n = args
        .get("n")
        .and_then(Value::as_i64)
        .ok_or_else(|| anyhow!("factorize needs an integer 'n'"))?;
    if n < 1 {
        return Err(anyhow!("factorize needs a positive integer (got {n})"));
    }
    let mut m = n as u64;
    if m == 1 {
        return Ok("1 = 1 (no prime factors)".to_string());
    }
    let mut factors: Vec<(u64, u32)> = Vec::new();
    let mut d: u64 = 2;
    while d.saturating_mul(d) <= m {
        if m % d == 0 {
            let mut exp = 0;
            while m % d == 0 {
                m /= d;
                exp += 1;
            }
            factors.push((d, exp));
        }
        d += if d == 2 { 1 } else { 2 };
    }
    if m > 1 {
        factors.push((m, 1));
    }
    let parts: Vec<String> = factors
        .iter()
        .map(|(p, e)| if *e == 1 { p.to_string() } else { format!("{p}^{e}") })
        .collect();
    Ok(format!("{n} = {}", parts.join(" * ")))
}

// ----------------------------------------------------------------------------
// factorial — n! with overflow guard.
// ----------------------------------------------------------------------------

/// `factorial {n}` -> n! for a non-negative integer, computed with checked
/// multiplication so an overflow past u64 is a friendly error (20! is the last
/// that fits). Pure + total.
fn factorial(args: &Value) -> Result<String> {
    let n = args
        .get("n")
        .and_then(Value::as_i64)
        .ok_or_else(|| anyhow!("factorial needs an integer 'n'"))?;
    if n < 0 {
        return Err(anyhow!("factorial is undefined for negative n (got {n})"));
    }
    let r = factorial_u64(n as u64)
        .ok_or_else(|| anyhow!("factorial: {n}! overflows 64-bit (max is 20!)"))?;
    Ok(format!("{n}! = {r}"))
}

/// Checked factorial: `Some(n!)` or `None` on overflow. Pure + total.
fn factorial_u64(n: u64) -> Option<u64> {
    let mut acc: u64 = 1;
    for k in 2..=n {
        acc = acc.checked_mul(k)?;
    }
    Some(acc)
}

// ----------------------------------------------------------------------------
// combinations — nCr and nPr.
// ----------------------------------------------------------------------------

/// `combinations {n, r, kind?}` -> nCr (default) or nPr. `kind` = "combination"
/// or "permutation". Computed iteratively to avoid factorial overflow where the
/// answer itself fits; an answer that overflows u64 is a friendly error. Pure.
fn combinations(args: &Value) -> Result<String> {
    let n = args
        .get("n")
        .and_then(Value::as_i64)
        .ok_or_else(|| anyhow!("combinations needs an integer 'n'"))?;
    let r = args
        .get("r")
        .and_then(Value::as_i64)
        .ok_or_else(|| anyhow!("combinations needs an integer 'r'"))?;
    if n < 0 || r < 0 {
        return Err(anyhow!("combinations needs non-negative n and r"));
    }
    if r > n {
        return Err(anyhow!("combinations needs r <= n (got n={n}, r={r})"));
    }
    let kind = args.get("kind").and_then(Value::as_str).unwrap_or("combination");
    let (n, r) = (n as u64, r as u64);
    match kind {
        "combination" | "ncr" | "c" => {
            let v = ncr_u64(n, r).ok_or_else(|| anyhow!("combinations: nCr overflows 64-bit"))?;
            Ok(format!("C({n},{r}) = {v}"))
        }
        "permutation" | "npr" | "p" => {
            let v = npr_u64(n, r).ok_or_else(|| anyhow!("combinations: nPr overflows 64-bit"))?;
            Ok(format!("P({n},{r}) = {v}"))
        }
        other => Err(anyhow!("combinations: unknown kind '{other}' (use combination|permutation)")),
    }
}

/// nPr = n! / (n-r)! computed as a falling product, checked. Pure + total.
fn npr_u64(n: u64, r: u64) -> Option<u64> {
    let mut acc: u64 = 1;
    for k in 0..r {
        acc = acc.checked_mul(n - k)?;
    }
    Some(acc)
}

/// nCr via a multiply-then-divide loop that stays exact and minimizes overflow.
/// Uses the symmetry C(n,r)=C(n,n-r) to keep r small. Pure + total.
fn ncr_u64(n: u64, r: u64) -> Option<u64> {
    let r = r.min(n - r);
    let mut acc: u64 = 1;
    for k in 1..=r {
        // acc = acc * (n - r + k) / k, exact because acc already holds C(n,k-1)
        // scaled so the division is whole at each step.
        acc = acc.checked_mul(n - r + k)?;
        acc /= k;
    }
    Some(acc)
}

// ----------------------------------------------------------------------------
// quadratic — solve a*x^2 + b*x + c = 0.
// ----------------------------------------------------------------------------

/// `quadratic {a, b, c}` -> the roots of a*x^2 + b*x + c = 0 via the
/// discriminant. Reports two real roots, one repeated real root, or a complex
/// conjugate pair. `a == 0` falls back to the linear case. Pure + total.
fn quadratic(args: &Value) -> Result<String> {
    let num = |k: &str| -> Result<f64> {
        args.get(k)
            .and_then(Value::as_f64)
            .ok_or_else(|| anyhow!("quadratic needs a numeric '{k}'"))
    };
    let a = num("a")?;
    let b = num("b")?;
    let c = num("c")?;
    if a == 0.0 {
        // Linear: b*x + c = 0.
        if b == 0.0 {
            return if c == 0.0 {
                Ok("any x is a solution (0 = 0)".to_string())
            } else {
                Err(anyhow!("quadratic: no solution (a=b=0, c!=0)"))
            };
        }
        return Ok(format!("linear root: x = {}", fmt_num(-c / b)));
    }
    let disc = b * b - 4.0 * a * c;
    if disc > 0.0 {
        let sq = disc.sqrt();
        let x1 = (-b + sq) / (2.0 * a);
        let x2 = (-b - sq) / (2.0 * a);
        Ok(format!("two real roots: x = {}, {}", fmt_num(x1), fmt_num(x2)))
    } else if disc == 0.0 {
        let x = -b / (2.0 * a);
        Ok(format!("one real (double) root: x = {}", fmt_num(x)))
    } else {
        let re = -b / (2.0 * a);
        let im = (-disc).sqrt() / (2.0 * a);
        Ok(format!(
            "two complex roots: x = {} ± {}i",
            fmt_num(re),
            fmt_num(im.abs())
        ))
    }
}

// ----------------------------------------------------------------------------
// round_number — multiple rounding modes.
// ----------------------------------------------------------------------------

/// `round_number {value, places?, mode?}` -> `value` rounded to `places` decimals
/// (default 0) under `mode` (default half_up). Modes: half_up, half_even
/// (banker's), floor, ceil, trunc. Pure + total; `places` is bounded 0..=15.
fn round_number(args: &Value) -> Result<String> {
    let value = args
        .get("value")
        .and_then(Value::as_f64)
        .ok_or_else(|| anyhow!("round_number needs a numeric 'value'"))?;
    if !value.is_finite() {
        return Err(anyhow!("round_number needs a finite value"));
    }
    let places = args.get("places").and_then(Value::as_u64).unwrap_or(0);
    if places > 15 {
        return Err(anyhow!("round_number 'places' must be 0..=15"));
    }
    let mode = args.get("mode").and_then(Value::as_str).unwrap_or("half_up");
    let scale = 10f64.powi(places as i32);
    let scaled = value * scale;
    let rounded = match mode {
        "half_up" => {
            // Round half away from zero (the common "round 2.5 -> 3" rule).
            (scaled.abs() + 0.5).floor() * scaled.signum()
        }
        "half_even" => round_half_even(scaled),
        "floor" => scaled.floor(),
        "ceil" => scaled.ceil(),
        "trunc" => scaled.trunc(),
        other => {
            return Err(anyhow!(
                "round_number: unknown mode '{other}' (use half_up|half_even|floor|ceil|trunc)"
            ))
        }
    };
    Ok(fmt_num(rounded / scale))
}

/// Round-half-to-even (banker's rounding) on an already-scaled value. Pure.
fn round_half_even(x: f64) -> f64 {
    let floor = x.floor();
    let diff = x - floor;
    if diff < 0.5 {
        floor
    } else if diff > 0.5 {
        floor + 1.0
    } else {
        // Exactly .5 -> round to the even neighbor.
        if (floor as i64) % 2 == 0 {
            floor
        } else {
            floor + 1.0
        }
    }
}

// ----------------------------------------------------------------------------
// number_base — integer base conversion (2..=36).
// ----------------------------------------------------------------------------

/// `number_base {value, from?, to}` -> `value` (a string in base `from`, default
/// 10) re-expressed in base `to`. Both bases are 2..=36; digits are 0-9a-z,
/// case-insensitive on input, lowercase on output. Non-negative integers only.
fn number_base(args: &Value) -> Result<String> {
    let value = args
        .get("value")
        .ok_or_else(|| anyhow!("number_base needs a 'value' (string or integer)"))?;
    let from = args.get("from").and_then(Value::as_u64).unwrap_or(10);
    let to = args
        .get("to")
        .and_then(Value::as_u64)
        .ok_or_else(|| anyhow!("number_base needs a target base 'to'"))?;
    if !(2..=36).contains(&from) || !(2..=36).contains(&to) {
        return Err(anyhow!("number_base: both bases must be 2..=36"));
    }
    // Accept value as a JSON string OR a JSON integer (interpreted base-10 then).
    let s = if let Some(s) = value.as_str() {
        s.trim().to_string()
    } else if let Some(i) = value.as_u64() {
        i.to_string()
    } else {
        return Err(anyhow!("number_base 'value' must be a string or non-negative integer"));
    };
    if s.is_empty() {
        return Err(anyhow!("number_base: empty value"));
    }
    // Parse from `from` base into a u64, rejecting invalid digits + overflow.
    let mut acc: u64 = 0;
    for ch in s.chars() {
        let digit = ch
            .to_digit(from as u32)
            .ok_or_else(|| anyhow!("number_base: '{ch}' is not a valid base-{from} digit"))?;
        acc = acc
            .checked_mul(from)
            .and_then(|a| a.checked_add(digit as u64))
            .ok_or_else(|| anyhow!("number_base: value overflows 64-bit"))?;
    }
    let out = to_base(acc, to);
    Ok(format!("{s} (base {from}) = {out} (base {to})"))
}

/// Render `n` in `base` (2..=36) with lowercase digits. Pure + total.
fn to_base(mut n: u64, base: u64) -> String {
    if n == 0 {
        return "0".to_string();
    }
    const DIGITS: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut buf = Vec::new();
    while n > 0 {
        buf.push(DIGITS[(n % base) as usize]);
        n /= base;
    }
    buf.reverse();
    String::from_utf8(buf).unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ---- catalog -----------------------------------------------------------

    #[test]
    fn catalog_is_all_pure_and_well_named() {
        let s = skills();
        assert_eq!(s.len(), 11, "mathx ships 11 skills");
        for d in &s {
            assert_eq!(d.category, Category::Mathx);
            assert!(!d.consequential, "{} must be pure", d.name);
            assert!(!d.source_gated, "{} needs no external source", d.name);
            assert!(super::super::is_snake_case(d.name), "{} snake_case", d.name);
            assert!(!d.description.is_empty());
        }
        // Names are unique within the module.
        let mut names: Vec<&str> = s.iter().map(|d| d.name).collect();
        names.sort();
        let len = names.len();
        names.dedup();
        assert_eq!(names.len(), len, "no duplicate names within mathx");
    }

    // ---- eval_expression ---------------------------------------------------

    #[test]
    fn eval_respects_precedence_and_parens() {
        assert_eq!(eval_expression(&json!({"expr": "2 + 3 * 4"})).unwrap(), "14");
        assert_eq!(eval_expression(&json!({"expr": "(2 + 3) * 4"})).unwrap(), "20");
        assert_eq!(eval_expression(&json!({"expr": "2 + 3 * (4 - 1)"})).unwrap(), "11");
        assert_eq!(eval_expression(&json!({"expr": "10 / 4"})).unwrap(), "2.5");
        assert_eq!(eval_expression(&json!({"expr": "7 % 3"})).unwrap(), "1");
        assert_eq!(eval_expression(&json!({"expr": "-3 + 5"})).unwrap(), "2");
    }

    #[test]
    fn eval_power_is_right_associative() {
        // 2^3^2 = 2^(3^2) = 2^9 = 512, not (2^3)^2 = 64.
        assert_eq!(eval_expression(&json!({"expr": "2 ^ 3 ^ 2"})).unwrap(), "512");
        assert_eq!(eval_expression(&json!({"expr": "2 ^ 10"})).unwrap(), "1024");
    }

    #[test]
    fn eval_rejects_bad_input() {
        assert!(eval_expression(&json!({"expr": "1 / 0"})).is_err(), "divide by zero");
        assert!(eval_expression(&json!({"expr": "2 +"})).is_err(), "dangling op");
        assert!(eval_expression(&json!({"expr": "(1 + 2"})).is_err(), "unbalanced paren");
        assert!(eval_expression(&json!({"expr": "2 @ 3"})).is_err(), "bad char");
        assert!(eval_expression(&json!({"expr": ""})).is_err(), "empty");
        assert!(eval_expression(&json!({})).is_err(), "missing expr arg");
    }

    // ---- percentage --------------------------------------------------------

    #[test]
    fn percentage_core_operations() {
        assert_eq!(percentage(&json!({"op": "of", "percent": 15, "value": 80})).unwrap(), "12 is 15% of 80");
        assert_eq!(percentage(&json!({"op": "what", "part": 12, "whole": 80})).unwrap(), "12 is 15% of 80");
        assert_eq!(percentage(&json!({"op": "change", "from": 200, "to": 250})).unwrap(), "25% increase (from 200 to 250)");
        assert_eq!(percentage(&json!({"op": "change", "from": 200, "to": 150})).unwrap(), "25% decrease (from 200 to 150)");
        assert_eq!(percentage(&json!({"op": "tip", "percent": 20, "value": 50})).unwrap(), "tip 10 + bill 50 = 60 (20% tip)");
        assert_eq!(percentage(&json!({"op": "discount", "percent": 25, "value": 80})).unwrap(), "60 after 25% off (save 20)");
        assert_eq!(percentage(&json!({"op": "markup", "percent": 50, "value": 40})).unwrap(), "60 after 50% markup (add 20)");
    }

    #[test]
    fn percentage_rejects_bad_args() {
        assert!(percentage(&json!({"op": "what", "part": 1, "whole": 0})).is_err(), "div by zero whole");
        assert!(percentage(&json!({"op": "change", "from": 0, "to": 5})).is_err(), "div by zero from");
        assert!(percentage(&json!({"op": "nope"})).is_err(), "unknown op");
        assert!(percentage(&json!({})).is_err(), "missing op");
        assert!(percentage(&json!({"op": "of", "value": 80})).is_err(), "missing percent");
    }

    // ---- stats_summary -----------------------------------------------------

    #[test]
    fn stats_known_dataset() {
        // [2,4,4,4,5,5,7,9]: mean 5, median 4.5, mode 4, pop variance 4, stddev 2.
        let out = stats_summary(&json!({"values": [2, 4, 4, 4, 5, 5, 7, 9]})).unwrap();
        assert!(out.contains("n=8"), "{out}");
        assert!(out.contains("mean=5"), "{out}");
        assert!(out.contains("median=4.5"), "{out}");
        assert!(out.contains("mode=4"), "{out}");
        assert!(out.contains("variance=4 (population)"), "{out}");
        assert!(out.contains("stddev=2"), "{out}");
        assert!(out.contains("min=2"), "{out}");
        assert!(out.contains("max=9"), "{out}");
        assert!(out.contains("sum=40"), "{out}");
    }

    #[test]
    fn stats_sample_variance_and_unique_mode() {
        // [1,2,3,4,5]: sample variance = 2.5, all unique -> no mode.
        let out = stats_summary(&json!({"values": [1, 2, 3, 4, 5], "sample": true})).unwrap();
        assert!(out.contains("variance=2.5 (sample)"), "{out}");
        assert!(out.contains("mode=none (all unique)"), "{out}");
        assert!(out.contains("median=3"), "{out}");
    }

    #[test]
    fn stats_rejects_bad_input() {
        assert!(stats_summary(&json!({"values": []})).is_err(), "empty");
        assert!(stats_summary(&json!({"values": [1, "x"]})).is_err(), "non-number");
        assert!(stats_summary(&json!({"values": [1], "sample": true})).is_err(), "sample needs >=2");
        assert!(stats_summary(&json!({})).is_err(), "missing values");
    }

    // ---- gcd_lcm -----------------------------------------------------------

    #[test]
    fn gcd_lcm_known_values() {
        assert_eq!(gcd_lcm(&json!({"values": [12, 18]})).unwrap(), "gcd = 6, lcm = 36");
        assert_eq!(gcd_lcm(&json!({"values": [4, 6, 8]})).unwrap(), "gcd = 2, lcm = 24");
        assert_eq!(gcd_lcm(&json!({"values": [-12, 18]})).unwrap(), "gcd = 6, lcm = 36");
        assert_eq!(gcd_lcm(&json!({"values": [7, 13]})).unwrap(), "gcd = 1, lcm = 91");
    }

    #[test]
    fn gcd_u64_is_correct() {
        assert_eq!(gcd_u64(12, 18), 6);
        assert_eq!(gcd_u64(0, 5), 5);
        assert_eq!(gcd_u64(0, 0), 0);
        assert_eq!(gcd_u64(17, 5), 1);
    }

    #[test]
    fn gcd_lcm_rejects_bad_input() {
        assert!(gcd_lcm(&json!({"values": [12]})).is_err(), "needs two");
        assert!(gcd_lcm(&json!({"values": [12, "x"]})).is_err(), "non-int");
        assert!(gcd_lcm(&json!({})).is_err(), "missing values");
        // Overflow guard: two large coprime primes whose product (~2.5e19)
        // exceeds u64::MAX, so the lcm must be a friendly error, never wrapped.
        assert!(gcd_lcm(&json!({"values": [5000000029i64, 5000000039i64]})).is_err(), "lcm overflow");
    }

    // ---- is_prime ----------------------------------------------------------

    #[test]
    fn primality_known_cases() {
        for &p in &[2u64, 3, 5, 7, 11, 13, 97, 7919] {
            assert!(is_prime_u64(p), "{p} is prime");
        }
        for &c in &[0u64, 1, 4, 9, 15, 100, 7917] {
            assert!(!is_prime_u64(c), "{c} is not prime");
        }
        assert_eq!(is_prime(&json!({"n": 97})).unwrap(), "97 is prime");
        assert_eq!(is_prime(&json!({"n": 100})).unwrap(), "100 is not prime");
        assert_eq!(is_prime(&json!({"n": -7})).unwrap(), "-7 is not prime (negative)");
    }

    #[test]
    fn is_prime_rejects_non_int() {
        assert!(is_prime(&json!({"n": "x"})).is_err());
        assert!(is_prime(&json!({})).is_err());
    }

    // ---- factorize ---------------------------------------------------------

    #[test]
    fn factorize_known() {
        assert_eq!(factorize(&json!({"n": 360})).unwrap(), "360 = 2^3 * 3^2 * 5");
        assert_eq!(factorize(&json!({"n": 97})).unwrap(), "97 = 97");
        assert_eq!(factorize(&json!({"n": 1})).unwrap(), "1 = 1 (no prime factors)");
        assert_eq!(factorize(&json!({"n": 12})).unwrap(), "12 = 2^2 * 3");
    }

    #[test]
    fn factorize_rejects_non_positive() {
        assert!(factorize(&json!({"n": 0})).is_err());
        assert!(factorize(&json!({"n": -5})).is_err());
        assert!(factorize(&json!({})).is_err());
    }

    // ---- factorial ---------------------------------------------------------

    #[test]
    fn factorial_known_and_overflow() {
        assert_eq!(factorial(&json!({"n": 0})).unwrap(), "0! = 1");
        assert_eq!(factorial(&json!({"n": 5})).unwrap(), "5! = 120");
        assert_eq!(factorial(&json!({"n": 10})).unwrap(), "10! = 3628800");
        assert_eq!(factorial(&json!({"n": 20})).unwrap(), "20! = 2432902008176640000");
        // 21! overflows u64.
        assert!(factorial(&json!({"n": 21})).is_err(), "21! overflows");
        assert!(factorial(&json!({"n": -1})).is_err(), "negative undefined");
    }

    // ---- combinations ------------------------------------------------------

    #[test]
    fn combinations_known() {
        assert_eq!(combinations(&json!({"n": 5, "r": 2})).unwrap(), "C(5,2) = 10");
        assert_eq!(combinations(&json!({"n": 52, "r": 5})).unwrap(), "C(52,5) = 2598960");
        assert_eq!(combinations(&json!({"n": 5, "r": 2, "kind": "permutation"})).unwrap(), "P(5,2) = 20");
        assert_eq!(combinations(&json!({"n": 10, "r": 0})).unwrap(), "C(10,0) = 1");
        assert_eq!(combinations(&json!({"n": 49, "r": 6})).unwrap(), "C(49,6) = 13983816");
    }

    #[test]
    fn ncr_npr_helpers_are_exact() {
        assert_eq!(ncr_u64(5, 2), Some(10));
        assert_eq!(ncr_u64(52, 5), Some(2598960));
        assert_eq!(ncr_u64(10, 10), Some(1));
        assert_eq!(npr_u64(5, 2), Some(20));
        assert_eq!(npr_u64(5, 0), Some(1));
    }

    #[test]
    fn combinations_rejects_bad_args() {
        assert!(combinations(&json!({"n": 2, "r": 5})).is_err(), "r > n");
        assert!(combinations(&json!({"n": -1, "r": 0})).is_err(), "negative n");
        assert!(combinations(&json!({"n": 5, "r": 2, "kind": "nope"})).is_err(), "bad kind");
        assert!(combinations(&json!({"n": 5})).is_err(), "missing r");
    }

    // ---- quadratic ---------------------------------------------------------

    #[test]
    fn quadratic_real_double_complex() {
        // x^2 - 5x + 6 = 0 -> 3, 2.
        assert_eq!(quadratic(&json!({"a": 1, "b": -5, "c": 6})).unwrap(), "two real roots: x = 3, 2");
        // x^2 - 4x + 4 = 0 -> double root 2.
        assert_eq!(quadratic(&json!({"a": 1, "b": -4, "c": 4})).unwrap(), "one real (double) root: x = 2");
        // x^2 + 1 = 0 -> ±i.
        assert_eq!(quadratic(&json!({"a": 1, "b": 0, "c": 1})).unwrap(), "two complex roots: x = 0 ± 1i");
        // Linear fallback: 2x + 4 = 0 -> -2.
        assert_eq!(quadratic(&json!({"a": 0, "b": 2, "c": 4})).unwrap(), "linear root: x = -2");
    }

    #[test]
    fn quadratic_degenerate_cases() {
        assert_eq!(quadratic(&json!({"a": 0, "b": 0, "c": 0})).unwrap(), "any x is a solution (0 = 0)");
        assert!(quadratic(&json!({"a": 0, "b": 0, "c": 1})).is_err(), "no solution");
        assert!(quadratic(&json!({"a": 1, "b": 2})).is_err(), "missing c");
    }

    // ---- round_number ------------------------------------------------------

    #[allow(clippy::approx_constant)] // 3.14159 is rounding test input, not an approximation of PI
    #[test]
    fn rounding_modes() {
        assert_eq!(round_number(&json!({"value": 2.5})).unwrap(), "3"); // half_up
        assert_eq!(round_number(&json!({"value": 3.14159, "places": 2})).unwrap(), "3.14");
        assert_eq!(round_number(&json!({"value": 2.5, "mode": "half_even"})).unwrap(), "2"); // banker's: 2.5 -> 2
        assert_eq!(round_number(&json!({"value": 3.5, "mode": "half_even"})).unwrap(), "4"); // banker's: 3.5 -> 4
        assert_eq!(round_number(&json!({"value": 2.9, "mode": "floor"})).unwrap(), "2");
        assert_eq!(round_number(&json!({"value": 2.1, "mode": "ceil"})).unwrap(), "3");
        assert_eq!(round_number(&json!({"value": 2.99, "mode": "trunc"})).unwrap(), "2");
        assert_eq!(round_number(&json!({"value": -2.5})).unwrap(), "-3"); // half away from zero
    }

    #[test]
    fn rounding_rejects_bad_args() {
        assert!(round_number(&json!({"value": 1.0, "mode": "nope"})).is_err(), "bad mode");
        assert!(round_number(&json!({"value": 1.0, "places": 20})).is_err(), "places too big");
        assert!(round_number(&json!({})).is_err(), "missing value");
    }

    // ---- number_base -------------------------------------------------------

    #[test]
    fn base_conversion_known() {
        assert_eq!(number_base(&json!({"value": 255, "to": 16})).unwrap(), "255 (base 10) = ff (base 16)");
        assert_eq!(number_base(&json!({"value": "ff", "from": 16, "to": 10})).unwrap(), "ff (base 16) = 255 (base 10)");
        assert_eq!(number_base(&json!({"value": 10, "to": 2})).unwrap(), "10 (base 10) = 1010 (base 2)");
        assert_eq!(number_base(&json!({"value": "1010", "from": 2, "to": 10})).unwrap(), "1010 (base 2) = 10 (base 10)");
        assert_eq!(number_base(&json!({"value": 0, "to": 16})).unwrap(), "0 (base 10) = 0 (base 16)");
    }

    #[test]
    fn to_base_roundtrip() {
        assert_eq!(to_base(255, 16), "ff");
        assert_eq!(to_base(0, 2), "0");
        assert_eq!(to_base(35, 36), "z");
        assert_eq!(to_base(1010, 10), "1010");
    }

    #[test]
    fn base_conversion_rejects_bad_args() {
        assert!(number_base(&json!({"value": "g", "from": 16, "to": 10})).is_err(), "bad digit");
        assert!(number_base(&json!({"value": 5, "to": 37})).is_err(), "base too high");
        assert!(number_base(&json!({"value": 5, "from": 1, "to": 10})).is_err(), "base too low");
        assert!(number_base(&json!({"value": 5})).is_err(), "missing to");
    }

    // ---- determinism -------------------------------------------------------

    #[test]
    fn skills_are_deterministic() {
        // Re-running any skill on the same args gives identical output.
        let cases: Vec<(&str, Value)> = vec![
            ("eval_expression", json!({"expr": "2 ^ 8 + 1"})),
            ("stats_summary", json!({"values": [3, 1, 4, 1, 5, 9, 2, 6]})),
            ("factorize", json!({"n": 5040})),
            ("combinations", json!({"n": 20, "r": 7})),
            ("number_base", json!({"value": 123456, "to": 36})),
        ];
        let reg = skills();
        for (name, args) in cases {
            let def = reg.iter().find(|d| d.name == name).unwrap();
            let a = (def.run)(&args).unwrap();
            let b = (def.run)(&args).unwrap();
            assert_eq!(a, b, "{name} must be deterministic");
        }
    }
}
