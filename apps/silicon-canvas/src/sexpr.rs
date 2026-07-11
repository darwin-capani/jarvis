//! Generic KiCad S-expression lexer + value tree.
//!
//! KiCad 7/8 serializes `.kicad_sch` / `.kicad_pcb` / `.kicad_sym` / `.kicad_mod`
//! as Lisp-style S-expressions, e.g.
//!
//! ```text
//! (kicad_sch (version 20230121) (generator eeschema)
//!   (symbol (lib_id "Device:R") (at 100 100 0)
//!     (property "Reference" "R1" (at 100 95 0))))
//! ```
//!
//! This module provides ONLY the generic layer: a tolerant [`Lexer`] and a
//! [`Value`] tree (`Atom` / `Str` / `Number` / `List`) with a [`parse`] entry.
//! The KiCad-specific interpretation (which list head means a symbol, a wire, a
//! pad, etc.) lives in `parser.rs` — written by a different agent against this
//! tree. Keeping the two layers apart lets each be driven against arbitrary
//! S-expr input independently (SPEC §1) while this layer stays a small, total,
//! allocation-light tokenizer.
//!
//! Panic-freedom (SPEC §1) is exercised both by the cargo-fuzz target at
//! `fuzz/fuzz_targets/parse_document.rs` (`cargo +nightly fuzz run`, needs
//! nightly + libFuzzer) and, on stable, by the deterministic seeded-LCG
//! randomized tests in this module's `#[cfg(test)] fuzz_*` functions, which run
//! under plain `cargo test`.
//!
//! Token grammar (the subset KiCad emits):
//!   - `(` and `)` delimit lists.
//!   - Bare atoms: a run of non-whitespace, non-paren, non-quote characters.
//!     KiCad symbols (`kicad_sch`, `lib_id`, `yes`, `F.Cu`) and unquoted numbers
//!     are bare atoms.
//!   - Quoted strings: `"..."` with backslash escapes (`\"`, `\\`, `\n`, `\t`,
//!     `\r`). KiCad quotes any string that contains spaces/parens/quotes.
//!   - Whitespace (space, tab, CR, LF) separates tokens and is otherwise
//!     insignificant. KiCad has no comments in these files; a stray `;` is a
//!     normal atom character here (not a comment) so we never silently drop data.
//!
//! Numbers: a bare atom that parses as an `f64` is classified [`Value::Number`];
//! everything else bare is [`Value::Atom`]. This keeps the parser from
//! re-parsing coordinates while leaving identifiers as atoms. (KiCad never quotes
//! numbers, so a quoted "10" stays a [`Value::Str`] — correct, it's a label.)
//!
//! This module is the CONTRACT. `parser.rs` builds against [`Value`] /
//! [`parse`] verbatim; do NOT change these types.

use crate::error::SexprError;

/// A parsed S-expression node.
///
/// Distinguishes the three leaf forms KiCad uses so the parser can match on
/// shape without re-tokenizing:
///   - [`Value::Atom`] — a bare identifier / keyword (`symbol`, `lib_id`, `yes`).
///   - [`Value::Str`] — a quoted string (a property value, a net name).
///   - [`Value::Number`] — a bare atom that parsed as `f64` (a coordinate).
///   - [`Value::List`] — a parenthesized sequence of values.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    /// A bare, unquoted identifier or keyword.
    Atom(String),
    /// A double-quoted string (escapes already decoded).
    Str(String),
    /// A bare numeric atom, parsed as `f64`.
    Number(f64),
    /// A parenthesized list.
    List(Vec<Value>),
}

impl Value {
    /// The list head as an atom, if this is a non-empty list whose first element
    /// is an [`Value::Atom`] (e.g. `(symbol ...)` → `Some("symbol")`). This is
    /// how the parser dispatches on KiCad node type.
    pub fn head(&self) -> Option<&str> {
        match self {
            Value::List(items) => match items.first() {
                Some(Value::Atom(a)) => Some(a.as_str()),
                _ => None,
            },
            _ => None,
        }
    }

    /// The list elements, if this is a [`Value::List`].
    pub fn list(&self) -> Option<&[Value]> {
        match self {
            Value::List(items) => Some(items.as_slice()),
            _ => None,
        }
    }

