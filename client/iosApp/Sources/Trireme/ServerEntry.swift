// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 The Pharos authors

//
// Mirror of the desktop ServerEntry schema (crates/trireme/src/servers.rs)
// and the Android one (ServerEntry.kt). Field names match the JSON keys
// used by the Android shell so a future iCloud-sync layer can round-trip
// both platforms without translation.

import Foundation

public struct ServerEntry: Codable, Identifiable, Equatable {
    public var id: String
    public var host: String
    public var port: Int
    public var alias: String?
    public var hostnameCache: String?
    public var pace: String?
    public var paired: Bool
    public var hostPubkey: String?
    public var pairPort: Int?
    public var lastConnected: String?

    private enum CodingKeys: String, CodingKey {
        case id, host, port, alias
        case hostnameCache = "hostname_cache"
        case pace, paired
        case hostPubkey = "host_pubkey"
        case pairPort = "pair_port"
        case lastConnected = "last_connected"
    }

    public init(
        id: String,
        host: String,
        port: Int,
        alias: String? = nil,
        hostnameCache: String? = nil,
        pace: String? = nil,
        paired: Bool = false,
        hostPubkey: String? = nil,
        pairPort: Int? = nil,
        lastConnected: String? = nil
    ) {
        self.id = id
        self.host = host
        self.port = port
        self.alias = alias
        self.hostnameCache = hostnameCache
        self.pace = pace
        self.paired = paired
        self.hostPubkey = hostPubkey
        self.pairPort = pairPort
        self.lastConnected = lastConnected
    }

    /// alias > hostnameCache > host (same precedence as desktop + Android).
    public func friendlyName() -> String {
        if let alias, !alias.trimmingCharacters(in: .whitespaces).isEmpty {
            return alias
        }
        if let cache = hostnameCache, !cache.isEmpty, cache != host {
            return cache
        }
        return host
    }

    public func addressLabel() -> String { "\(host):\(port)" }
    public func pairPortOrDefault() -> Int { pairPort ?? 4432 }

    public static func makeId(host: String, port: Int) -> String {
        "\(host)_\(port)".filter { c in
            c.isLetter || c.isNumber || c == "." || c == "_" || c == "-"
        }
    }
}

public struct Registry: Codable, Equatable {
    public var servers: [ServerEntry]

    public init(servers: [ServerEntry] = []) {
        self.servers = servers
    }

    public func find(_ id: String) -> ServerEntry? { servers.first { $0.id == id } }

    public func upsert(_ e: ServerEntry) -> Registry {
        Registry(servers: servers.filter { $0.id != e.id } + [e])
    }

    public func remove(_ id: String) -> Registry {
        Registry(servers: servers.filter { $0.id != id })
    }
}
