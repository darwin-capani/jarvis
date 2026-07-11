// ScreenReadoutTests.swift — PURE-logic tests for the actionable-region
// structuring over OCR `.text` blocks: reading order, control-label candidacy,
// the full readable text, and the "where is <query>" locator. NO camera, NO
// screen, NO TCC, NO socket, NO OCR — every test drives synthesized `.text`
// detections (string + normalized box + confidence) and asserts the pure
// transform deterministically. READ-ONLY: the structurer LOCATES/DESCRIBES, it
// never clicks/actuates — there is no actuation surface to test (by design).

import XCTest
import Foundation
@testable import vision

final class ScreenReadoutTests: XCTestCase {

    /// A `.text` detection at a normalized box (Vision coords, origin bottom-left).
    private func t(_ s: String, x: Double, y: Double, w: Double = 0.2, h: Double = 0.05,
                   conf: Double = 0.9) -> Detection {
        Detection(kind: .text, boundingBox: DetectionBox(x: x, y: y, width: w, height: h),
                  confidence: conf, label: s)
    }

    // --- (a) reading order ---------------------------------------------------

    func testReadingOrderTopToBottom() {
        // y is bottom-left origin, so HIGHER y is HIGHER on screen and reads first.
        let dets = [t("bottom", x: 0.1, y: 0.1), t("top", x: 0.1, y: 0.8), t("mid", x: 0.1, y: 0.45)]
        let r = ScreenStructurer.structure(dets)
        XCTAssertEqual(r.blocks.map(\.text), ["top", "mid", "bottom"])
        XCTAssertEqual(r.fullText, "top\nmid\nbottom")
    }

    func testReadingOrderLeftToRightWithinSameRow() {
        // Same row (y within band) -> left-to-right by x.
        let dets = [t("right", x: 0.7, y: 0.5), t("left", x: 0.1, y: 0.5), t("mid", x: 0.4, y: 0.51)]
        let r = ScreenStructurer.structure(dets)
        XCTAssertEqual(r.blocks.map(\.text), ["left", "mid", "right"],
                       "blocks in the same row read left-to-right")
    }

    func testReadingOrderRowsThenColumns() {
        // Two rows of two; top row reads first, each row left-to-right.
        let dets = [
            t("B2", x: 0.6, y: 0.2), t("A2", x: 0.1, y: 0.2),   // bottom row
            t("B1", x: 0.6, y: 0.8), t("A1", x: 0.1, y: 0.8),   // top row
        ]
        let r = ScreenStructurer.structure(dets)
        XCTAssertEqual(r.blocks.map(\.text), ["A1", "B1", "A2", "B2"])
    }

    func testReadingOrderIsStableForEqualKeys() {
        // Identical boxes keep input order (deterministic, no thrash).
        let dets = [t("first", x: 0.1, y: 0.5), t("second", x: 0.1, y: 0.5)]
        let r = ScreenStructurer.structure(dets)
        XCTAssertEqual(r.blocks.map(\.text), ["first", "second"])
    }

    func testReadingOrderIsTransitiveAcrossBandBoundary() {
        // Three labels whose vertical CENTERS chain across the band tolerance
        // (0.025, 0.040, 0.055 — each within 0.02 of its neighbour, but the ends
        // differ by 0.03). A pairwise |Δcenter| <= tolerance comparator is NOT a
        // strict weak ordering here (A<B, B<C, yet C<A) and can scramble the sort;
        // the quantized-band comparator is transitive, so the result is a complete,
        // deterministic permutation. B and C fall in the same (higher) band and read
        // left-to-right; A is the lower band and reads last.
        let dets = [t("A", x: 0.1, y: 0.0), t("B", x: 0.5, y: 0.015), t("C", x: 0.9, y: 0.030)]
        let r = ScreenStructurer.structure(dets)
        XCTAssertEqual(r.blocks.count, 3, "no label is dropped by an inconsistent comparator")
        XCTAssertEqual(Set(r.blocks.map(\.text)), ["A", "B", "C"])
        XCTAssertEqual(r.blocks.map(\.text), ["B", "C", "A"])
    }

    func testNonTextDetectionsAreIgnored() {
        let dets: [Detection] = [
            t("Hello", x: 0.1, y: 0.5),
            Detection(kind: .human, boundingBox: .full, confidence: 0.9, label: ""),
            Detection(kind: .object, boundingBox: .full, confidence: 0.8, label: "keyboard"),
        ]
        let r = ScreenStructurer.structure(dets)
        XCTAssertEqual(r.blocks.map(\.text), ["Hello"],
                       "only .text detections are structured; human/object ignored")
    }

    func testEmptyLabelsDropped() {
        let dets = [t("", x: 0.1, y: 0.5), t("Real", x: 0.1, y: 0.4)]
        let r = ScreenStructurer.structure(dets)
        XCTAssertEqual(r.blocks.map(\.text), ["Real"])
    }