    /// The string contents of an [`Value::Str`] OR [`Value::Atom`] (KiCad
    /// sometimes leaves short tokens unquoted), for fields the parser reads as
    /// text regardless of quoting.
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Str(s) | Value::Atom(s) => Some(s.as_str()),
            _ => None,
        }
    }

    /// The numeric value of a [`Value::Number`], or of an [`Value::Atom`] /
    /// [`Value::Str`] that parses as `f64` (defensive: KiCad usually leaves
    /// numbers bare, but a quoted coordinate should still read).
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Value::Number(n) => Some(*n),
            Value::Atom(s) | Value::Str(s) => s.parse::<f64>().ok(),
            _ => None,
        }
    }

    /// True for `(`-less leaves (atom/str/number).
    pub fn is_leaf(&self) -> bool {
        !matches!(self, Value::List(_))
    }

    /// Find the first direct child list with the given head, e.g.
    /// `node.get("at")` on a `(symbol (at 1 2 0) ...)` returns the `(at 1 2 0)`
    /// list. Convenience the parser leans on heavily.
    pub fn get(&self, head: &str) -> Option<&Value> {
        match self {
            Value::List(items) => items.iter().find(|v| v.head() == Some(head)),
            _ => None,
        }
    }

    /// Iterate every direct child list with the given head (KiCad repeats e.g.
    /// `property` and `pin` nodes).
    pub fn get_all<'a>(&'a self, head: &'a str) -> impl Iterator<Item = &'a Value> + 'a {
        let slice: &[Value] = self.list().unwrap_or(&[]);
        slice.iter().filter(move |v| v.head() == Some(head))
    }
}

/// One lexical token of the S-expression grammar.
#[derive(Debug, Clone, PartialEq)]
pub enum Token {
    /// `(`
    Open,
    /// `)`
    Close,
    /// A bare (unquoted) atom run.
    Atom(String),
    /// A quoted string with escapes already decoded.
    Str(String),
}

/// A streaming tokenizer over an S-expression source. Total over any input: it
/// either yields a [`Token`], reports an [`SexprError`] (currently only an
/// unterminated string / bad escape), or signals end of input. The parser drives
/// it; the fuzz target and the in-tree randomized tests drive it directly.
pub struct Lexer<'a> {
    src: &'a [u8],
    pos: usize,
}

impl<'a> Lexer<'a> {
    pub fn new(src: &'a str) -> Self {
        Lexer {
            src: src.as_bytes(),
            pos: 0,
        }
    }

    /// Current byte offset — used in error messages.
    pub fn offset(&self) -> usize {
        self.pos
    }

    #[inline]
    fn is_ws(b: u8) -> bool {
        matches!(b, b' ' | b'\t' | b'\r' | b'\n')
    }

    /// A byte that terminates a bare atom: whitespace, parens, or a quote.
    #[inline]
    fn is_atom_boundary(b: u8) -> bool {
        Self::is_ws(b) || b == b'(' || b == b')' || b == b'"'
    }

    fn skip_ws(&mut self) {
        while self.pos < self.src.len() && Self::is_ws(self.src[self.pos]) {
            self.pos += 1;
        }
    }

    /// Lex one quoted string starting AT the opening quote. Decodes `\"`, `\\`,
    /// `\n`, `\t`, `\r`; any other `\x` keeps the literal `x` (KiCad does not
    /// emit other escapes, and dropping the backslash is the least-surprising
    /// recovery). Errors only on an unterminated string.
    fn lex_string(&mut self) -> Result<Token, SexprError> {
        debug_assert_eq!(self.src[self.pos], b'"');
        let start = self.pos;
        self.pos += 1; // consume opening quote
        let mut out = String::new();
        while self.pos < self.src.len() {
            let b = self.src[self.pos];
            match b {
                b'"' => {
                    self.pos += 1; // consume closing quote
                    return Ok(Token::Str(out));
                }
                b'\\' => {
                    self.pos += 1;
                    if self.pos >= self.src.len() {
                        break; // trailing backslash → unterminated
                    }
                    let e = self.src[self.pos];
                    match e {
                        b'n' => out.push('\n'),
                        b't' => out.push('\t'),
                        b'r' => out.push('\r'),
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        // Unknown escape: keep the escaped byte literally.
                        other => out.push(other as char),
                    }
                    self.pos += 1;
                }
                _ => {
                    // Copy a run of normal bytes. We rebuild from bytes; KiCad
                    // strings are UTF-8 and a multi-byte char never contains a
                    // `"` or `\` byte, so byte-wise copy preserves UTF-8.
                    let run_start = self.pos;
                    while self.pos < self.src.len()
                        && self.src[self.pos] != b'"'
                        && self.src[self.pos] != b'\\'
                    {
                        self.pos += 1;
                    }
                    // `from_utf8_lossy` is safe and cheap; input is valid UTF-8.
                    out.push_str(&String::from_utf8_lossy(&self.src[run_start..self.pos]));
                }
            }
        }
        Err(SexprError::UnterminatedString { offset: start })
    }

