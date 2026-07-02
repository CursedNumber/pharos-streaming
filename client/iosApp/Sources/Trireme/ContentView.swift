import SwiftUI

/// The iOS launcher hosts the [`ServerSelectorView`] introduced in the
/// SwiftUI port of the desktop / Android selectors. This file used to carry
/// the "hello core" round-trip; that's preserved as `HelloCoreView` below
/// in case you want to keep it as a debug screen.
struct ContentView: View {
    var body: some View {
        ServerSelectorView()
    }
}

/// Debug-only "hello core" round-trip. Not on the navigation stack by
/// default; keep it for local sanity checks.
struct HelloCoreView: View {
    private let info = coreInfo()

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            Text("Trireme").font(.largeTitle)
            Text("core agent:       \(info.agent)")
            Text("core version:     \(info.coreVersion)")
            Text("protocol version: \(info.protocolVersion)")
            Text(greet(name: "Trireme"))
        }
        .padding()
        .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .topLeading)
    }
}

#Preview {
    ContentView()
}
