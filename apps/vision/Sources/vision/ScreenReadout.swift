// ScreenReadout.swift — PURE actionable-region structuring over OCR text blocks.
//
// Responsibility: turn the raw `.text` Detections (recognized string + box +
// confidence, in Vision's normalized bottom-left coords) the OCR detector
// produces into a structured, read-only readout:
//   (a) the full readable text in READING ORDER (top-to-bottom, left-to-right);
//   (b) a list of candidate CONTROL labels — short, button-ish strings — each
//       with its box + center, so the host can answer "what can I press";
//   (c) a "where is <query>" LOCATOR -> the best-matching block's box + center.
//
// READ-ONLY by construction: this LOCATES / DESCRIBES a control. It does NOT
// click, actuate, or move anything. There is deliberately no actuation API here
// — clicking is a separate, out-of-scope, gated automation surface.
//
// DEFENSIVE: the input is glyph text, never a face/person identity. Nothing here
// attaches an identity; it only re-orders, filters, and matches strings.
//
// PURE + headlessly testable: every function is a value transform over
// [Detection] with no I/O, no clock, no capture — the unit tests drive it with
// synthesized `.text` detections and assert reading order, control candidacy,
// and locator matching deterministically.

import Foundation

// ===========================================================================
// ScreenReadout — the structured result of reading a screen's text.
// ===========================================================================

/// A structured, READ-ONLY readout derived purely from OCR `.text` blocks.
public struct ScreenReadout: Sendable, Equatable {

    /// One recognized text block in reading order: its string, normalized box
    /// (Vision coords, origin bottom-left), confidence, and whether it looks like
    /// an actionable control label. A locator/description target, never clickable.
    public struct Block: Sendable, Equatable {
        public let text: String
        public let box: DetectionBox
        public let confidence: Double
        /// True if this block reads like a short, button-ish control label.
        public let isControlCandidate: Bool

        public init(text: String, box: DetectionBox, confidence: Double, isControlCandidate: Bool) {
            self.text = text
            self.box = box
            self.confidence = confidence
            self.isControlCandidate = isControlCandidate
        }

        /// The block's center in Vision coords (origin bottom-left). A "where",
        /// purely for describing/locating — NOT a click target.
        public var center: (x: Double, y: Double) {
            (box.x + box.width / 2.0, box.y + box.height / 2.0)
        }
    }

    /// All recognized blocks, in reading order (top-to-bottom, left-to-right).
    public let blocks: [Block]
    /// The full readable text — every block joined in reading order by newlines.
    public let fullText: String
    /// The subset of `blocks` that read like actionable control labels.
    public let controls: [Block]

    public init(blocks: [Block]) {
        self.blocks = blocks
        self.fullText = blocks.map(\.text).joined(separator: "\n")
        self.controls = blocks.filter { $0.isControlCandidate }
    }
}

// ===========================================================================
// ScreenStructurer — the PURE transform: [Detection] -> ScreenReadout + locate.
// ===========================================================================

/// Pure structuring of OCR `.text` detections into a `ScreenReadout` plus a
/// "where is <query>" locator. No I/O — every method is a deterministic value
/// transform, so the unit tests drive it with literal detections.
public enum ScreenStructurer {

    /// A row-banding tolerance (fraction of the unit height): two blocks whose
    /// vertical centers are within this are treated as the SAME reading row, so
    /// they sort left-to-right rather than by a sub-pixel y jitter.
    public static let rowBandTolerance = 0.02

    /// Max words for a block to be a plausible CONTROL label. Buttons/menu items
    /// are short ("Submit", "Sign In", "OK"); a paragraph is not a control.
    public static let maxControlWords = 4
    /// Max characters for a plausible control label (defense against a long
    /// run-on line that happens to be few "words").
    public static let maxControlChars = 24

    // -- (a)+(b): structure blocks into reading order + control candidacy -----

    /// Build a `ScreenReadout` from OCR `.text` detections. Non-`.text`
    /// detections are ignored. Blocks are ordered top-to-bottom then
    /// left-to-right (reading order); each is flagged as a control candidate if
    /// it reads like a short button-ish label.
    public static func structure(_ detections: [Detection]) -> ScreenReadout {
        let textDets = detections.filter { $0.kind == .text && !$0.label.isEmpty }
        let ordered = readingOrder(textDets)
        let blocks = ordered.map { d in
            ScreenReadout.Block(
                text: d.label,
                box: d.boundingBox,
                confidence: d.confidence,
                isControlCandidate: isControlLabel(d.label))
        }
        return ScreenReadout(blocks: blocks)
    }