    func testEmptyInputYieldsEmptyReadout() {
        let r = ScreenStructurer.structure([])
        XCTAssertTrue(r.blocks.isEmpty)
        XCTAssertEqual(r.fullText, "")
        XCTAssertTrue(r.controls.isEmpty)
    }

    // --- (b) control-label candidacy ----------------------------------------

    func testShortLabelsAreControlCandidates() {
        for label in ["Submit", "OK", "Sign In", "Add to Cart", "Cancel"] {
            XCTAssertTrue(ScreenStructurer.isControlLabel(label), "'\(label)' should be a control")
        }
    }

    func testLongParagraphsAreNotControls() {
        let paragraph = "This is a long sentence of body text that is clearly not a button label at all."
        XCTAssertFalse(ScreenStructurer.isControlLabel(paragraph))
        // Too many words even if short-ish.
        XCTAssertFalse(ScreenStructurer.isControlLabel("one two three four five"))
        // Empty / whitespace -> not a control.
        XCTAssertFalse(ScreenStructurer.isControlLabel("   "))
    }

    func testControlsSubsetFiltersBlocks() {
        let dets = [
            t("Submit", x: 0.1, y: 0.9),
            t("This is a paragraph of body copy that runs long and is not a control.", x: 0.1, y: 0.5),
            t("Cancel", x: 0.1, y: 0.1),
        ]
        let r = ScreenStructurer.structure(dets)
        XCTAssertEqual(r.blocks.count, 3, "all three are recognized blocks")
        XCTAssertEqual(r.controls.map(\.text), ["Submit", "Cancel"],
                       "only the short button-ish labels are control candidates")
        XCTAssertTrue(r.controls.allSatisfy { $0.isControlCandidate })
    }

    func testBlockCenterIsComputed() {
        let det = t("X", x: 0.2, y: 0.4, w: 0.4, h: 0.2)
        let r = ScreenStructurer.structure([det])
        let c = r.blocks[0].center
        XCTAssertEqual(c.x, 0.4, accuracy: 1e-12, "center x = x + w/2")
        XCTAssertEqual(c.y, 0.5, accuracy: 1e-12, "center y = y + h/2")
    }

    // --- (c) where-is locator ------------------------------------------------

    func testLocateExactMatchCaseInsensitive() {
        let r = ScreenStructurer.structure([
            t("Submit", x: 0.4, y: 0.2), t("Cancel", x: 0.1, y: 0.2)])
        let loc = ScreenStructurer.locate("SUBMIT", in: r)
        XCTAssertEqual(loc?.block.text, "Submit")
        XCTAssertEqual(loc?.score, 1.0)
    }

    func testLocateQueryContainsBlockText() {
        // "where is the Submit button" -> matches the "Submit" block.
        let r = ScreenStructurer.structure([t("Submit", x: 0.4, y: 0.2), t("Reset", x: 0.1, y: 0.2)])
        let loc = ScreenStructurer.locate("Submit button", in: r)
        XCTAssertEqual(loc?.block.text, "Submit")
        XCTAssertNotNil(loc)
    }

    func testLocateBlockTextContainsQuery() {
        // Query is a substring of a longer block ("File" within "File Menu").
        let r = ScreenStructurer.structure([t("File Menu", x: 0.1, y: 0.9)])
        let loc = ScreenStructurer.locate("File", in: r)
        XCTAssertEqual(loc?.block.text, "File Menu")
    }

    func testLocatePrefersExactOverPartial() {
        let r = ScreenStructurer.structure([
            t("Save As", x: 0.1, y: 0.5),   // partial (contains "Save")
            t("Save", x: 0.5, y: 0.5),      // exact
        ])
        let loc = ScreenStructurer.locate("Save", in: r)
        XCTAssertEqual(loc?.block.text, "Save", "an exact match outranks a containing match")
        XCTAssertEqual(loc?.score, 1.0)
    }

    func testLocateNoMatchReturnsNil() {
        let r = ScreenStructurer.structure([t("Submit", x: 0.1, y: 0.5)])
        XCTAssertNil(ScreenStructurer.locate("delete", in: r), "no match -> nil, never invent a location")
        XCTAssertNil(ScreenStructurer.locate("", in: r), "empty query -> nil")
    }

    func testLocateReturnsCenterForPointing() {
        // The located block exposes a center (a 'where' for describing/pointing,
        // NOT a click target) — proving the locator yields a usable location.
        let r = ScreenStructurer.structure([t("Login", x: 0.3, y: 0.6, w: 0.2, h: 0.1)])
        let loc = ScreenStructurer.locate("login", in: r)
        let c = try? XCTUnwrap(loc).block.center
        XCTAssertEqual(c?.x ?? -1, 0.4, accuracy: 1e-12)
        XCTAssertEqual(c?.y ?? -1, 0.65, accuracy: 1e-12)
    }
}
