import SwiftUI

/// Minimal host application for the iOS File Provider extension.
///
/// On iOS a File Provider extension cannot be installed on its own; it must be
/// embedded in a containing app. This app exists only to carry the extension
/// bundle. It does not drive the engine itself — the extension runs the
/// in-process `CascadeNode` over the shared app-group container.
@main
struct CascadeHostApp: App {
    var body: some Scene {
        WindowGroup {
            ContentView()
        }
    }
}

struct ContentView: View {
    var body: some View {
        VStack(spacing: 16) {
            Text("Cascade")
                .font(.largeTitle)
                .bold()
            Text("The Cascade files appear in the Files app under the Cascade location once this app is installed.")
                .multilineTextAlignment(.center)
                .padding(.horizontal)
        }
        .padding()
    }
}