    /// Sort detections into reading order. Vision boxes use an origin at the
    /// BOTTOM-LEFT, so a higher `y` is HIGHER on screen and must read first. We
    /// band rows by vertical center (within `rowBandTolerance`) so a row of
    /// labels reads left-to-right instead of by tiny y differences.
    static func readingOrder(_ dets: [Detection]) -> [Detection] {
        // Stable indices so equal keys keep input order (deterministic).
        let indexed = Array(dets.enumerated())
        // Quantize each vertical center onto a FIXED band grid. Comparing bands
        // pairwise by |Δcenter| <= tolerance is NOT transitive — three labels whose
        // centers chain across the boundary form a sort cycle, so `sorted(by:)` gets
        // an invalid strict weak ordering and can scramble the reading order. A fixed
        // grid makes "same band" transitive: rows read top-to-bottom (Vision's origin
        // is bottom-left, so a HIGHER band is higher on screen and reads first) and,
        // within a band, left-to-right by x.
        func band(_ box: DetectionBox) -> Int { Int((centerY(box) / rowBandTolerance).rounded(.down)) }
        let sorted = indexed.sorted { a, b in
            let aband = band(a.element.boundingBox)
            let bband = band(b.element.boundingBox)
            if aband != bband { return aband > bband }   // higher on screen reads first
            let ax = a.element.boundingBox.x
            let bx = b.element.boundingBox.x
            if abs(ax - bx) > 1e-9 { return ax < bx }     // same row -> left-to-right
            return a.offset < b.offset                    // stable tiebreak
        }
        return sorted.map(\.element)
    }

    /// Whether a recognized string reads like an actionable control label: short
    /// (few words, bounded length) and non-empty after trimming. This is a
    /// heuristic DESCRIPTION ("this looks pressable"), never an actuation.
    static func isControlLabel(_ raw: String) -> Bool {
        let trimmed = raw.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !trimmed.isEmpty else { return false }
        guard trimmed.count <= maxControlChars else { return false }
        let words = trimmed.split { $0 == " " || $0 == "\t" || $0 == "\n" }
        guard !words.isEmpty, words.count <= maxControlWords else { return false }
        return true
    }

    // -- (c): "where is <query>" locator --------------------------------------

    /// The result of locating a query string on screen: the matched block plus
    /// a match score (1 = exact case-insensitive, lower = looser substring).
    /// READ-ONLY: it points AT a control; it does not press it.
    public struct Located: Sendable, Equatable {
        public let block: ScreenReadout.Block
        public let score: Double
        public init(block: ScreenReadout.Block, score: Double) {
            self.block = block
            self.score = score
        }
    }

    /// Find the block best matching `query` (e.g. "where is the Submit button").
    /// Matching is case-insensitive: an exact match scores highest, then a block
    /// that CONTAINS the query, then the query containing the block's text. Ties
    /// break toward the higher-confidence, then earlier (reading-order) block.
    /// Returns nil when nothing matches — never invents a location.
    public static func locate(_ query: String, in readout: ScreenReadout) -> Located? {
        let q = normalize(query)
        guard !q.isEmpty else { return nil }

        var best: Located?
        for block in readout.blocks {
            let t = normalize(block.text)
            guard !t.isEmpty else { continue }
            let score: Double
            if t == q {
                score = 1.0
            } else if t.contains(q) {
                // Block text contains the query: closer length -> higher score.
                score = 0.7 * (Double(q.count) / Double(t.count))
            } else if q.contains(t) {
                // Query contains the block text (e.g. "Submit button" ~ "Submit").
                score = 0.6 * (Double(t.count) / Double(q.count))
            } else {
                continue
            }
            if let current = best {
                if score > current.score
                    || (score == current.score && block.confidence > current.block.confidence) {
                    best = Located(block: block, score: score)
                }
            } else {
                best = Located(block: block, score: score)
            }
        }
        return best
    }

    // -- helpers --------------------------------------------------------------

    /// Lowercased, whitespace-trimmed form for case-insensitive matching.
    static func normalize(_ s: String) -> String {
        s.trimmingCharacters(in: .whitespacesAndNewlines).lowercased()
    }

    /// Vertical center of a box (Vision coords; higher = higher on screen).
    static func centerY(_ b: DetectionBox) -> Double { b.y + b.height / 2.0 }
}
