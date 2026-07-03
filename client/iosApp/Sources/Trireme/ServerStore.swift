// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 The Pharos/Trireme authors

//
// UserDefaults-backed persistence for the registry on iOS. Stores a single
// JSON-encoded Registry under "trireme.servers"; identity.toml lives in the
// app's Documents directory so the shared core-crypto can load_or_create it
// via the path passed across FFI.

import Foundation

public final class ServerStore {
    private let defaults: UserDefaults
    private let key = "trireme.servers"

    public init(defaults: UserDefaults = .standard) {
        self.defaults = defaults
    }

    public func load() -> Registry {
        guard let data = defaults.data(forKey: key),
              let reg = try? JSONDecoder().decode(Registry.self, from: data)
        else { return Registry() }
        return reg
    }

    public func save(_ registry: Registry) {
        guard let data = try? JSONEncoder().encode(registry) else { return }
        defaults.set(data, forKey: key)
    }

    public func upsert(_ entry: ServerEntry) -> Registry {
        let next = load().upsert(entry)
        save(next)
        return next
    }

    public func remove(_ id: String) -> Registry {
        let next = load().remove(id)
        save(next)
        return next
    }

    /// Path the shared core-crypto uses for `Identity::load_or_create`.
    public func identityPath() -> String {
        let docs = try? FileManager.default.url(
            for: .documentDirectory,
            in: .userDomainMask,
            appropriateFor: nil,
            create: true
        )
        let url = (docs ?? URL(fileURLWithPath: NSTemporaryDirectory()))
            .appendingPathComponent("identity.toml")
        return url.path
    }
}
