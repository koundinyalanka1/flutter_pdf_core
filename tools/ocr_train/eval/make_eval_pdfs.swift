// Born-digital evaluation PDFs in fonts the recognizer never trained on.
//
//     swift make_eval_pdfs.swift <out-dir> <text.md>...
//
// One PDF per font, laid out by Core Text exactly as macOS apps write PDFs
// (embedded font subsets): a single-column page at 11 pt with a bold title,
// a two-column page at 10 pt, and a page of 8 pt small print. Score them with
// `pdf_cli ocr-eval`, which renders each page, recognizes it and compares the
// result with the text the PDF itself carries.

import CoreGraphics
import CoreText
import Foundation

let arguments = CommandLine.arguments
guard arguments.count >= 3 else {
    print("usage: make_eval_pdfs <out-dir> <text.md>...")
    exit(2)
}
let outDir = URL(fileURLWithPath: arguments[1], isDirectory: true)
try FileManager.default.createDirectory(at: outDir, withIntermediateDirectories: true)

// Prose from Markdown: drop code blocks, tables and markup characters.
var paragraphs: [String] = []
for path in arguments[2...] {
    let markdown = try String(contentsOfFile: path, encoding: .utf8)
    var inCode = false
    var current: [String] = []
    func flush() {
        let paragraph = current.joined(separator: " ")
            .replacingOccurrences(of: "`", with: "")
            .replacingOccurrences(of: "**", with: "")
            .trimmingCharacters(in: .whitespaces)
        if paragraph.count > 60 { paragraphs.append(paragraph) }
        current = []
    }
    for line in markdown.components(separatedBy: "\n") {
        if line.hasPrefix("```") { inCode.toggle(); flush(); continue }
        let trimmed = line.trimmingCharacters(in: .whitespaces)
        if inCode || trimmed.hasPrefix("|") || trimmed.hasPrefix("#") || trimmed.isEmpty {
            flush()
            continue
        }
        current.append(trimmed.hasPrefix("* ") ? String(trimmed.dropFirst(2)) : trimmed)
    }
    flush()
}

let fonts = [
    "TimesNewRomanPSMT", "ArialMT", "Georgia", "Helvetica", "Verdana", "Palatino-Roman",
    "Baskerville", "CourierNewPSMT", "Futura-Medium", "GillSans", "Optima-Regular",
    "Avenir-Book", "Menlo-Regular", "Tahoma", "TrebuchetMS", "Didot", "Cochin",
    "HoeflerText-Regular", "AmericanTypewriter", "Rockwell-Regular", "HelveticaNeue",
    "Charter-Roman", "BigCaslon-Medium", "Superclarendon-Regular",
]

var generator = SystemRandomNumberGenerator()
func text(_ count: Int, from start: Int) -> String {
    (0..<count).map { paragraphs[(start + $0) % paragraphs.count] }.joined(separator: "\n\n")
}

func draw(_ string: NSAttributedString, in rect: CGRect, context: CGContext) {
    let framesetter = CTFramesetterCreateWithAttributedString(string)
    let frame = CTFramesetterCreateFrame(framesetter, CFRange(location: 0, length: 0), CGPath(rect: rect, transform: nil), nil)
    CTFrameDraw(frame, context)
}

func attributed(_ string: String, font: CTFont, leading: CGFloat) -> NSAttributedString {
    var spacing = leading
    let style = withUnsafeBytes(of: &spacing) { raw -> CTParagraphStyle in
        var setting = CTParagraphStyleSetting(spec: .paragraphSpacing, valueSize: MemoryLayout<CGFloat>.size, value: raw.baseAddress!)
        return CTParagraphStyleCreate(&setting, 1)
    }
    return NSAttributedString(string: string, attributes: [
        NSAttributedString.Key(kCTFontAttributeName as String): font,
        NSAttributedString.Key(kCTParagraphStyleAttributeName as String): style,
    ])
}

var written = 0
for (index, name) in fonts.enumerated() {
    let body = CTFontCreateWithName(name as CFString, 11, nil)
    guard (CTFontCopyPostScriptName(body) as String) == name else {
        print("skip \(name): not installed")
        continue
    }
    let bold = CTFontCreateCopyWithSymbolicTraits(body, 18, nil, .traitBold, .traitBold) ?? CTFontCreateWithName(name as CFString, 18, nil)
    var media = CGRect(x: 0, y: 0, width: 612, height: 792)
    let url = outDir.appendingPathComponent("\(name).pdf")
    guard let context = CGContext(url as CFURL, mediaBox: &media, nil) else { continue }
    let start = index * 7 % max(paragraphs.count, 1)

    context.beginPDFPage(nil)
    let titled = NSMutableAttributedString(attributedString: attributed("Evaluation page in \(name)\n", font: bold, leading: 10))
    titled.append(attributed(text(6, from: start), font: body, leading: 8))
    draw(titled, in: CGRect(x: 72, y: 72, width: 468, height: 648), context: context)
    context.endPDFPage()

    context.beginPDFPage(nil)
    let columns = CTFontCreateWithName(name as CFString, 10, nil)
    draw(attributed(text(4, from: start + 6), font: columns, leading: 6), in: CGRect(x: 54, y: 54, width: 240, height: 684), context: context)
    draw(attributed(text(4, from: start + 10), font: columns, leading: 6), in: CGRect(x: 318, y: 54, width: 240, height: 684), context: context)
    context.endPDFPage()

    context.beginPDFPage(nil)
    let small = CTFontCreateWithName(name as CFString, 8, nil)
    draw(attributed(text(8, from: start + 14), font: small, leading: 4), in: CGRect(x: 72, y: 72, width: 468, height: 648), context: context)
    context.endPDFPage()

    context.closePDF()
    written += 1
}
print("wrote \(written) PDFs from \(paragraphs.count) paragraphs to \(outDir.path)")