    /// Lex one bare atom (assumes the current byte starts an atom).
    fn lex_atom(&mut self) -> Token {
        let start = self.pos;
        while self.pos < self.src.len() && !Self::is_atom_boundary(self.src[self.pos]) {
            self.pos += 1;
        }
        let text = String::from_utf8_lossy(&self.src[start..self.pos]).into_owned();
        Token::Atom(text)
    }

    /// Produce the next token, or `Ok(None)` at end of input.
    pub fn next_token(&mut self) -> Result<Option<Token>, SexprError> {
        self.skip_ws();
        if self.pos >= self.src.len() {
            return Ok(None);
        }
        let b = self.src[self.pos];
        match b {
            b'(' => {
                self.pos += 1;
                Ok(Some(Token::Open))
            }
            b')' => {
                self.pos += 1;
                Ok(Some(Token::Close))
            }
            b'"' => self.lex_string().map(Some),
            _ => Ok(Some(self.lex_atom())),
        }
    }
}

/// Parse an S-expression source into a single [`Value`].
///
/// KiCad files are exactly one top-level list `(kicad_sch ...)`. This entry:
///   - errors on empty input ([`SexprError::Empty`]),
///   - errors on a leaf-only document (a bare atom is not a KiCad document),
///   - errors on unbalanced parens or trailing tokens after the first complete
///     value.
///
/// For the more permissive "parse every top-level form" need (a `.kicad_sym`
/// library can hold multiple), use [`parse_many`].
pub fn parse(src: &str) -> Result<Value, SexprError> {
    let mut lexer = Lexer::new(src);
    let first = match lexer.next_token()? {
        Some(t) => t,
        None => return Err(SexprError::Empty),
    };
    let value = parse_value(&mut lexer, first)?;
    // After one complete top-level value there must be nothing but whitespace.
    if let Some(extra) = lexer.next_token()? {
        return Err(SexprError::TrailingTokens {
            offset: lexer.offset(),
            found: token_label(&extra),
        });
    }
    Ok(value)
}

/// Parse EVERY top-level form in the source (a `.kicad_sym` library can hold
/// several sibling `(symbol ...)` forms at the root). Errors on the same
/// structural faults as [`parse`] but does not require exactly one form.
pub fn parse_many(src: &str) -> Result<Vec<Value>, SexprError> {
    let mut lexer = Lexer::new(src);
    let mut out = Vec::new();
    while let Some(tok) = lexer.next_token()? {
        out.push(parse_value(&mut lexer, tok)?);
    }
    Ok(out)
}

/// Parse one value given its already-lexed leading token.
fn parse_value(lexer: &mut Lexer<'_>, lead: Token) -> Result<Value, SexprError> {
    match lead {
        Token::Open => parse_list(lexer),
        Token::Close => Err(SexprError::UnexpectedClose {
            offset: lexer.offset(),
        }),
        Token::Atom(a) => Ok(classify_atom(a)),
        Token::Str(s) => Ok(Value::Str(s)),
    }
}

/// Parse the body of a list after its `(` has been consumed, up to the matching
/// `)`. Bounded by a recursion-depth guard so a pathological deeply-nested input
/// cannot overflow the stack (defensive — the parser is fed arbitrary bytes by
/// the fuzz target and the in-tree randomized tests).
fn parse_list(lexer: &mut Lexer<'_>) -> Result<Value, SexprError> {
    parse_list_depth(lexer, 0)
}

/// Maximum list nesting depth. KiCad files are shallow (well under 100 levels);
/// this guard only fires on adversarial input (the fuzz target / randomized
/// panic-freedom tests deliberately exceed it).
const MAX_DEPTH: usize = 1024;

