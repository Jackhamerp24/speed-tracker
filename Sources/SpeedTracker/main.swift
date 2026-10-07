import AppKit

let arguments = CommandLine.arguments
let application = NSApplication.shared

if let index = arguments.firstIndex(of: "--snapshot"), arguments.count > index + 1 {
    // Renders the popover to a PNG with demo data and exits. Used to check the UI without clicking.
    application.setActivationPolicy(.prohibited)
    Snapshot.render(to: arguments[index + 1], arguments: arguments)
    exit(0)
}

if let index = arguments.firstIndex(of: "--diagnose") {
    // Prints what automatic detection can see on this Mac, then watches traffic for a while.
    let seconds = arguments.count > index + 1 ? Double(arguments[index + 1]) ?? 20 : 20
    Diagnose.run(seconds: seconds)
    exit(0)
}

let delegate = AppDelegate()
application.delegate = delegate
application.setActivationPolicy(.accessory)
application.run()
