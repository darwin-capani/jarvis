//! Category: UTILITIES — general-purpose glue (encoders, hashers, counters,
//! id/slug helpers, formatters). This module also carries the THREE proof skills
//! that exercise the whole framework end-to-end (registry -> `skill_list` ->
//! `skill_invoke` -> pure run): `base64_encode`, `word_count`, `dice_roll`. They
//! prove the path is real before the Library phase fills the other category files.
//!
//! Every skill here is PURE + DETERMINISTIC: anything that conceptually needs
//! randomness (`dice_roll`, `uuid_v4`, `password_gen`) takes a REQUIRED `seed` so
//! it has no ambient entropy, and the rest are total functions of their args. No
//! network, no clock, no I/O — hermetically testable. The hashers compute REAL
//! algorithms (SHA-256 via the RustCrypto `sha2` crate already in the tree, a
//! hand-rolled IEEE CRC-32), never approximations that lie.

use anyhow::{anyhow, Result};
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::{Category, SkillDef};

/// The utilities catalog. The Library phase appends more `SkillDef::new(...)`
/// entries to THIS vec (and nothing in mod.rs changes).
pub fn skills() -> Vec<SkillDef> {
    vec![
        SkillDef::new(
            "base64_encode",
            Category::Utilities,
            "Encode UTF-8 text to standard Base64. Use when the user wants text base64-encoded, or to wrap bytes for a token/data-URI.",
            &["base64", "base 64", "encode base64", "b64 encode"],
            base64_encode,
        ),
        SkillDef::new(
            "base64_decode",
            Category::Utilities,
            "Decode standard Base64 back to UTF-8 text. Use when the user has a base64 string and wants the original text.",
            &["decode base64", "base64 decode", "un-base64", "from base64"],
            base64_decode,
        ),
        SkillDef::new(
            "hex_encode",
            Category::Utilities,
            "Encode UTF-8 text to lowercase hexadecimal (two hex digits per byte). Use to show the raw bytes of a string as hex.",
            &["to hex", "hex encode", "hexadecimal", "bytes as hex"],
            hex_encode,
        ),
        SkillDef::new(
            "hex_decode",
            Category::Utilities,
            "Decode a hexadecimal string back to UTF-8 text. Use when the user has hex bytes and wants the original text.",
            &["from hex", "hex decode", "decode hex", "un-hex"],
            hex_decode,
        ),
        SkillDef::new(
            "url_encode",
            Category::Utilities,
            "Percent-encode text for safe use in a URL component (RFC 3986 unreserved set kept literal). Use for query params or path segments.",
            &["url encode", "percent encode", "escape for url", "urlencode"],
            url_encode,
        ),
        SkillDef::new(
            "url_decode",
            Category::Utilities,
            "Decode a percent-encoded URL component back to text (`+` is treated as a literal plus, not a space). Use to read a urlencoded value.",
            &["url decode", "percent decode", "unescape url", "urldecode"],
            url_decode,
        ),
        SkillDef::new(
            "sha256_hex",
            Category::Utilities,
            "Compute the SHA-256 hash of UTF-8 text as a 64-char lowercase hex digest. Use for a content fingerprint or integrity check.",
            &["sha256", "sha-256", "hash this", "checksum sha256", "digest"],
            sha256_hex,
        ),
        SkillDef::new(
            "crc32",
            Category::Utilities,
            "Compute the IEEE CRC-32 checksum of UTF-8 text as 8 hex digits. Use for a fast non-cryptographic integrity/dedup check (zip/png style).",
            &["crc32", "crc-32", "checksum", "crc"],
            crc32,
        ),
        SkillDef::new(
            "slugify",
            Category::Utilities,
            "Turn arbitrary text into a clean URL/file slug: lowercase ASCII words joined by single hyphens. Use for a permalink or filename stem.",
            &["slugify", "make a slug", "url slug", "permalink", "filename from title"],
            slugify,
        ),
        SkillDef::new(
            "case_convert",
            Category::Utilities,
            "Convert an identifier/phrase between cases: snake, kebab, camel, pascal, title, upper, lower. Pass 'text' and 'case'. Use to rename or reformat identifiers.",
            &["snake case", "camel case", "kebab case", "pascal case", "title case", "convert case"],
            case_convert,
        ),
        SkillDef::new(
            "byte_size_humanize",
            Category::Utilities,
            "Format a raw byte count as a human-readable size (1536 -> 1.5 KiB). Pass 'bytes' and optional binary=false for SI (KB) units. Use to display file sizes.",
            &["humanize bytes", "file size", "byte size", "human readable size", "MB GB KB"],
            byte_size_humanize,
        ),
        SkillDef::new(
            "uuid_v4",
            Category::Utilities,
            "Generate an RFC 4122 version-4 UUID deterministically from a REQUIRED integer 'seed' (so it is reproducible — no ambient entropy). Use for a stable test/fixture id.",
            &["uuid", "guid", "generate uuid", "uuid v4", "random id"],
            uuid_v4,
        ),
        SkillDef::new(
            "password_gen",
            Category::Utilities,
            "Generate a strong password deterministically from a REQUIRED integer 'seed' plus optional 'length' (default 16). Guarantees lower/upper/digit/symbol. Reproducible — seed it; not for ambient secrets.",
            &["generate password", "strong password", "make a password", "password generator"],
            password_gen,
        ),
        SkillDef::new(
            "word_count",
            Category::Utilities,
            "Count words, characters, and lines in a block of text. Use when the user asks how many words/characters/lines something has.",
            &["word count", "how many words", "count words", "character count", "line count"],
            word_count,
        ),
        SkillDef::new(
            "dice_roll",
            Category::Utilities,
            "Roll dice deterministically from a seed (e.g. 2d6, 1d20). Use for a tabletop/game roll; REQUIRES a seed so the result is reproducible — there is no ambient randomness.",
            &["roll dice", "roll a d20", "2d6", "dice roll", "throw the dice"],
            dice_roll,
        ),
        SkillDef::new(
            "rot13",
            Category::Utilities,
            "Apply the ROT13 letter substitution (a<->n, etc.) to text; non-letters pass through. Self-inverse. Use to lightly obfuscate or reveal a spoiler.",
            &["rot13", "rot-13", "obfuscate text", "spoiler cipher"],
            rot13,
        )
        .with_eval_vectors(&[("{\"text\":\"Hello\"}", "Uryyb"), ("{\"text\":\"Uryyb\"}", "Hello")]),
        SkillDef::new(
            "caesar_cipher",
            Category::Utilities,
            "Shift the letters of 'text' by integer 'shift' (mod 26, wrapping); non-letters pass through. Use for a classic Caesar cipher — a negative shift decodes.",
            &["caesar cipher", "shift cipher", "rotate letters", "encrypt with a shift"],
            caesar_cipher,
        )
        .with_eval_vectors(&[("{\"text\":\"abc\",\"shift\":1}", "bcd"), ("{\"text\":\"xyz\",\"shift\":3}", "abc")]),
        SkillDef::new(
            "luhn_check",
            Category::Utilities,
            "Validate a number string against the Luhn checksum (credit cards, IMEIs); spaces/dashes ignored. Returns 'valid' or 'invalid'. Use to sanity-check a card/ID check digit.",
            &["luhn", "luhn check", "validate credit card", "card checksum"],
            luhn_check,
        )
        .with_eval_vectors(&[("{\"number\":\"79927398713\"}", "valid"), ("{\"number\":\"79927398710\"}", "invalid")]),
        SkillDef::new(
            "rgb_to_hex",
            Category::Utilities,
            "Convert an RGB color (integers 'r','g','b', each 0..=255) to a #rrggbb hex string. Use to turn channel values into a CSS hex color.",
            &["rgb to hex", "color to hex", "hex color", "rgb hex"],
            rgb_to_hex,
        )
        .with_eval_vectors(&[("{\"r\":255,\"g\":0,\"b\":128}", "#ff0080"), ("{\"r\":0,\"g\":0,\"b\":0}", "#000000")]),
        SkillDef::new(
            "hex_to_rgb",
            Category::Utilities,
            "Convert a #rrggbb hex color (the leading # is optional) to an rgb(r, g, b) string. Use to read a hex color as channel values.",
            &["hex to rgb", "hex color to rgb", "parse hex color", "rgb from hex"],
            hex_to_rgb,
        )
        .with_eval_vectors(&[("{\"hex\":\"#ff0080\"}", "rgb(255, 0, 128)"), ("{\"hex\":\"00ff00\"}", "rgb(0, 255, 0)")]),
        SkillDef::new(
            "binary_encode",
            Category::Utilities,
            "Encode 'text' as space-separated 8-bit binary, one octet per UTF-8 byte. Use to show the raw bits of a string.",
            &["text to binary", "binary encode", "to bits", "ascii to binary"],
            binary_encode,
        )
        .with_eval_vectors(&[("{\"text\":\"A\"}", "01000001"), ("{\"text\":\"Hi\"}", "01001000 01101001")]),
        SkillDef::new(
            "sql_guard",
            Category::Utilities,
            "Classify whether a SQL query is strictly READ-ONLY (a single SELECT/WITH/EXPLAIN with no write/DDL keywords, no extra statements, no comments). Returns 'read-only: OK' or 'REJECTED: <reason>'. The fail-closed safety core for any read-only query tool — conservative by design (it rejects a query that merely MENTIONS a write keyword).",
            &["is this sql read only", "sql guard", "validate sql", "read only query check"],
            sql_guard,
        )
        .with_eval_vectors(&[
            ("{\"sql\":\"SELECT * FROM traces\"}", "read-only: OK"),
            ("{\"sql\":\"UPDATE traces SET x=1\"}", "REJECTED: must start with SELECT, WITH, or EXPLAIN (got 'UPDATE')"),
            ("{\"sql\":\"SELECT 1; DROP TABLE t\"}", "REJECTED: multiple statements are not allowed"),
        ]),
        SkillDef::new(
            "levenshtein",
            Category::Utilities,
            "Compute the Levenshtein edit distance (minimum single-character insertions, deletions, or substitutions) between strings 'a' and 'b'. Use for fuzzy matching, typo distance, or 'how different are these two strings'.",
            &["levenshtein", "edit distance", "string distance", "how similar are two strings"],
            levenshtein,
        )
        .with_eval_vectors(&[
            ("{\"a\":\"kitten\",\"b\":\"sitting\"}", "3"),
            ("{\"a\":\"\",\"b\":\"abc\"}", "3"),
            ("{\"a\":\"same\",\"b\":\"same\"}", "0"),
        ]),
        SkillDef::new(
            "hamming_distance",
            Category::Utilities,
            "Count the positions at which equal-length strings 'a' and 'b' differ (the Hamming distance). Errors if the lengths differ. Use to compare fixed-width codes, bit strings, or DNA sequences.",
            &["hamming distance", "count differing positions", "bit difference"],
            hamming_distance,
        )
        .with_eval_vectors(&[
            ("{\"a\":\"karolin\",\"b\":\"kathrin\"}", "3"),
            ("{\"a\":\"1011101\",\"b\":\"1001001\"}", "2"),
        ]),
        SkillDef::new(
            "jaccard_similarity",
            Category::Utilities,
            "Word-set Jaccard similarity of texts 'a' and 'b' (|intersection| / |union| of their whitespace-split word sets), as a 0.00-1.00 ratio. Use to gauge how much two short texts overlap.",
            &["jaccard similarity", "text overlap", "word set similarity", "how much do two texts overlap"],
            jaccard_similarity,
        )
        .with_eval_vectors(&[
            ("{\"a\":\"a b c\",\"b\":\"b c d\"}", "0.50"),
            ("{\"a\":\"one two\",\"b\":\"one two\"}", "1.00"),
        ]),
        SkillDef::new(
            "base32_encode",
            Category::Utilities,
            "Encode 'text' (UTF-8) to standard RFC 4648 Base32 (A-Z2-7, '=' padding). Use when a system expects Base32 (e.g. TOTP secrets, some DNS/URL contexts) rather than Base64.",
            &["base32", "base32 encode", "rfc 4648 base32", "encode base32"],
            base32_encode,
        )
        .with_eval_vectors(&[
            ("{\"text\":\"foobar\"}", "MZXW6YTBOI======"),
            ("{\"text\":\"f\"}", "MY======"),
        ]),
    ]
}