fn parse_list_depth(lexer: &mut Lexer<'_>, depth: usize) -> Result<Value, SexprError> {
    if depth >= MAX_DEPTH {
        return Err(SexprError::TooDeep {
            offset: lexer.offset(),
        });
    }
    let mut items = Vec::new();
    loop {
        match lexer.next_token()? {
            None => {
                return Err(SexprError::UnterminatedList {
                    offset: lexer.offset(),
                })
            }
            Some(Token::Close) => return Ok(Value::List(items)),
            Some(Token::Open) => items.push(parse_list_depth(lexer, depth + 1)?),
            Some(Token::Atom(a)) => items.push(classify_atom(a)),
            Some(Token::Str(s)) => items.push(Value::Str(s)),
        }
    }
}

/// Classify a bare atom as [`Value::Number`] when it parses as `f64`, else
/// [`Value::Atom`]. We reject empty (cannot happen from the lexer) and treat a
/// leading `+`/`-`/`.` numerically only via the `f64` parse (so `+` alone stays
/// an atom, `-12.5` becomes a number).
fn classify_atom(a: String) -> Value {
    // A pure integer/float? KiCad numbers never have unit suffixes here.
    // `f64::parse` SATURATES an over-magnitude literal (e.g. "1e400") to
    // ±Infinity rather than erroring, so require the parsed value to be finite —
    // otherwise the guard's contract (keep inf/nan OUT of Value::Number) is
    // silently defeated and the overflowing token falls through to Value::Atom.
    match a.parse::<f64>() {
        Ok(n) if n.is_finite() && is_numeric_token(&a) => Value::Number(n),
        _ => Value::Atom(a),
    }
}

/// Guard `f64::parse` against accepting `"inf"`, `"nan"`, etc. as numbers (those
/// are valid KiCad atoms / keywords, not coordinates). Only treat a token as a
/// number when it is made solely of digits, sign, decimal point, and an `e`/`E`
/// exponent — the exact shape KiCad emits for coordinates.
fn is_numeric_token(s: &str) -> bool {
    let mut seen_digit = false;
    for (i, c) in s.char_indices() {
        match c {
            '0'..='9' => seen_digit = true,
            '+' | '-' => { /* sign, allowed anywhere a parse accepts it */ }
            '.' => {}
            'e' | 'E' if i > 0 => {}
            _ => return false,
        }
    }
    seen_digit
}

