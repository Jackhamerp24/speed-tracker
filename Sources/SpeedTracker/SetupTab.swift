import ServiceManagement
import SpeedTrackerCore
import SwiftUI

enum Preferences {
    static let showTTFTInMenuBar = "showTTFTInMenuBar"
}

private struct Recipe: Identifiable {
    let title: String
    let note: String
    let snippet: String

    var id: String { title }
}

/// What the app has found, and a few options. Nothing here is needed for the app to work.
struct SettingsTab: View {
    @EnvironmentObject private var store: MetricsStore
    @AppStorage(Preferences.showTTFTInMenuBar) private var showTTFT = false
    // Spelled out because the `@State` macro plugin ships only with Xcode.
    private let launchAtLogin = State(initialValue: SMAppService.mainApp.status == .enabled)
    private let copied = State<String?>(initialValue: nil)

    var body: some View {
        let harnesses = store.harnesses
        VStack(alignment: .leading, spacing: 12) {
            SectionLabel("Harnesses found")
            Card(padding: 0) {
                if harnesses.isEmpty {
                    Text("No harness found yet. Start one and it appears here.")
                        .font(.system(size: 12))
                        .foregroundStyle(Theme.muted)
                        .padding(12)
                } else {
                    VStack(spacing: 0) {
                        ForEach(harnesses) { harness in
                            HarnessRow(harness: harness, isTarget: store.target == harness.name)
                            if harness.id != harnesses.last?.id {
                                Rectangle().fill(Theme.divider).frame(height: 1).padding(.leading, 30)
                            }
                        }
                    }
                }
            }
            Text("Harnesses are found automatically. Choose which one to watch at the top of the Live tab.")
                .font(.system(size: 11))
                .foregroundStyle(Theme.muted)
                .fixedSize(horizontal: false, vertical: true)

            SectionLabel("Options")
                .padding(.top, 2)
            Card(padding: 0) {
                VStack(spacing: 0) {
                    OptionRow(title: "Show TTFT in the menu bar", isOn: $showTTFT)
                    Rectangle().fill(Theme.divider).frame(height: 1).padding(.leading, 12)
                    OptionRow(title: "Launch at login", isOn: launchAtLogin.projectedValue)
                        .onChange(of: launchAtLogin.wrappedValue) { _, enabled in
                            do {
                                if enabled {
                                    try SMAppService.mainApp.register()
                                } else {
                                    try SMAppService.mainApp.unregister()
                                }
                            } catch {
                                launchAtLogin.wrappedValue = SMAppService.mainApp.status == .enabled
                            }
                        }
                }
            }

            DisclosureGroup("Exact timing through the local proxy") {
                VStack(alignment: .leading, spacing: 8) {
                    Text("Optional. Pointing a harness's base URL at the proxy measures the stream itself instead of estimating from logs and traffic. \(proxyLine)")
                        .font(.system(size: 11))
                        .foregroundStyle(Theme.muted)
                        .fixedSize(horizontal: false, vertical: true)
                    ForEach(recipes) { recipe in
                        RecipeRow(recipe: recipe, copied: copied.wrappedValue == recipe.id) {
                            NSPasteboard.general.clearContents()
                            NSPasteboard.general.setString(recipe.snippet, forType: .string)
                            copied.wrappedValue = recipe.id
                        }
                    }
                }
                .padding(.top, 6)
            }
            .font(.system(size: 12))
            .padding(12)
            .background(Theme.panel, in: RoundedRectangle(cornerRadius: Layout.cardRadius, style: .continuous))
            .overlay(RoundedRectangle(cornerRadius: Layout.cardRadius, style: .continuous).strokeBorder(Theme.border, lineWidth: 1))

            HStack {
                Button("Open Data Folder") {
                    NSWorkspace.shared.open(AppPaths.home)
                }
                Spacer()
                Button("Quit Speed Tracker") { NSApp.terminate(nil) }
                    .keyboardShortcut("q")
            }
            .controlSize(.small)
            .padding(.top, 2)
        }
    }

    private var base: String { "http://127.0.0.1:\(store.port)" }

    private var proxyLine: String {
        switch store.proxyState {
        case .running(let port): return "It is listening on port \(port)."
        case .stopped: return "It is starting."
        case .failed(let message): return "It is off: \(message)."
        }
    }

    private var recipes: [Recipe] {
        [
            Recipe(title: "Claude Code", note: "shell environment", snippet: "export ANTHROPIC_BASE_URL=\(base)/anthropic"),
            Recipe(
                title: "Codex",
                note: "~/.codex/config.toml",
                snippet: """
                model_provider = "speedtracker"

                [model_providers.speedtracker]
                name = "OpenAI via Speed Tracker"
                base_url = "\(base)/chatgpt"
                wire_api = "responses"
                requires_openai_auth = true
                """
            ),
            Recipe(title: "Any other", note: "base URL for any HTTPS provider", snippet: "\(base)/_/<provider-host>/<path>"),
        ]
    }
}

private struct HarnessRow: View {
    let harness: HarnessInfo
    let isTarget: Bool

    var body: some View {
        HStack(spacing: 10) {
            HarnessDot(harness: harness.name)
            VStack(alignment: .leading, spacing: 1) {
                Text(harness.name)
                    .font(.system(size: 12, weight: .medium))
                Text(status)
                    .font(.system(size: 10.5))
                    .foregroundStyle(harness.isGenerating ? Theme.accent : Theme.muted)
                    .lineLimit(1)
            }
            Spacer(minLength: 8)
            if isTarget {
                Text("Live target")
                    .font(.system(size: 10, weight: .medium))
                    .foregroundStyle(Theme.muted)
            }
        }
        .padding(.horizontal, 12)
        .padding(.vertical, 8)
        .help(harness.hasLogs
            ? "Model and token counts come from this harness's own session log."
            : "No readable session log, so token counts are estimated from its traffic.")
    }

    private var status: String {
        if !harness.isPresent { return "Not found on this Mac" }
        var parts: [String] = []
        if harness.isGenerating {
            parts.append("Generating now")
        } else {
            if harness.isOpen { parts.append("Open, idle") }
            parts.append(harness.lastCall.map { "last call \(Format.ago($0)) ago" } ?? "no calls yet")
        }
        if !harness.hasLogs { parts.append("estimated") }
        return parts.joined(separator: " · ")
    }
}

private struct OptionRow: View {
    let title: String
    let isOn: Binding<Bool>

    var body: some View {
        HStack {
            Text(title)
                .font(.system(size: 12))
            Spacer(minLength: 8)
            Toggle(title, isOn: isOn)
                .labelsHidden()
                .toggleStyle(.switch)
                .controlSize(.mini)
        }
        .padding(.horizontal, 12)
        .padding(.vertical, 8)
    }
}

private struct RecipeRow: View {
    let recipe: Recipe
    let copied: Bool
    let copy: () -> Void

    var body: some View {
        HStack(alignment: .top, spacing: 8) {
            VStack(alignment: .leading, spacing: 2) {
                Text("\(recipe.title), \(recipe.note)")
                    .font(.system(size: 11, weight: .medium))
                Text(recipe.snippet)
                    .font(.system(size: 10, design: .monospaced))
                    .foregroundStyle(Theme.muted)
                    .lineLimit(2)
                    .truncationMode(.tail)
                    .fixedSize(horizontal: false, vertical: true)
            }
            Spacer(minLength: 0)
            Button(copied ? "Copied" : "Copy", action: copy)
                .controlSize(.small)
        }
    }
}
