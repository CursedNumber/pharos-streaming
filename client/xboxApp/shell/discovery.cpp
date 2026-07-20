// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 The Pharos/Trireme authors
//
// See discovery.h.

#include "discovery.h"

#include <winrt/Windows.Devices.Enumeration.h>
#include <winrt/Windows.Foundation.Collections.h>
#include <winrt/Windows.Foundation.h>

#include <map>
#include <mutex>
#include <string>

using namespace winrt;
using namespace winrt::Windows::Devices::Enumeration;
using namespace winrt::Windows::Foundation;
using namespace winrt::Windows::Foundation::Collections;

void shellLog(const std::string& s);  // main.cpp

namespace {

// DNS-SD over DeviceWatcher: protocol GUID is fixed (documented AQS for
// AssociationEndpointService), service filter picks our type.
const wchar_t kAqs[] =
    L"System.Devices.AepService.ProtocolId:={4526e8c1-8aac-4153-9b16-55e86ada0e54}"
    L" AND System.Devices.Dnssd.ServiceName:=\"_pharos._udp\""
    L" AND System.Devices.Dnssd.Domain:=\"local\"";

std::string propString(IMapView<hstring, IInspectable> const& props, const wchar_t* key) {
    if (auto v = props.TryLookup(key)) {
        if (auto r = v.try_as<IReference<hstring>>()) return to_string(r.Value());
    }
    return {};
}

}  // namespace

struct HostDiscovery::Impl {
    DeviceWatcher watcher{nullptr};
    event_token addedTok, updatedTok, removedTok;
    mutable std::mutex mx;
    std::map<std::string, DiscoveredHost> found;  // by watcher device id

    void onSeen(DeviceInformation const& di) { parse(to_string(di.Id()), di.Properties()); }
    void onUpdated(DeviceInformationUpdate const& up) {
        // Updates carry only changed properties; merge over what we have.
        parse(to_string(up.Id()), up.Properties());
    }

    void parse(const std::string& id, IMapView<hstring, IInspectable> const& props) {
        std::lock_guard<std::mutex> lk(mx);
        DiscoveredHost& h = found[id];
        try {
            if (auto s = propString(props, L"System.Devices.Dnssd.InstanceName"); !s.empty())
                h.instance = s;
            if (auto v = props.TryLookup(L"System.Devices.Dnssd.PortNumber"))
                if (auto r = v.try_as<IReference<uint16_t>>()) h.pairPort = r.Value();
            if (auto v = props.TryLookup(L"System.Devices.IpAddress"))
                if (auto arr = v.try_as<IReferenceArray<hstring>>())
                    for (auto const& ip : arr.Value()) {
                        auto s = to_string(ip);
                        if (s.find('.') != std::string::npos && s.rfind("169.254", 0) != 0) {
                            h.addr = s;  // first routable IPv4
                            break;
                        }
                    }
            if (auto v = props.TryLookup(L"System.Devices.Dnssd.TextAttributes"))
                if (auto arr = v.try_as<IReferenceArray<hstring>>())
                    for (auto const& kv : arr.Value()) {
                        auto s = to_string(kv);
                        auto eq = s.find('=');
                        if (eq == std::string::npos) continue;
                        auto k = s.substr(0, eq), val = s.substr(eq + 1);
                        if (k == "pubkey") h.pubkey = val;
                        else if (k == "alias") h.alias = val;
                        else if (k == "stream") h.streamPort = atoi(val.c_str());
                    }
        } catch (...) {
        }
    }
};

void HostDiscovery::start() {
    if (p_) return;
    p_ = new Impl();
    try {
        p_->watcher = DeviceInformation::CreateWatcher(
            hstring{kAqs},
            {L"System.Devices.Dnssd.InstanceName", L"System.Devices.IpAddress",
             L"System.Devices.Dnssd.PortNumber", L"System.Devices.Dnssd.TextAttributes"},
            DeviceInformationKind::AssociationEndpointService);
        p_->addedTok = p_->watcher.Added([impl = p_](auto&&, DeviceInformation const& di) {
            impl->onSeen(di);
        });
        p_->updatedTok = p_->watcher.Updated([impl = p_](auto&&, DeviceInformationUpdate const& u) {
            impl->onUpdated(u);
        });
        p_->removedTok = p_->watcher.Removed([impl = p_](auto&&, DeviceInformationUpdate const& u) {
            std::lock_guard<std::mutex> lk(impl->mx);
            impl->found.erase(to_string(u.Id()));
        });
        p_->watcher.Start();
        shellLog("discovery: watching _pharos._udp");
    } catch (hresult_error const& e) {
        shellLog("discovery: start failed 0x" + std::to_string(static_cast<uint32_t>(e.code())));
    }
}

void HostDiscovery::stop() {
    if (!p_) return;
    try {
        if (p_->watcher) {
            p_->watcher.Added(p_->addedTok);
            p_->watcher.Updated(p_->updatedTok);
            p_->watcher.Removed(p_->removedTok);
            p_->watcher.Stop();
        }
    } catch (...) {
    }
    delete p_;
    p_ = nullptr;
}

std::vector<DiscoveredHost> HostDiscovery::snapshot() const {
    std::vector<DiscoveredHost> out;
    if (!p_) return out;
    std::lock_guard<std::mutex> lk(p_->mx);
    for (auto const& [id, h] : p_->found)
        if (!h.addr.empty()) out.push_back(h);
    return out;
}
