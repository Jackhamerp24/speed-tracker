import SwiftUI

/// Each harness gets a colour, so its calls can be told apart at a glance.
enum HarnessStyle {
    private static let known: [String: Color] = [
        "Claude Code": Color(red: 0.91, green: 0.48, blue: 0.31),
        "Claude Desktop": Color(red: 0.91, green: 0.48, blue: 0.31),
        "Codex": Color(red: 0.20, green: 0.77, blue: 0.56),
        "OMP": Color(red: 0.67, green: 0.49, blue: 0.96),
        "Pi": Color(red: 0.41, green: 0.56, blue: 0.98),
        "Gemini CLI": Color(red: 0.29, green: 0.61, blue: 0.98),
        "Antigravity": Color(red: 0.93, green: 0.36, blue: 0.47),
        "DeepSeek CLI": Color(red: 0.36, green: 0.50, blue: 1.0),
        "opencode": Color(red: 0.56, green: 0.63, blue: 0.72),
    ]

    private static let fallback: [Color] = [.pink, .cyan, .mint, .yellow, .teal, .indigo, .brown]

    static func color(for harness: String) -> Color {
        if let color = known[harness] { return color }
        // Stable across launches, unlike `hashValue`.
        let sum = harness.unicodeScalars.reduce(0) { $0 &+ Int($1.value) }
        return fallback[sum % fallback.count]
    }
}

/// Semantic tokens shared by the popover. They intentionally sit on top of
/// system-adaptive foregrounds so the surface hierarchy survives both themes.
enum Theme {
    static let accent = Color(red: 0.20, green: 0.78, blue: 0.53)
    static let accentSoft = accent.opacity(0.16)
    static let panel = Color.primary.opacity(0.052)
    static let panelRaised = Color.primary.opacity(0.075)
    static let border = Color.primary.opacity(0.105)
    static let divider = Color.primary.opacity(0.075)
    static let muted = Color.secondary
}

enum Layout {
    static let width: CGFloat = 404
    static let padding: CGFloat = 16
    static let cardRadius: CGFloat = 14
}

struct Card<Content: View>: View {
    var padding: CGFloat = 12
    var background: Color = Theme.panel
    var borderColor: Color = Theme.border
    @ViewBuilder var content: Content

    var body: some View {
        content
            .padding(padding)
            .frame(maxWidth: .infinity, alignment: .leading)
            .background(
                RoundedRectangle(cornerRadius: Layout.cardRadius, style: .continuous)
                    .fill(background)
            )
            .overlay(
                RoundedRectangle(cornerRadius: Layout.cardRadius, style: .continuous)
                    .strokeBorder(borderColor, lineWidth: 1)
            )
    }
}

struct SectionLabel: View {
    let text: String

    init(_ text: String) { self.text = text }

    var body: some View {
        Text(text.uppercased())
            .font(.system(size: 10, weight: .bold, design: .rounded))
            .kerning(0.7)
            .foregroundStyle(Theme.muted)
    }
}

struct HarnessDot: View {
    let harness: String
    var size: CGFloat = 8

    var body: some View {
        Circle()
            .fill(HarnessStyle.color(for: harness))
            .frame(width: size, height: size)
            .accessibilityHidden(true)
    }
}

struct HarnessChip: View {
    let name: String

    var body: some View {
        HStack(spacing: 5) {
            HarnessDot(harness: name, size: 6)
            Text(name)
                .font(.system(size: 11, weight: .semibold))
                .lineLimit(1)
        }
        .padding(.horizontal, 8)
        .padding(.vertical, 3.5)
        .background(HarnessStyle.color(for: name).opacity(0.14), in: Capsule())
        .overlay(Capsule().strokeBorder(HarnessStyle.color(for: name).opacity(0.28), lineWidth: 1))
        .accessibilityElement(children: .combine)
    }
}

/// A large figure with its unit and a small caption above.
struct Metric: View {
    let label: String
    let value: String
    let unit: String
    var dimmed = false
    @Environment(\.accessibilityReduceMotion) private var reduceMotion

