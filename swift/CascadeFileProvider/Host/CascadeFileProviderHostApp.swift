// SPDX-License-Identifier: Apache-2.0
//
// Cascade File Provider host app.
//
// macOS will not load a `.appex` File Provider extension unless the
// extension is embedded in a containing `.app` and that app has
// registered an `NSFileProviderDomain` at runtime via
// `NSFileProviderManager.add(_:completionHandler:)`. This SwiftUI app
// is the smallest plausible container — its single purpose is to be
// installed, run once, and call the registration API.

import SwiftUI

@main
struct CascadeFileProviderHostApp: App {
    var body: some Scene {
        WindowGroup("Cascade File Provider") {
            ContentView()
                .frame(minWidth: 360, minHeight: 240)
        }
    }
}
