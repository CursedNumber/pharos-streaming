// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 The Pharos/Trireme authors
//
// LAN discovery of Pharos hosts via DNS-SD (the host advertises
// `_pharos._udp` while its pairing window is open, with TXT records carrying
// pubkey + stream port + alias). Wraps a Windows.Devices.Enumeration
// DeviceWatcher; results accumulate and are read with snapshot() from the UI
// thread each frame.

#pragma once

#include <string>
#include <vector>

struct DiscoveredHost {
    std::string instance;    // advertised instance name
    std::string alias;       // TXT alias (pretty hostname); may be empty
    std::string addr;        // best IPv4 address
    int pairPort = 4432;
    int streamPort = 15777;  // TXT `stream`, defaulted for older hosts
    std::string pubkey;      // TXT pubkey (64 hex chars)
};

class HostDiscovery {
public:
    void start();
    void stop();
    std::vector<DiscoveredHost> snapshot() const;

private:
    struct Impl;
    Impl* p_ = nullptr;
};