/// A short human label for a token, for error messages.
fn token_label(t: &Token) -> String {
    match t {
        Token::Open => "(".to_string(),
        Token::Close => ")".to_string(),
        Token::Atom(a) => format!("atom {a:?}"),
        Token::Str(s) => format!("string {s:?}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_nested_kicad_like_form() {
        let src = r#"(kicad_sch (version 20230121) (generator "eeschema")
            (symbol (lib_id "Device:R") (at 100 100 0)
                (property "Reference" "R1")))"#;
        let v = parse(src).unwrap();
        assert_eq!(v.head(), Some("kicad_sch"));
        let sym = v.get("symbol").unwrap();
        assert_eq!(sym.head(), Some("symbol"));
        let at = sym.get("at").unwrap();
        let coords: Vec<f64> = at.list().unwrap()[1..]
            .iter()
            .filter_map(Value::as_f64)
            .collect();
        assert_eq!(coords, vec![100.0, 100.0, 0.0]);
        // The property value is a quoted string.
        let prop = sym.get("property").unwrap();
        assert_eq!(prop.list().unwrap()[2].as_str(), Some("R1"));
    }

    #[test]
    fn numbers_vs_atoms() {
        let v = parse("(at 1.5 -2 0 yes F.Cu)").unwrap();
        let items = v.list().unwrap();
        assert_eq!(items[1], Value::Number(1.5));
        assert_eq!(items[2], Value::Number(-2.0));
        assert_eq!(items[3], Value::Number(0.0));
        assert_eq!(items[4], Value::Atom("yes".to_string()));
        // A layer name with a dot is NOT a number.
        assert_eq!(items[5], Value::Atom("F.Cu".to_string()));
    }

    #[test]
    fn quoted_number_stays_string() {
        // A quoted "10" is a label, not a coordinate.
        let v = parse(r#"(property "Value" "10")"#).unwrap();
        assert_eq!(v.list().unwrap()[2], Value::Str("10".to_string()));
    }

    #[test]
    fn string_escapes_decode() {
        let v = parse(r#"(t "a\"b\\c\nd")"#).unwrap();
        assert_eq!(v.list().unwrap()[1], Value::Str("a\"b\\c\nd".to_string()));
    }

    #[test]
    fn unterminated_string_errors() {
        let err = parse(r#"(t "no end)"#).unwrap_err();
        assert!(matches!(err, SexprError::UnterminatedString { .. }));
    }

    #[test]
    fn unbalanced_parens_error() {
        assert!(matches!(parse("(a (b)").unwrap_err(), SexprError::UnterminatedList { .. }));
        assert!(matches!(parse("(a))").unwrap_err(), SexprError::TrailingTokens { .. }));
        assert!(matches!(parse(")").unwrap_err(), SexprError::UnexpectedClose { .. }));
    }

    #[test]
    fn empty_input_errors() {
        assert!(matches!(parse("   ").unwrap_err(), SexprError::Empty));
    }

    #[test]
    fn parse_many_handles_multiple_roots() {
        let forms = parse_many("(symbol A) (symbol B)").unwrap();
        assert_eq!(forms.len(), 2);
        assert_eq!(forms[0].head(), Some("symbol"));
        assert_eq!(forms[1].head(), Some("symbol"));
    }

    #[test]
    fn inf_and_nan_are_atoms_not_numbers() {
        let v = parse("(x inf nan NaN)").unwrap();
        let items = v.list().unwrap();
        assert_eq!(items[1], Value::Atom("inf".to_string()));
        assert_eq!(items[2], Value::Atom("nan".to_string()));
        assert_eq!(items[3], Value::Atom("NaN".to_string()));
        // An over-magnitude numeric literal parses to +Inf via f64::parse (which
        // saturates rather than erroring) — it must NOT become Value::Number(inf);
        // it falls through to an Atom, keeping inf/nan out of Value::Number.
        let over = parse("(net 1e400 \"GND\")").unwrap();
        let over_items = over.list().unwrap();
        assert_eq!(over_items[1], Value::Atom("1e400".to_string()));
    }

    #[test]
    fn deeply_nested_does_not_overflow() {
        // 2000 levels of nesting trips the depth guard rather than the stack.
        let src: String = "(".repeat(2000);
        assert!(matches!(parse(&src).unwrap_err(), SexprError::TooDeep { .. }));
    }

    #[test]
    fn get_all_iterates_repeats() {
        let v = parse("(sym (pin 1) (pin 2) (at 0 0))").unwrap();
        assert_eq!(v.get_all("pin").count(), 2);
    }

    // ---- in-tree fuzz: deterministic panic-freedom (SPEC §1/§7) -----------
    //
    // The cargo-fuzz target (`fuzz/fuzz_targets/parse_document.rs`) needs the
    // nightly toolchain + libFuzzer and may not run on a given box. These tests
    // give the SAME panic-freedom guarantee on stable, deterministically: a
    // seeded LCG (no system RNG, so failures reproduce from the printed seed)
    // generates many adversarial byte/char sequences and asserts that every
    // entry point returns a `Result` (`Ok` OR `Err`) and never panics. The test
    // harness turns any panic — including a stack overflow from unbounded
    // recursion or an integer overflow — into a failure.

    /// A tiny seeded linear-congruential generator (Numerical Recipes constants).
    /// Deterministic and self-contained; NOT a system RNG, so a failing case is
    /// reproducible purely from its seed.
    struct Lcg(u64);
    impl Lcg {
        fn new(seed: u64) -> Self {
            // Avoid the degenerate all-zero state.
            Lcg(seed ^ 0x9E37_79B9_7F4A_7C15)
        }
        fn next_u32(&mut self) -> u32 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (self.0 >> 32) as u32
        }
        fn below(&mut self, n: u32) -> u32 {
            if n == 0 {
                0
            } else {
                self.next_u32() % n
            }
        }
    }

    /// Build one adversarial source string from the generator. Deliberately mixes
    /// the shapes a fuzzer trips over: deep/unbalanced parens, gigantic atoms,
    /// unterminated and escape-laden quoted strings, control chars, and assorted
    /// non-ASCII / near-invalid-UTF-8 scalars (the public API is `&str`, so we
    /// stress odd-but-valid scalars and char boundaries rather than raw bad bytes
    /// — raw invalid UTF-8 is the cargo-fuzz target's job via `from_utf8_lossy`).
    fn adversarial_source(rng: &mut Lcg, max_len: usize) -> String {
        // Palette of bytes/chars worth throwing at the lexer.
        const SOUP: &[char] = &[
            '(', ')', '"', '\\', ' ', '\t', '\n', '\r', ';', '.', '-', '+', 'e', 'E', '0', '9',
            'a', 'Z', '_', ':', 'i', 'n', 'f', '/', '\0', '\u{7f}', '\u{80}', 'é', '中', '𝄞',
            '\u{FEFF}', '\u{FFFD}',
        ];
        let mut s = String::new();
        let len = rng.below(max_len as u32) as usize;
        while s.len() < len {
            match rng.below(16) {
                // A run of deeply nested opens (unbalanced on purpose sometimes).
                0 => {
                    let depth = rng.below(2200); // can exceed MAX_DEPTH (1024)
                    for _ in 0..depth {
                        s.push('(');
                    }
                }
                // A run of closes (may underflow the open count).
                1 => {
                    for _ in 0..rng.below(64) {
                        s.push(')');
                    }
                }
                // A huge bare atom.
                2 => {
                    for _ in 0..rng.below(4096) {
                        s.push('x');
                    }
                }
                // A quoted string that may never terminate, full of escapes.
                3 => {
                    s.push('"');
                    for _ in 0..rng.below(128) {
                        s.push(SOUP[rng.below(SOUP.len() as u32) as usize]);
                    }
                    // Half the time leave it unterminated.
                    if rng.below(2) == 0 {
                        s.push('"');
                    }
                }
                // A trailing-backslash / bad-escape tail.
                4 => {
                    s.push('"');
                    s.push('\\');
                }
                // A number-shaped token (stresses classify_atom / f64 parse).
                5 => {
                    for _ in 0..rng.below(40) {
                        s.push(SOUP[rng.below(SOUP.len() as u32) as usize]);
                    }
                }
                // Otherwise: a single char from the soup.
                _ => s.push(SOUP[rng.below(SOUP.len() as u32) as usize]),
            }
        }
        s
    }

    #[test]
    fn fuzz_sexpr_layer_never_panics() {
        // Iterations kept high enough to cover the shape space, low enough to stay
        // a sub-second unit test. Each seed is printed implicitly via the case
        // index on failure (the input itself is reconstructable from the seed).
        for seed in 0u64..4000 {
            let mut rng = Lcg::new(seed);
            let src = adversarial_source(&mut rng, 1024);

            // Every entry point must return a Result, never panic. We assert on
            // the variant only loosely: the contract is "no panic", and both Ok
            // and Err satisfy it.
            let _ = parse(&src);
            let _ = parse_many(&src);

            // Drive the lexer to exhaustion directly — it must be total on its own.
            let mut lexer = Lexer::new(&src);
            loop {
                match lexer.next_token() {
                    Ok(Some(_)) => {}
                    Ok(None) | Err(_) => break,
                }
            }
        }
    }

    #[test]
    fn fuzz_sexpr_hand_picked_adversarial_inputs_never_panic() {
        // Explicit corner cases that a random walk reaches rarely but that have
        // historically broken hand-rolled lexers. Each must yield a Result.
        let cases: &[&str] = &[
            "",
            "(",
            ")",
            "()",
            "(((((((((((((((((((((",
            ")))))))))))))))))))))",
            "\"",
            "\"\\",
            "\"unterminated",
            "\"a\\nb\\t\\\"c\"",
            "(a . b)",
            "(1e999999 -0.0 +.5 .e 1e 1.2.3)",
            "(\u{0}\u{1}\u{2}\u{7f})",
            "(é 中 𝄞 \u{FEFF})",
            ";not-a-comment (still atoms)",
            "(nan inf NaN Infinity -inf)",
            "(\"\")",
        ];
        for c in cases {
            let _ = parse(c);
            let _ = parse_many(c);
            let mut lexer = Lexer::new(c);
            while let Ok(Some(_)) = lexer.next_token() {}
        }
        // A pathologically deep but balanced form trips the depth guard, never
        // the stack.
        let deep = format!("{}{}", "(".repeat(5000), ")".repeat(5000));
        assert!(matches!(parse(&deep).unwrap_err(), SexprError::TooDeep { .. }));
    }
}