    var body: some View {
        VStack(alignment: .leading, spacing: 1) {
            Text(label)
                .font(.system(size: 10, weight: .bold, design: .rounded))
                .kerning(0.6)
                .foregroundStyle(Theme.muted)
            HStack(alignment: .firstTextBaseline, spacing: 2) {
                if value.hasPrefix("~") {
                    Text("~")
                        .font(.system(size: 20, weight: .medium, design: .rounded))
                        .foregroundStyle(Theme.muted)
                    Text(String(value.dropFirst()))
                        .font(.system(size: 30, weight: .semibold, design: .rounded))
                        .monospacedDigit()
                        .contentTransition(.numericText())
                        .foregroundStyle(dimmed ? .secondary : .primary)
                } else {
                    Text(value)
                        .font(.system(size: 30, weight: .semibold, design: .rounded))
                        .monospacedDigit()
                        .contentTransition(.numericText())
                        .foregroundStyle(dimmed ? .secondary : .primary)
                }
                if !unit.isEmpty {
                    Text(unit)
                        .font(.system(size: 11, weight: .semibold, design: .rounded))
                        .foregroundStyle(Theme.muted)
                        .padding(.leading, 1)
                }
            }
            .animation(reduceMotion ? nil : .snappy(duration: 0.25), value: value)
        }
        .frame(maxWidth: .infinity, alignment: .leading)
    }
}

/// A smooth line of recent speed, filled underneath, with a dot at the latest value.
struct Sparkline: View {
    let values: [Double]
    var color: Color = Theme.accent

    var body: some View {
        GeometryReader { proxy in
            let points = self.points(in: proxy.size)
            if points.count >= 2, let last = points.last {
                ZStack {
                    // Subtle baseline grid guide
                    Path { path in
                        path.move(to: CGPoint(x: 0, y: proxy.size.height - 2))
                        path.addLine(to: CGPoint(x: proxy.size.width, y: proxy.size.height - 2))
                    }
                    .stroke(
                        Color.primary.opacity(0.04),
                        style: StrokeStyle(lineWidth: 1, dash: [4, 4])
                    )

                    area(points, height: proxy.size.height)
                        .fill(LinearGradient(
                            colors: [color.opacity(0.28), color.opacity(0.0)],
                            startPoint: .top, endPoint: .bottom
                        ))
                    line(points)
                        .stroke(color, style: StrokeStyle(lineWidth: 2, lineCap: .round, lineJoin: .round))
                    // Precision probe indicator with outer pulse ring and white center
                    Circle()
                        .stroke(color.opacity(0.25), lineWidth: 5)
                        .frame(width: 10, height: 10)
                        .position(last)
                    Circle()
                        .fill(color)
                        .frame(width: 6, height: 6)
                        .position(last)
                    Circle()
                        .fill(Color.white.opacity(0.9))
                        .frame(width: 2, height: 2)
                        .position(last)
                }
            } else {
                ZStack(alignment: .bottom) {
                    Capsule()
                        .fill(Color.primary.opacity(0.06))
                        .frame(height: 2)
                }
                .frame(maxHeight: .infinity, alignment: .bottom)
            }
        }
        .accessibilityElement(children: .ignore)
        .accessibilityLabel("Speed trend")
        .accessibilityValue(values.isEmpty ? "No measurements yet" : "Live measurements available")
    }

    private func points(in size: CGSize) -> [CGPoint] {
        guard values.count >= 2, let peak = values.max(), peak > 0 else { return [] }
        // Room for the end dot on the right and above the highest point.
        let width = size.width - 6
        let step = width / CGFloat(values.count - 1)
        return values.enumerated().map { index, value in
            CGPoint(x: CGFloat(index) * step, y: size.height - 4 - CGFloat(value / (peak * 1.15)) * (size.height - 8))
        }
    }

    /// Catmull-Rom through the points, so the line bends instead of zig-zagging.
    private func line(_ points: [CGPoint]) -> Path {
        var path = Path()
        path.move(to: points[0])
        for index in 1..<points.count {
            let previous = points[max(index - 2, 0)]
            let start = points[index - 1]
            let end = points[index]
            let next = points[min(index + 1, points.count - 1)]
            let control1 = CGPoint(x: start.x + (end.x - previous.x) / 6, y: start.y + (end.y - previous.y) / 6)
            let control2 = CGPoint(x: end.x - (next.x - start.x) / 6, y: end.y - (next.y - start.y) / 6)
            path.addCurve(to: end, control1: control1, control2: control2)
        }
        return path
    }

    private func area(_ points: [CGPoint], height: CGFloat) -> Path {
        var path = line(points)
        path.addLine(to: CGPoint(x: points[points.count - 1].x, y: height))
        path.addLine(to: CGPoint(x: points[0].x, y: height))
        path.closeSubpath()
        return path
    }
}