// ---------------------------------------------------------------------------
// Base64 (RFC 4648, standard alphabet with `=` padding)
// ---------------------------------------------------------------------------

/// Standard Base64 alphabet (RFC 4648, `+/` with `=` padding). Hand-rolled so the
/// skill carries no new crate dependency and is trivially verifiable.
const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// `base64_encode {text}` -> standard Base64 of the UTF-8 bytes. Pure + total.
fn base64_encode(args: &Value) -> Result<String> {
    let text = require_str(args, "text", "base64_encode")?;
    Ok(encode_b64(text.as_bytes()))
}

/// Pure standard-Base64 encoder over arbitrary bytes. Three input bytes map to
/// four output chars; the final 1- or 2-byte group is `=`-padded.
fn encode_b64(input: &[u8]) -> String {
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(B64[((n >> 18) & 63) as usize] as char);
        out.push(B64[((n >> 12) & 63) as usize] as char);
        out.push(if chunk.len() > 1 {
            B64[((n >> 6) & 63) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            B64[(n & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}

/// `base64_decode {text}` -> the decoded UTF-8 string. Rejects non-alphabet
/// characters, bad length, and bytes that are not valid UTF-8 (a friendly error,
/// never a panic or a garbage fabrication).
fn base64_decode(args: &Value) -> Result<String> {
    let text = require_str(args, "text", "base64_decode")?;
    let bytes = decode_b64(text)?;
    String::from_utf8(bytes)
        .map_err(|_| anyhow!("base64_decode: the decoded bytes are not valid UTF-8 text"))
}

/// Pure standard-Base64 decoder. Ignores nothing silently: any character outside
/// the alphabet (besides trailing `=` padding) is an error, and the string length
/// must be a multiple of 4.
fn decode_b64(s: &str) -> Result<Vec<u8>> {
    let s = s.trim();
    if s.is_empty() {
        return Ok(Vec::new());
    }
    if !s.is_ascii() || !s.len().is_multiple_of(4) {
        return Err(anyhow!(
            "base64_decode: input length must be a multiple of 4 and ASCII"
        ));
    }
    // Map a base64 char to its 6-bit value; `=` is padding (handled below).
    fn val(c: u8) -> Option<u32> {
        match c {
            b'A'..=b'Z' => Some((c - b'A') as u32),
            b'a'..=b'z' => Some((c - b'a' + 26) as u32),
            b'0'..=b'9' => Some((c - b'0' + 52) as u32),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(s.len() / 4 * 3);
    for (i, chunk) in bytes.chunks(4).enumerate() {
        let is_last = i == bytes.len() / 4 - 1;
        // Count padding `=` — only allowed in the final quartet, and only as the
        // trailing one or two characters (`xy==` / `xyz=`), never leading/interior.
        let pad = chunk.iter().filter(|&&c| c == b'=').count();
        let trailing_pad = match pad {
            0 => true,
            1 => chunk[3] == b'=',
            2 => chunk[2] == b'=' && chunk[3] == b'=',
            _ => false,
        };
        if (pad > 0 && !is_last) || pad > 2 || !trailing_pad {
            return Err(anyhow!("base64_decode: misplaced '=' padding"));
        }
        let mut n = 0u32;
        for &c in chunk {
            let v = if c == b'=' {
                0
            } else {
                val(c).ok_or_else(|| anyhow!("base64_decode: invalid character '{}'", c as char))?
            };
            n = (n << 6) | v;
        }
        out.push((n >> 16) as u8);
        if pad < 2 {
            out.push((n >> 8) as u8);
        }
        if pad < 1 {
            out.push(n as u8);
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Hex
// ---------------------------------------------------------------------------

/// `hex_encode {text}` -> lowercase hex of the UTF-8 bytes. Pure.
fn hex_encode(args: &Value) -> Result<String> {
    let text = require_str(args, "text", "hex_encode")?;
    Ok(hex::encode(text.as_bytes()))
}

/// `hex_decode {text}` -> the decoded UTF-8 string. Rejects odd length, non-hex
/// digits, and non-UTF-8 results with a friendly error.
fn hex_decode(args: &Value) -> Result<String> {
    let text = require_str(args, "text", "hex_decode")?.trim();
    let bytes = hex::decode(text)
        .map_err(|e| anyhow!("hex_decode: not valid hexadecimal ({e})"))?;
    String::from_utf8(bytes)
        .map_err(|_| anyhow!("hex_decode: the decoded bytes are not valid UTF-8 text"))
}

// ---------------------------------------------------------------------------
// URL percent-encoding (RFC 3986)
// ---------------------------------------------------------------------------

/// `url_encode {text}` -> percent-encoded form. Keeps the RFC 3986 unreserved set
/// (`A-Z a-z 0-9 - _ . ~`) literal; every other byte becomes `%XX`. Pure.
fn url_encode(args: &Value) -> Result<String> {
    let text = require_str(args, "text", "url_encode")?;
    let mut out = String::with_capacity(text.len() * 3);
    for &b in text.as_bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push('%');
            out.push(hex_upper(b >> 4));
            out.push(hex_upper(b & 0x0f));
        }
    }
    Ok(out)
}

/// `url_decode {text}` -> the decoded text. `%XX` triples become their byte; `+`
/// stays a literal `+` (this decodes a URI *component*, not a form field). Errors
/// on a truncated/invalid `%` escape or non-UTF-8 result. Pure.
fn url_decode(args: &Value) -> Result<String> {
    let text = require_str(args, "text", "url_decode")?;
    let bytes = text.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            if i + 2 >= bytes.len() {
                return Err(anyhow!("url_decode: truncated '%' escape"));
            }
            let hi = from_hex_digit(bytes[i + 1])
                .ok_or_else(|| anyhow!("url_decode: invalid hex after '%'"))?;
            let lo = from_hex_digit(bytes[i + 2])
                .ok_or_else(|| anyhow!("url_decode: invalid hex after '%'"))?;
            out.push((hi << 4) | lo);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out)
        .map_err(|_| anyhow!("url_decode: the decoded bytes are not valid UTF-8 text"))
}

/// Map a nibble (0..=15) to an uppercase hex digit char.
fn hex_upper(nibble: u8) -> char {
    match nibble {
        0..=9 => (b'0' + nibble) as char,
        10..=15 => (b'A' + nibble - 10) as char,
        _ => unreachable!("nibble is masked to 0..=15"),
    }
}

/// Parse a single ASCII hex digit (either case) to its value, or `None`.
fn from_hex_digit(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Hashes / checksums
// ---------------------------------------------------------------------------

/// `sha256_hex {text}` -> the lowercase 64-char SHA-256 hex digest of the UTF-8
/// bytes. REAL SHA-256 via the in-tree `sha2` crate (not an approximation). Pure.
fn sha256_hex(args: &Value) -> Result<String> {
    let text = require_str(args, "text", "sha256_hex")?;
    let mut hasher = Sha256::new();
    hasher.update(text.as_bytes());
    Ok(hex::encode(hasher.finalize()))
}

/// `crc32 {text}` -> the IEEE CRC-32 of the UTF-8 bytes as 8 lowercase hex digits.
/// Standard reflected polynomial 0xEDB88320, init 0xFFFFFFFF, final XOR — the
/// variant zip/png/gzip use. Computed table-free so it is self-contained. Pure.
fn crc32(args: &Value) -> Result<String> {
    let text = require_str(args, "text", "crc32")?;
    Ok(format!("{:08x}", crc32_ieee(text.as_bytes())))
}

/// Pure bit-by-bit IEEE CRC-32. No precomputed table (the inputs are short), so
/// the algorithm is fully visible and verifiable against known vectors.
fn crc32_ieee(data: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFF_FFFF;
    for &byte in data {
        crc ^= byte as u32;
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg(); // 0xFFFFFFFF if low bit set, else 0
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

// ---------------------------------------------------------------------------
// Slug + case conversion
// ---------------------------------------------------------------------------

/// Split text into ASCII-lowercase "words". A word is a maximal run of
/// alphanumerics; case boundaries inside an identifier (camelCase, PascalCase)
/// and digit/letter boundaries also split, so `getHTTPResponse2` ->
/// `[get, http, response, 2]`. The shared tokenizer for slugify + case_convert.
fn tokenize_words(s: &str) -> Vec<String> {
    let chars: Vec<char> = s.chars().collect();
    let mut words: Vec<String> = Vec::new();
    let mut cur = String::new();
    let push = |cur: &mut String, words: &mut Vec<String>| {
        if !cur.is_empty() {
            words.push(std::mem::take(cur));
        }
    };
    for i in 0..chars.len() {
        let c = chars[i];
        if !c.is_alphanumeric() {
            push(&mut cur, &mut words);
            continue;
        }
        // Boundary BEFORE this char inside an alphanumeric run:
        if let Some(&prev) = chars.get(i.wrapping_sub(1)).filter(|_| i > 0) {
            let lower_to_upper = prev.is_lowercase() && c.is_uppercase();
            let digit_boundary = prev.is_numeric() != c.is_numeric();
            // ACRONYM->Word boundary: UPPER followed by Upper+lower, e.g. HTTPResponse.
            let acronym_end = prev.is_uppercase()
                && c.is_uppercase()
                && chars.get(i + 1).map(|n| n.is_lowercase()).unwrap_or(false);
            if lower_to_upper || digit_boundary || acronym_end {
                push(&mut cur, &mut words);
            }
        }
        for lc in c.to_lowercase() {
            cur.push(lc);
        }
    }
    push(&mut cur, &mut words);
    words
}

/// `slugify {text}` -> a clean URL/file slug: tokenized words joined by single
/// hyphens, all lowercase ASCII-folded. Empty/symbol-only input is an error
/// (there is no honest slug for it). Pure.
fn slugify(args: &Value) -> Result<String> {
    let text = require_str(args, "text", "slugify")?;
    let slug = tokenize_words(text).join("-");
    if slug.is_empty() {
        return Err(anyhow!(
            "slugify: nothing slug-able in the input (no letters or digits)"
        ));
    }
    Ok(slug)
}

/// `case_convert {text, case}` -> the text reformatted into the requested case.
/// Supported: snake, kebab, camel, pascal, title, upper, lower, screaming_snake.
/// Pure; an unknown case or empty input is a friendly error.
fn case_convert(args: &Value) -> Result<String> {
    let text = require_str(args, "text", "case_convert")?;
    let case = require_str(args, "case", "case_convert")?.to_ascii_lowercase();
    let words = tokenize_words(text);
    if words.is_empty() {
        return Err(anyhow!("case_convert: no words to convert in the input"));
    }
    let titlecase = |w: &str| -> String {
        let mut c = w.chars();
        match c.next() {
            Some(first) => first.to_ascii_uppercase().to_string() + c.as_str(),
            None => String::new(),
        }
    };
    let out = match case.as_str() {
        "snake" | "snake_case" => words.join("_"),
        "kebab" | "kebab_case" | "kebab-case" => words.join("-"),
        "screaming_snake" | "screaming" | "constant" => words.join("_").to_ascii_uppercase(),
        "camel" | "camel_case" => {
            let mut s = words[0].clone();
            for w in &words[1..] {
                s.push_str(&titlecase(w));
            }
            s
        }
        "pascal" | "pascal_case" => words.iter().map(|w| titlecase(w)).collect::<String>(),
        "title" | "title_case" => {
            words.iter().map(|w| titlecase(w)).collect::<Vec<_>>().join(" ")
        }
        "upper" | "uppercase" => words.join(" ").to_ascii_uppercase(),
        "lower" | "lowercase" => words.join(" "),
        other => {
            return Err(anyhow!(
                "case_convert: unknown case '{other}' (try snake, kebab, camel, pascal, title, upper, lower, screaming_snake)"
            ))
        }
    };
    Ok(out)
}

// ---------------------------------------------------------------------------
// Byte-size humanize
// ---------------------------------------------------------------------------

/// `byte_size_humanize {bytes, binary?}` -> a human-readable size string. Binary
/// (default true) uses 1024-step KiB/MiB/GiB...; binary=false uses SI 1000-step
/// KB/MB/GB. Rounds to one decimal (whole numbers drop the `.0`). Pure.
fn byte_size_humanize(args: &Value) -> Result<String> {
    let bytes = args
        .get("bytes")
        .and_then(Value::as_u64)
        .ok_or_else(|| anyhow!("byte_size_humanize needs a non-negative integer 'bytes' argument"))?;
    let binary = args.get("binary").and_then(Value::as_bool).unwrap_or(true);
    let (base, units): (f64, &[&str]) = if binary {
        (1024.0, &["B", "KiB", "MiB", "GiB", "TiB", "PiB", "EiB"])
    } else {
        (1000.0, &["B", "KB", "MB", "GB", "TB", "PB", "EB"])
    };
    if bytes < base as u64 {
        return Ok(format!("{bytes} B"));
    }
    let mut size = bytes as f64;
    let mut unit = 0usize;
    while size >= base && unit < units.len() - 1 {
        size /= base;
        unit += 1;
    }
    // One decimal place; trim a trailing ".0" for clean whole values.
    let rounded = (size * 10.0).round() / 10.0;
    let num = if (rounded.fract()).abs() < f64::EPSILON {
        format!("{}", rounded as u64)
    } else {
        format!("{rounded:.1}")
    };
    Ok(format!("{num} {}", units[unit]))
}

// ---------------------------------------------------------------------------
// Seeded id + password generation (deterministic, no ambient entropy)
// ---------------------------------------------------------------------------

/// `uuid_v4 {seed}` -> a canonical RFC 4122 version-4 UUID built from a REQUIRED
/// integer seed via SplitMix64, with the version (4) and variant (10xx) bits set
/// per the spec. Same seed => same UUID, every time. Reproducible by design — for
/// ambient entropy a caller would supply OS randomness as the seed. Pure.
fn uuid_v4(args: &Value) -> Result<String> {
    let seed = args
        .get("seed")
        .and_then(Value::as_u64)
        .ok_or_else(|| anyhow!("uuid_v4 needs an integer 'seed' (so the UUID is reproducible)"))?;
    let mut state = seed;
    // 16 bytes from two SplitMix64 draws.
    let mut bytes = [0u8; 16];
    let hi = splitmix64(&mut state).to_be_bytes();
    let lo = splitmix64(&mut state).to_be_bytes();
    bytes[..8].copy_from_slice(&hi);
    bytes[8..].copy_from_slice(&lo);
    // Set the version (4) in the high nibble of byte 6.
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    // Set the variant (10xx) in the top two bits of byte 8.
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let h = hex::encode(bytes);
    Ok(format!(
        "{}-{}-{}-{}-{}",
        &h[0..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..32],
    ))
}

/// `password_gen {seed, length?}` -> a strong password built deterministically
/// from a REQUIRED integer seed. Length defaults to 16 (bounded 8..=128). The
/// result is GUARANTEED to contain at least one lowercase, uppercase, digit, and
/// symbol (the first four positions are seeded picks from each class, then the
/// whole string is seed-shuffled so the classes are not positionally predictable).
/// Reproducible — seed it; this is a deterministic generator, not an entropy
/// source for live secrets. Pure.
fn password_gen(args: &Value) -> Result<String> {
    let seed = args
        .get("seed")
        .and_then(Value::as_u64)
        .ok_or_else(|| anyhow!("password_gen needs an integer 'seed' (so the password is reproducible)"))?;
    let length = args.get("length").and_then(Value::as_u64).unwrap_or(16);
    if !(8..=128).contains(&length) {
        return Err(anyhow!("password_gen 'length' must be 8..=128"));
    }
    const LOWER: &[u8] = b"abcdefghijklmnopqrstuvwxyz";
    const UPPER: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ";
    const DIGIT: &[u8] = b"0123456789";
    const SYMBOL: &[u8] = b"!@#$%^&*()-_=+[]{};:,.?";
    let all: Vec<u8> = [LOWER, UPPER, DIGIT, SYMBOL].concat();

    let mut state = seed;
    let pick = |state: &mut u64, set: &[u8]| set[(splitmix64(state) % set.len() as u64) as usize];

    let mut chars: Vec<u8> = Vec::with_capacity(length as usize);
    // Guarantee one of each class first.
    chars.push(pick(&mut state, LOWER));
    chars.push(pick(&mut state, UPPER));
    chars.push(pick(&mut state, DIGIT));
    chars.push(pick(&mut state, SYMBOL));
    // Fill the rest from the full alphabet.
    for _ in 4..length {
        chars.push(pick(&mut state, &all));
    }
    // Fisher-Yates shuffle (seeded) so the guaranteed classes are not stuck at the
    // front — deterministic for the same seed.
    for i in (1..chars.len()).rev() {
        let j = (splitmix64(&mut state) % (i as u64 + 1)) as usize;
        chars.swap(i, j);
    }
    // Every byte is ASCII-printable by construction.
    Ok(String::from_utf8(chars).expect("password bytes are ASCII by construction"))
}

// ---------------------------------------------------------------------------
// Counters / proof skills
// ---------------------------------------------------------------------------

/// `word_count {text}` -> a one-line report of words / characters / lines. Words
/// are whitespace-separated runs; characters are Unicode scalar values; lines are
/// `\n`-separated (a trailing newline does not add a phantom empty line). Pure.
fn word_count(args: &Value) -> Result<String> {
    let text = require_str(args, "text", "word_count")?;
    let words = text.split_whitespace().count();
    let chars = text.chars().count();
    let lines = if text.is_empty() {
        0
    } else {
        text.lines().count()
    };
    Ok(format!("{words} words, {chars} characters, {lines} lines"))
}

/// `dice_roll {seed, count?, sides?}` -> a deterministic roll. `count` (default 1)
/// dice of `sides` (default 6) each, driven entirely by the REQUIRED integer
/// `seed` — same seed + same dice => same roll, every time. No ambient
/// randomness: this is a pure function of (seed, count, sides). The PRNG is a
/// tiny SplitMix64, ample for a game roll and fully reproducible.
fn dice_roll(args: &Value) -> Result<String> {
    let seed = args
        .get("seed")
        .and_then(Value::as_u64)
        .ok_or_else(|| anyhow!("dice_roll needs an integer 'seed' (so the roll is reproducible)"))?;
    let count = args.get("count").and_then(Value::as_u64).unwrap_or(1);
    let sides = args.get("sides").and_then(Value::as_u64).unwrap_or(6);
    if !(1..=100).contains(&count) {
        return Err(anyhow!("dice_roll 'count' must be 1..=100"));
    }
    if !(2..=1000).contains(&sides) {
        return Err(anyhow!("dice_roll 'sides' must be 2..=1000"));
    }
    let mut state = seed;
    let mut rolls = Vec::with_capacity(count as usize);
    let mut total: u64 = 0;
    for _ in 0..count {
        let r = splitmix64(&mut state) % sides + 1;
        total += r;
        rolls.push(r.to_string());
    }
    Ok(format!(
        "{count}d{sides} (seed {seed}): {} = {total}",
        rolls.join(" + ")
    ))
}

// ---------------------------------------------------------------------------
// Ciphers / encoders (pure, deterministic) — Library batch
// ---------------------------------------------------------------------------

/// `rot13 {text}` -> ROT13 of the ASCII letters; everything else passes through.
/// Self-inverse. Pure.
fn rot13(args: &Value) -> Result<String> {
    let text = require_str(args, "text", "rot13")?;
    Ok(text
        .chars()
        .map(|c| match c {
            'A'..='Z' => (((c as u8 - b'A' + 13) % 26) + b'A') as char,
            'a'..='z' => (((c as u8 - b'a' + 13) % 26) + b'a') as char,
            _ => c,
        })
        .collect())
}

/// `caesar_cipher {text, shift}` -> letters shifted by `shift` (mod 26, wrapping;
/// a negative shift decodes); non-letters pass through. Pure.
fn caesar_cipher(args: &Value) -> Result<String> {
    let text = require_str(args, "text", "caesar_cipher")?;
    let shift = args
        .get("shift")
        .and_then(Value::as_i64)
        .ok_or_else(|| anyhow!("caesar_cipher needs an integer 'shift'"))?;
    let s = shift.rem_euclid(26) as u8;
    Ok(text
        .chars()
        .map(|c| match c {
            'A'..='Z' => (((c as u8 - b'A' + s) % 26) + b'A') as char,
            'a'..='z' => (((c as u8 - b'a' + s) % 26) + b'a') as char,
            _ => c,
        })
        .collect())
}

/// `luhn_check {number}` -> "valid" or "invalid" per the Luhn checksum. Spaces and
/// dashes are ignored; any other non-digit is a friendly error. Pure.
fn luhn_check(args: &Value) -> Result<String> {
    let number = require_str(args, "number", "luhn_check")?;
    let mut digits: Vec<u32> = Vec::new();
    for c in number.chars() {
        if c.is_whitespace() || c == '-' {
            continue;
        }
        digits.push(
            c.to_digit(10)
                .ok_or_else(|| anyhow!("luhn_check: '{c}' is not a digit"))?,
        );
    }
    if digits.is_empty() {
        return Err(anyhow!("luhn_check: no digits in the input"));
    }
    let mut sum = 0u32;
    let mut double = false;
    for &d in digits.iter().rev() {
        let v = if double {
            let x = d * 2;
            if x > 9 {
                x - 9
            } else {
                x
            }
        } else {
            d
        };
        sum += v;
        double = !double;
    }
    Ok(if sum.is_multiple_of(10) {
        "valid".to_string()
    } else {
        "invalid".to_string()
    })
}

/// `rgb_to_hex {r,g,b}` -> a #rrggbb hex string (each channel an integer 0..=255).
/// Pure.
fn rgb_to_hex(args: &Value) -> Result<String> {
    let chan = |k: &str| -> Result<u64> {
        args.get(k)
            .and_then(Value::as_u64)
            .filter(|v| *v <= 255)
            .ok_or_else(|| anyhow!("rgb_to_hex needs integer 'r','g','b', each 0..=255"))
    };
    Ok(format!(
        "#{:02x}{:02x}{:02x}",
        chan("r")?,
        chan("g")?,
        chan("b")?
    ))
}

/// `hex_to_rgb {hex}` -> an rgb(r, g, b) string from a #rrggbb hex color (the
/// leading # is optional). Pure.
fn hex_to_rgb(args: &Value) -> Result<String> {
    let raw = require_str(args, "hex", "hex_to_rgb")?;
    let hex = raw.trim().trim_start_matches('#');
    if hex.len() != 6 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(anyhow!("hex_to_rgb needs a 6-digit hex color like #ff0080"));
    }
    let parse = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).expect("validated hex pair");
    Ok(format!("rgb({}, {}, {})", parse(0), parse(2), parse(4)))
}

/// `binary_encode {text}` -> space-separated 8-bit binary, one octet per UTF-8
/// byte. Pure.
fn binary_encode(args: &Value) -> Result<String> {
    let text = require_str(args, "text", "binary_encode")?;
    if text.is_empty() {
        return Err(anyhow!("binary_encode: empty input"));
    }
    Ok(text
        .bytes()
        .map(|b| format!("{b:08b}"))
        .collect::<Vec<_>>()
        .join(" "))
}

// ---------------------------------------------------------------------------
// sql_guard — read-only SQL safety classifier (the core a query tool gates on)
// ---------------------------------------------------------------------------

/// `sql_guard {sql}` -> "read-only: OK" or "REJECTED: <reason>". A FAIL-CLOSED
/// classifier: a query is read-only only if it is a single SELECT/WITH/EXPLAIN
/// statement with no comments, no extra statements, and no write/DDL keyword
/// token anywhere. Conservative by design — it rejects a query that merely
/// MENTIONS a write keyword (e.g. in a string literal), because for a safety gate
/// a false reject is fine and a false accept is not. Pure + total (only a missing
/// 'sql' arg is an error; every query yields a verdict, never a panic).
fn sql_guard(args: &Value) -> Result<String> {
    let sql = require_str(args, "sql", "sql_guard")?;
    Ok(classify_sql(sql))
}

/// Pure verdict function behind [`sql_guard`], factored out so it is exhaustively
/// unit-tested without the arg-plumbing.
fn classify_sql(sql: &str) -> String {
    let trimmed = sql.trim().trim_end_matches(';').trim();
    if trimmed.is_empty() {
        return "REJECTED: empty query".to_string();
    }
    if trimmed.contains("--") || trimmed.contains("/*") {
        return "REJECTED: comments are not allowed".to_string();
    }
    // A ';' that survived the trailing-trim means more than one statement.
    if trimmed.contains(';') {
        return "REJECTED: multiple statements are not allowed".to_string();
    }
    let upper = trimmed.to_ascii_uppercase();
    let first = upper.split_whitespace().next().unwrap_or("");
    if !matches!(first, "SELECT" | "WITH" | "EXPLAIN") {
        return format!("REJECTED: must start with SELECT, WITH, or EXPLAIN (got '{first}')");
    }
    // Write/DDL keywords are forbidden as whole tokens ANYWHERE (a writable CTE,
    // a stacked write, etc.). Tokenize on non-identifier chars so substrings of a
    // larger identifier (e.g. `updated_at`) do not trip it.
    const DENY: &[&str] = &[
        "INSERT", "UPDATE", "DELETE", "DROP", "ALTER", "CREATE", "REPLACE", "TRUNCATE", "ATTACH",
        "DETACH", "PRAGMA", "VACUUM", "REINDEX", "GRANT", "REVOKE", "MERGE", "UPSERT",
    ];
    let tokens: std::collections::HashSet<&str> = upper
        .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
        .filter(|t| !t.is_empty())
        .collect();
    for &kw in DENY {
        if tokens.contains(kw) {
            return format!("REJECTED: contains the write/DDL keyword '{kw}'");
        }
    }
    "read-only: OK".to_string()
}

/// `levenshtein {a, b}` -> the edit distance (insert/delete/substitute) as an
/// integer string. Two-row DP, O(|a|*|b|) time / O(|b|) space. Pure.
fn levenshtein(args: &Value) -> Result<String> {
    let a = require_str(args, "a", "levenshtein")?;
    let b = require_str(args, "b", "levenshtein")?;
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut curr: Vec<usize> = vec![0; b.len() + 1];
    for (i, ca) in a.iter().enumerate() {
        curr[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let cost = usize::from(ca != cb);
            curr[j + 1] = (prev[j + 1] + 1).min(curr[j] + 1).min(prev[j] + cost);
        }
        std::mem::swap(&mut prev, &mut curr);
    }
    Ok(prev[b.len()].to_string())
}

/// `hamming_distance {a, b}` -> the number of positions at which equal-length
/// strings differ. Errors when the lengths differ. Pure.
fn hamming_distance(args: &Value) -> Result<String> {
    let a: Vec<char> = require_str(args, "a", "hamming_distance")?.chars().collect();
    let b: Vec<char> = require_str(args, "b", "hamming_distance")?.chars().collect();
    if a.len() != b.len() {
        return Err(anyhow!(
            "hamming_distance: strings must be equal length ({} vs {})",
            a.len(),
            b.len()
        ));
    }
    Ok(a.iter().zip(b.iter()).filter(|(x, y)| x != y).count().to_string())
}

/// `jaccard_similarity {a, b}` -> |A∩B| / |A∪B| over the whitespace-split word
/// sets, as a 2-dp ratio. Two empty texts are identical (1.00). Pure.
fn jaccard_similarity(args: &Value) -> Result<String> {
    use std::collections::HashSet;
    let a = require_str(args, "a", "jaccard_similarity")?;
    let b = require_str(args, "b", "jaccard_similarity")?;
    let sa: HashSet<&str> = a.split_whitespace().collect();
    let sb: HashSet<&str> = b.split_whitespace().collect();
    if sa.is_empty() && sb.is_empty() {
        return Ok("1.00".to_string());
    }
    let inter = sa.intersection(&sb).count();
    let union = sa.union(&sb).count();
    Ok(format!("{:.2}", inter as f64 / union as f64))
}

/// `base32_encode {text}` -> standard RFC 4648 Base32 of the UTF-8 bytes
/// (A-Z2-7, '=' padding). Hand-rolled (no new dep), matching the SHA/CRC skills'
/// "real algorithm, never an approximation" discipline. Pure.
fn base32_encode(args: &Value) -> Result<String> {
    const ALPHA: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";
    let bytes = require_str(args, "text", "base32_encode")?.as_bytes();
    let mut out = String::new();
    for chunk in bytes.chunks(5) {
        let mut b = [0u8; 5];
        b[..chunk.len()].copy_from_slice(chunk);
        let idx = [
            b[0] >> 3,
            ((b[0] & 0x07) << 2) | (b[1] >> 6),
            (b[1] >> 1) & 0x1f,
            ((b[1] & 0x01) << 4) | (b[2] >> 4),
            ((b[2] & 0x0f) << 1) | (b[3] >> 7),
            (b[3] >> 2) & 0x1f,
            ((b[3] & 0x03) << 3) | (b[4] >> 5),
            b[4] & 0x1f,
        ];
        // Output chars carrying real bits for this chunk length; the rest is '='.
        let filled = match chunk.len() {
            1 => 2,
            2 => 4,
            3 => 5,
            4 => 7,
            _ => 8,
        };
        for &i in idx.iter().take(filled) {
            out.push(ALPHA[i as usize] as char);
        }
        for _ in filled..8 {
            out.push('=');
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// Pull a required string arg or return a friendly, skill-named error. Centralizes
/// the "needs a 'text' string argument" pattern so every encoder reports the same
/// way and no skill panics on a missing/typed-wrong arg.
fn require_str<'a>(args: &'a Value, key: &str, skill: &str) -> Result<&'a str> {
    args.get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("{skill} needs a '{key}' string argument"))
}

/// SplitMix64: a tiny, well-known, fully deterministic PRNG step. Advances
/// `state` and returns a scrambled 64-bit value. Seeded, so the dice roll / UUID /
/// password are reproducible — no OS entropy, no clock.
fn splitmix64(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ---- base64 -----------------------------------------------------------

    #[test]
    fn base64_encode_matches_known_vectors() {
        // RFC 4648 test vectors — pins the encoder exactly.
        assert_eq!(encode_b64(b""), "");
        assert_eq!(encode_b64(b"f"), "Zg==");
        assert_eq!(encode_b64(b"fo"), "Zm8=");
        assert_eq!(encode_b64(b"foo"), "Zm9v");
        assert_eq!(encode_b64(b"foob"), "Zm9vYg==");
        assert_eq!(encode_b64(b"fooba"), "Zm9vYmE=");
        assert_eq!(encode_b64(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn base64_encode_skill_reads_text_arg() {
        // "hi" -> "aGk=" is the canonical pin from the task.
        assert_eq!(base64_encode(&json!({"text": "hi"})).unwrap(), "aGk=");
        assert_eq!(base64_encode(&json!({"text": "hello"})).unwrap(), "aGVsbG8=");
        assert!(base64_encode(&json!({})).is_err(), "missing text -> friendly error");
        assert!(base64_encode(&json!({"text": 7})).is_err(), "non-string text -> error");
    }

    #[test]
    fn base64_decode_roundtrips_and_pins_vectors() {
        assert_eq!(base64_decode(&json!({"text": "aGk="})).unwrap(), "hi");
        assert_eq!(base64_decode(&json!({"text": "Zm9vYmFy"})).unwrap(), "foobar");
        assert_eq!(base64_decode(&json!({"text": ""})).unwrap(), "");
        // Round-trip a non-trivial string.
        let original = "The quick brown fox jumps over 13 lazy dogs!";
        let enc = base64_encode(&json!({"text": original})).unwrap();
        assert_eq!(base64_decode(&json!({"text": enc})).unwrap(), original);
    }

    #[test]
    fn base64_decode_rejects_bad_input() {
        assert!(base64_decode(&json!({"text": "aGk"})).is_err(), "length not /4");
        assert!(base64_decode(&json!({"text": "aG!="})).is_err(), "illegal char");
        assert!(base64_decode(&json!({"text": "=AAA"})).is_err(), "misplaced padding");
        assert!(base64_decode(&json!({})).is_err(), "missing arg");
    }

    // ---- hex --------------------------------------------------------------

    #[test]
    fn hex_encode_and_decode_roundtrip() {
        assert_eq!(hex_encode(&json!({"text": "hi"})).unwrap(), "6869");
        assert_eq!(hex_encode(&json!({"text": "ABC"})).unwrap(), "414243");
        assert_eq!(hex_decode(&json!({"text": "6869"})).unwrap(), "hi");
        assert_eq!(hex_decode(&json!({"text": "414243"})).unwrap(), "ABC");
        // Round trip.
        let h = hex_encode(&json!({"text": "round-trip ✓"})).unwrap();
        assert_eq!(hex_decode(&json!({"text": h})).unwrap(), "round-trip ✓");
    }

    #[test]
    fn hex_decode_rejects_bad_input() {
        assert!(hex_decode(&json!({"text": "abc"})).is_err(), "odd length");
        assert!(hex_decode(&json!({"text": "zz"})).is_err(), "non-hex digit");
        assert!(hex_encode(&json!({})).is_err(), "missing arg");
    }

    // ---- url --------------------------------------------------------------

    #[test]
    fn url_encode_keeps_unreserved_and_escapes_the_rest() {
        assert_eq!(url_encode(&json!({"text": "a b"})).unwrap(), "a%20b");
        assert_eq!(
            url_encode(&json!({"text": "hello world/+&=?"})).unwrap(),
            "hello%20world%2F%2B%26%3D%3F"
        );
        // Unreserved set is preserved literally.
        assert_eq!(url_encode(&json!({"text": "A-Z_a.z~9"})).unwrap(), "A-Z_a.z~9");
        // UTF-8 multibyte -> per-byte percent escapes.
        assert_eq!(url_encode(&json!({"text": "é"})).unwrap(), "%C3%A9");
    }

    #[test]
    fn url_decode_roundtrips_and_rejects_bad_escapes() {
        assert_eq!(url_decode(&json!({"text": "a%20b"})).unwrap(), "a b");
        assert_eq!(url_decode(&json!({"text": "%C3%A9"})).unwrap(), "é");
        // `+` is a literal plus for a URI component (not a space).
        assert_eq!(url_decode(&json!({"text": "a+b"})).unwrap(), "a+b");
        let original = "key=value&x=1 2/3?";
        let enc = url_encode(&json!({"text": original})).unwrap();
        assert_eq!(url_decode(&json!({"text": enc})).unwrap(), original);
        assert!(url_decode(&json!({"text": "%2"})).is_err(), "truncated escape");
        assert!(url_decode(&json!({"text": "%ZZ"})).is_err(), "bad hex");
    }

    // ---- sha256 / crc32 ---------------------------------------------------

    #[test]
    fn sha256_hex_matches_known_vectors() {
        // NIST/RFC well-known digests.
        assert_eq!(
            sha256_hex(&json!({"text": ""})).unwrap(),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_hex(&json!({"text": "abc"})).unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            sha256_hex(&json!({"text": "hello"})).unwrap(),
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
        assert!(sha256_hex(&json!({})).is_err(), "missing arg");
    }

    #[test]
    fn crc32_matches_known_vectors() {
        // The canonical CRC-32/ISO-HDLC of "123456789" is 0xCBF43926.
        assert_eq!(crc32(&json!({"text": "123456789"})).unwrap(), "cbf43926");
        // CRC of empty input is 0.
        assert_eq!(crc32(&json!({"text": ""})).unwrap(), "00000000");
        // "The quick brown fox jumps over the lazy dog" -> 0x414FA339.
        assert_eq!(
            crc32(&json!({"text": "The quick brown fox jumps over the lazy dog"})).unwrap(),
            "414fa339"
        );
        assert!(crc32(&json!({})).is_err(), "missing arg");
    }

    // ---- slugify / case ---------------------------------------------------

    #[test]
    fn slugify_makes_clean_slugs() {
        assert_eq!(slugify(&json!({"text": "Hello, World!"})).unwrap(), "hello-world");
        assert_eq!(
            slugify(&json!({"text": "  Multiple   Spaces & Dashes--here "})).unwrap(),
            "multiple-spaces-dashes-here"
        );
        assert_eq!(slugify(&json!({"text": "getHTTPResponse2"})).unwrap(), "get-http-response-2");
        assert!(slugify(&json!({"text": "!!!"})).is_err(), "no slug-able content");
        assert!(slugify(&json!({})).is_err(), "missing arg");
    }

    #[test]
    fn case_convert_covers_every_supported_case() {
        let t = "getHTTPResponse code";
        assert_eq!(case_convert(&json!({"text": t, "case": "snake"})).unwrap(), "get_http_response_code");
        assert_eq!(case_convert(&json!({"text": t, "case": "kebab"})).unwrap(), "get-http-response-code");
        assert_eq!(case_convert(&json!({"text": t, "case": "camel"})).unwrap(), "getHttpResponseCode");
        assert_eq!(case_convert(&json!({"text": t, "case": "pascal"})).unwrap(), "GetHttpResponseCode");
        assert_eq!(case_convert(&json!({"text": t, "case": "title"})).unwrap(), "Get Http Response Code");
        assert_eq!(case_convert(&json!({"text": t, "case": "upper"})).unwrap(), "GET HTTP RESPONSE CODE");
        assert_eq!(case_convert(&json!({"text": t, "case": "lower"})).unwrap(), "get http response code");
        assert_eq!(
            case_convert(&json!({"text": "my var name", "case": "screaming_snake"})).unwrap(),
            "MY_VAR_NAME"
        );
        // Simple round-trip-ish check from snake to camel.
        assert_eq!(
            case_convert(&json!({"text": "user_id", "case": "camel"})).unwrap(),
            "userId"
        );
    }

    #[test]
    fn case_convert_rejects_unknown_case_and_empty() {
        assert!(case_convert(&json!({"text": "x", "case": "weird"})).is_err(), "unknown case");
        assert!(case_convert(&json!({"text": "!!!", "case": "snake"})).is_err(), "no words");
        assert!(case_convert(&json!({"text": "x"})).is_err(), "missing case arg");
        assert!(case_convert(&json!({"case": "snake"})).is_err(), "missing text arg");
    }

    // ---- byte humanize ----------------------------------------------------

    #[test]
    fn byte_size_humanize_binary_and_si() {
        // Binary (default) — 1024-step.
        assert_eq!(byte_size_humanize(&json!({"bytes": 0})).unwrap(), "0 B");
        assert_eq!(byte_size_humanize(&json!({"bytes": 512})).unwrap(), "512 B");
        assert_eq!(byte_size_humanize(&json!({"bytes": 1024})).unwrap(), "1 KiB");
        assert_eq!(byte_size_humanize(&json!({"bytes": 1536})).unwrap(), "1.5 KiB");
        assert_eq!(byte_size_humanize(&json!({"bytes": 1048576})).unwrap(), "1 MiB");
        assert_eq!(byte_size_humanize(&json!({"bytes": 1073741824})).unwrap(), "1 GiB");
        // SI — 1000-step.
        assert_eq!(byte_size_humanize(&json!({"bytes": 1000, "binary": false})).unwrap(), "1 KB");
        assert_eq!(byte_size_humanize(&json!({"bytes": 1500, "binary": false})).unwrap(), "1.5 KB");
        assert_eq!(byte_size_humanize(&json!({"bytes": 1000000, "binary": false})).unwrap(), "1 MB");
        assert!(byte_size_humanize(&json!({})).is_err(), "missing bytes");
        assert!(byte_size_humanize(&json!({"bytes": -1})).is_err(), "negative rejected");
    }

    // ---- uuid_v4 ----------------------------------------------------------

    #[test]
    fn uuid_v4_is_well_formed_and_seed_reproducible() {
        let a = uuid_v4(&json!({"seed": 42})).unwrap();
        let b = uuid_v4(&json!({"seed": 42})).unwrap();
        assert_eq!(a, b, "same seed -> same UUID");
        // Canonical 8-4-4-4-12 shape.
        let parts: Vec<&str> = a.split('-').collect();
        assert_eq!(parts.iter().map(|p| p.len()).collect::<Vec<_>>(), vec![8, 4, 4, 4, 12]);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit() || c == '-'));
        // Version nibble is '4' (first char of the 3rd group).
        assert_eq!(parts[2].chars().next().unwrap(), '4', "version 4");
        // Variant nibble (first char of the 4th group) is one of 8,9,a,b.
        assert!(matches!(parts[3].chars().next().unwrap(), '8' | '9' | 'a' | 'b'), "RFC 4122 variant");
        // Different seed -> different UUID.
        assert_ne!(a, uuid_v4(&json!({"seed": 43})).unwrap());
        assert!(uuid_v4(&json!({})).is_err(), "missing seed");
    }

    // ---- password_gen -----------------------------------------------------

    #[test]
    fn password_gen_is_strong_and_reproducible() {
        let a = password_gen(&json!({"seed": 1})).unwrap();
        let b = password_gen(&json!({"seed": 1})).unwrap();
        assert_eq!(a, b, "same seed -> same password");
        assert_eq!(a.len(), 16, "default length");
        // Guaranteed class coverage.
        assert!(a.chars().any(|c| c.is_ascii_lowercase()), "has lowercase");
        assert!(a.chars().any(|c| c.is_ascii_uppercase()), "has uppercase");
        assert!(a.chars().any(|c| c.is_ascii_digit()), "has a digit");
        assert!(a.chars().any(|c| !c.is_ascii_alphanumeric()), "has a symbol");
        // Custom length honored; bounds enforced.
        assert_eq!(password_gen(&json!({"seed": 9, "length": 32})).unwrap().len(), 32);
        assert!(password_gen(&json!({"seed": 1, "length": 4})).is_err(), "too short");
        assert!(password_gen(&json!({"seed": 1, "length": 200})).is_err(), "too long");
        assert!(password_gen(&json!({})).is_err(), "missing seed");
        // Different seed -> different password.
        assert_ne!(a, password_gen(&json!({"seed": 2})).unwrap());
    }

    // ---- proof skills (preserved) ----------------------------------------

    #[test]
    fn word_count_is_accurate_and_deterministic() {
        let out = word_count(&json!({"text": "the quick brown fox"})).unwrap();
        assert_eq!(out, "4 words, 19 characters, 1 lines");
        let out = word_count(&json!({"text": "one two\nthree"})).unwrap();
        assert_eq!(out, "3 words, 13 characters, 2 lines");
        assert_eq!(word_count(&json!({"text": ""})).unwrap(), "0 words, 0 characters, 0 lines");
        let a = word_count(&json!({"text": "a b c"})).unwrap();
        let b = word_count(&json!({"text": "a b c"})).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn word_count_requires_text() {
        assert!(word_count(&json!({})).is_err());
    }

    #[test]
    fn dice_roll_is_reproducible_from_the_seed() {
        let a = dice_roll(&json!({"seed": 42, "count": 2, "sides": 6})).unwrap();
        let b = dice_roll(&json!({"seed": 42, "count": 2, "sides": 6})).unwrap();
        assert_eq!(a, b, "a seeded roll is deterministic");
        assert!(a.starts_with("2d6 (seed 42): "));
        let c = dice_roll(&json!({"seed": 43, "count": 2, "sides": 6})).unwrap();
        assert_ne!(a, c);
    }

    #[test]
    fn dice_roll_values_are_in_range_and_total_adds_up() {
        let out = dice_roll(&json!({"seed": 7, "count": 5, "sides": 20})).unwrap();
        let (lhs, rhs) = out.split_once(" = ").unwrap();
        let total: u64 = rhs.parse().unwrap();
        let dice_part = lhs.split(": ").nth(1).unwrap();
        let vals: Vec<u64> = dice_part.split(" + ").map(|v| v.parse().unwrap()).collect();
        assert_eq!(vals.len(), 5);
        assert!(vals.iter().all(|&v| (1..=20).contains(&v)), "each die in 1..=20");
        assert_eq!(vals.iter().sum::<u64>(), total, "total is the sum");
    }

    #[test]
    fn dice_roll_validates_args() {
        assert!(dice_roll(&json!({})).is_err(), "missing seed -> error");
        assert!(dice_roll(&json!({"seed": 1, "count": 0})).is_err(), "count 0 rejected");
        assert!(dice_roll(&json!({"seed": 1, "sides": 1})).is_err(), "1-sided rejected");
        assert!(dice_roll(&json!({"seed": 1, "count": 101})).is_err(), "too many dice");
    }

    // ---- library batch: ciphers / encoders --------------------------------

    #[test]
    fn library_batch_matches_known_vectors() {
        assert_eq!(rot13(&json!({"text": "Hello"})).unwrap(), "Uryyb");
        assert_eq!(rot13(&json!({"text": "Uryyb"})).unwrap(), "Hello");
        assert_eq!(caesar_cipher(&json!({"text": "abc", "shift": 1})).unwrap(), "bcd");
        assert_eq!(caesar_cipher(&json!({"text": "bcd", "shift": -1})).unwrap(), "abc");
        assert_eq!(luhn_check(&json!({"number": "79927398713"})).unwrap(), "valid");
        assert_eq!(luhn_check(&json!({"number": "79927398710"})).unwrap(), "invalid");
        assert_eq!(rgb_to_hex(&json!({"r": 255, "g": 0, "b": 128})).unwrap(), "#ff0080");
        assert_eq!(hex_to_rgb(&json!({"hex": "#ff0080"})).unwrap(), "rgb(255, 0, 128)");
        assert_eq!(binary_encode(&json!({"text": "Hi"})).unwrap(), "01001000 01101001");
        // Argument validation — friendly errors, never panics.
        assert!(rot13(&json!({})).is_err());
        assert!(caesar_cipher(&json!({"text": "x"})).is_err(), "missing shift");
        assert!(rgb_to_hex(&json!({"r": 256, "g": 0, "b": 0})).is_err(), "channel out of range");
        assert!(hex_to_rgb(&json!({"hex": "xyz"})).is_err(), "bad hex");
    }

    // ---- sql_guard --------------------------------------------------------

    #[test]
    fn sql_guard_accepts_read_only_and_rejects_the_rest() {
        let ok = |q: &str| sql_guard(&json!({ "sql": q })).unwrap();
        // Accepts a single SELECT/WITH/EXPLAIN.
        assert_eq!(ok("SELECT * FROM traces"), "read-only: OK");
        assert_eq!(ok("  select id, ts from traces where outcome = 'failed'  "), "read-only: OK");
        assert_eq!(ok("WITH r AS (SELECT 1) SELECT * FROM r"), "read-only: OK");
        assert_eq!(ok("EXPLAIN SELECT 1"), "read-only: OK");
        // A trailing semicolon is tolerated (single statement).
        assert_eq!(ok("SELECT 1;"), "read-only: OK");
        // An identifier that merely CONTAINS a keyword substring is fine.
        assert_eq!(ok("SELECT updated_at, created_at FROM traces"), "read-only: OK");
        // Rejects non-SELECT leads.
        assert_eq!(
            ok("UPDATE traces SET x=1"),
            "REJECTED: must start with SELECT, WITH, or EXPLAIN (got 'UPDATE')"
        );
        // Rejects a write keyword anywhere (writable CTE / smuggled write).
        assert!(ok("WITH x AS (DELETE FROM t RETURNING *) SELECT * FROM x").starts_with("REJECTED: contains"));
        // Rejects stacked statements and comments (fail-closed).
        assert_eq!(ok("SELECT 1; DROP TABLE t"), "REJECTED: multiple statements are not allowed");
        assert_eq!(ok("SELECT 1 -- ; DROP TABLE t"), "REJECTED: comments are not allowed");
        assert_eq!(ok("SELECT 1 /* hidden */"), "REJECTED: comments are not allowed");
        assert_eq!(ok("   "), "REJECTED: empty query");
        // Only a missing arg is a hard error.
        assert!(sql_guard(&json!({})).is_err());
    }

    // ---- catalog ----------------------------------------------------------

    #[test]
    fn skills_list_is_pure_and_well_formed() {
        let s = skills();
        // Every utilities skill is pure read-only (no consequential / source-gated).
        assert!(s.iter().all(|d| !d.consequential && !d.source_gated), "utilities skills are pure");
        // Names are unique within the category.
        let mut names: Vec<&str> = s.iter().map(|d| d.name).collect();
        let count = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), count, "no duplicate names within utilities");
        // The proof skills are still present (framework end-to-end path intact).
        assert!(s.iter().any(|d| d.name == "base64_encode"));
        assert!(s.iter().any(|d| d.name == "word_count"));
        assert!(s.iter().any(|d| d.name == "dice_roll"));
        // The library phase added a substantial set.
        assert!(count >= 11, "utilities ships the proof skills plus the library set");
    }
}
