// SPDX-License-Identifier: Apache-2.0

import FileProvider
import SwiftUI

/// Bundle identifier of the embedded File Provider extension.
///
/// Must match the `.appex` target's `PRODUCT_BUNDLE_IDENTIFIER`.
private let extensionBundleIdentifier = "io.cascade.CascadeFileProviderHost.FileProvider"

/// Stable domain identifier under which Cascade exposes its virtual
/// tree to the system. Changing this value on a machine that already
/// has a registered domain leaves the old domain orphaned, so it
/// should not vary across releases without a migration story.
private let domainIdentifier = NSFileProviderDomainIdentifier("io.cascade.fileprovider.default")

struct ContentView: View {
    @State private var status: String = "Domain not registered."
    @State private var working = false

    var body: some View {
        VStack(alignment: .leading, spacing: 16) {
            Text("Cascade File Provider")
                .font(.title2)
                .bold()

            Text(
                "Register the Cascade domain with macOS so the File Provider extension can present the virtual tree under Locations in Finder."
            )
            .foregroundStyle(.secondary)

            HStack {
                Button(action: register) {
                    Label("Register File Provider", systemImage: "externaldrive.badge.icloud")
                }
                .disabled(working)

                Button(action: unregister) {
                    Label("Remove", systemImage: "trash")
                }
                .disabled(working)
            }

            Text(status)
                .font(.callout)
                .foregroundStyle(.secondary)
                .frame(maxWidth: .infinity, alignment: .leading)

            Spacer()
        }
        .padding(24)
    }

    private func register() {
        working = true
        status = "Registering…"

        let domain = NSFileProviderDomain(
            identifier: domainIdentifier,
            displayName: "Cascade"
        )

        NSFileProviderManager.add(domain) { error in
            DispatchQueue.main.async {
                working = false
                if let error {
                    status = "Failed to register: \(error.localizedDescription)"
                } else {
                    status = "Cascade domain registered. Look for it under Locations in Finder."
                }
            }
        }
    }

    private func unregister() {
        working = true
        status = "Removing…"

        NSFileProviderManager.getDomainsWithCompletionHandler { domains, error in
            if let error {
                DispatchQueue.main.async {
                    working = false
                    status = "Failed to enumerate domains: \(error.localizedDescription)"
                }
                return
            }

            guard let target = domains.first(where: { $0.identifier == domainIdentifier }) else {
                DispatchQueue.main.async {
                    working = false
                    status = "Domain was not registered."
                }
                return
            }

            NSFileProviderManager.remove(target) { error in
                DispatchQueue.main.async {
                    working = false
                    if let error {
                        status = "Failed to remove: \(error.localizedDescription)"
                    } else {
                        status = "Cascade domain removed."
                    }
                }
            }
        }
    }
}

#Preview {
    ContentView()
}
