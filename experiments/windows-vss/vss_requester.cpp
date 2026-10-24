// SPDX-License-Identifier: MIT
// Copyright (c) 2026 M4n5ter
// Original experimental requester. Not a Sprig checkpoint implementation.
// Build only by default. Enabling runtime requires a separately approved fixture.
// MSVC: cl /nologo /std:c++17 /EHsc /W4 vss_requester.cpp /link VssApi.lib Ole32.lib Advapi32.lib
// Runtime build additionally requires /DSPRIG_VSS_ENABLE_RUNTIME=1.
// Windows 10 / Windows Server 2016 or newer; diff-area management may require Server.
// This is STORAGE consistency only: no application writers, transactions or
// Sprig mutation barrier participate. It does not make an application-consistent
// backup. The trusted child is NOT sandboxed; it must read only the approved
// fixture and must not delegate work through services/WMI or other job escapes.
// No formatting, mounting, privilege enablement, registry changes, service
// changes, fallback providers, persistent snapshots, or blanket deletes exist.

#ifndef _WIN32_WINNT
#define _WIN32_WINNT 0x0A00
#endif
#define NOMINMAX
#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#include <objbase.h>
#include <vss.h>
#include <vswriter.h>
#include <vsbackup.h>
#include <vsmgmt.h>
#include <algorithm>
#include <cstdint>
#include <cwchar>
#include <iomanip>
#include <iostream>
#include <set>
#include <sstream>
#include <stdexcept>
#include <string>
#include <vector>

#pragma comment(lib, "VssApi.lib")
#pragma comment(lib, "Ole32.lib")
#pragma comment(lib, "Advapi32.lib")

#ifndef SPRIG_VSS_ENABLE_RUNTIME
#define SPRIG_VSS_ENABLE_RUNTIME 0
#endif

namespace {
constexpr LONGLONG kDiffBytes = 536870912;
constexpr DWORD kDrainMs = 30000;
// Compile-time parser boundary checks. The actual GUID parser remains strict.
constexpr wchar_t kExampleVolume[] = L"\\\\?\\Volume{12345678-1234-1234-1234-123456789abc}\\";
constexpr wchar_t kVolumePrefix[] = L"\\\\?\\Volume{";
static_assert(sizeof(kExampleVolume) / sizeof(wchar_t) == 50, "49 root characters plus NUL");
static_assert(sizeof(kVolumePrefix) / sizeof(wchar_t) == 12, "11 prefix characters plus NUL");
static_assert(kExampleVolume[10] == L'{' && kExampleVolume[47] == L'}' &&
              kExampleVolume[48] == L'\\', "Volume GUID substring boundaries");
const VSS_ID kProvider = {0xb5946137, 0x7b9f, 0x4925,
                         {0xaf, 0x80, 0x51, 0xab, 0xd6, 0x0b, 0x20, 0xd5}};

template<class T> class ComPtr {
    T* p_ = nullptr;
public:
    ComPtr() = default;
    ComPtr(const ComPtr&) = delete;
    ComPtr& operator=(const ComPtr&) = delete;
    ~ComPtr() { reset(); }
    void reset() noexcept { if (p_) { p_->Release(); p_ = nullptr; } }
    T** put() { reset(); return &p_; }
    T* get() const { return p_; }
    T* operator->() const { return p_; }
    explicit operator bool() const { return p_ != nullptr; }
};

class Handle {
    HANDLE h_ = nullptr;
public:
    explicit Handle(HANDLE h = nullptr) : h_(h) {}
    Handle(const Handle&) = delete;
    Handle& operator=(const Handle&) = delete;
    ~Handle() { reset(); }
    void reset(HANDLE h = nullptr) noexcept {
        if (h_ && h_ != INVALID_HANDLE_VALUE) CloseHandle(h_);
        h_ = h;
    }
    HANDLE get() const { return h_; }
    explicit operator bool() const { return h_ && h_ != INVALID_HANDLE_VALUE; }
};

std::string hexHr(HRESULT hr) {
    std::ostringstream out;
    out << "0x" << std::hex << std::setw(8) << std::setfill('0')
        << static_cast<std::uint32_t>(hr);
    return out.str();
}
[[noreturn]] void fail(const std::string& message) { throw std::runtime_error(message); }
void require(bool condition, const char* message) { if (!condition) fail(message); }
void ok(HRESULT hr, const char* action) {
    if (hr != S_OK) fail(std::string(action) + ": " + hexHr(hr));
}
void win(bool success, const char* action) {
    if (!success) fail(std::string(action) + ": " + hexHr(HRESULT_FROM_WIN32(GetLastError())));
}
bool same(const std::wstring& a, const std::wstring& b) { return _wcsicmp(a.c_str(), b.c_str()) == 0; }
std::wstring guidText(const GUID& id) {
    wchar_t text[40]{};
    require(StringFromGUID2(id, text, 40) != 0, "StringFromGUID2 failed");
    return text;
}
bool nonzero(const GUID& id) { return !IsEqualGUID(id, GUID_NULL); }
std::wstring nullableGuid(const GUID& id) {
    return nonzero(id) ? L"\"" + guidText(id) + L"\"" : L"null";
}
std::wstring jsonString(const std::wstring& value) {
    std::wstring out = L"\"";
    for (wchar_t c : value) {
        if (c == L'\\' || c == L'\"') out.push_back(L'\\');
        require(c >= 32, "Control character in JSON identity");
        out.push_back(c);
    }
    return out + L"\"";
}

struct Report {
    std::wstring volume;
    VSS_ID setId = GUID_NULL;
    VSS_ID snapshotId = GUID_NULL;
    bool snapshotCreated = false;
    bool childStarted = false;
    bool childDrained = true;
    bool childExitKnown = false;
    DWORD childExit = 0;
    std::wstring snapshotCleanup = L"not_needed";
    std::wstring diffCleanup = L"not_needed";
    LONGLONG diffUsed = -1, diffAllocated = -1, diffMaximum = -1;
    double snapshotSeconds = 0, childSeconds = 0, cleanupSeconds = 0;
    bool cleanupSafe() const {
        return childDrained && snapshotCleanup != L"unverified" && diffCleanup != L"unverified";
    }
    void print(bool success) const {
        std::wcout << L"SPRIG_VSS_RESULT {\"version\":1,\"ok\":" << (success ? L"true" : L"false")
            << L",\"volume\":" << jsonString(volume)
            << L",\"provider_id\":" << jsonString(guidText(kProvider))
            << L",\"snapshot_set_id\":" << nullableGuid(setId)
            << L",\"snapshot_id\":" << nullableGuid(snapshotId)
            << L",\"snapshot_created\":" << (snapshotCreated ? L"true" : L"false")
            << L",\"child_started\":" << (childStarted ? L"true" : L"false")
            << L",\"child_drained\":" << (childDrained ? L"true" : L"false")
            << L",\"child_exit_code\":" << (childExitKnown ? std::to_wstring(childExit) : L"null")
            << L",\"snapshot_cleanup\":\"" << snapshotCleanup
            << L"\",\"diff_area_cleanup\":\"" << diffCleanup << L"\""
            << L",\"cleanup_safe\":" << (cleanupSafe() ? L"true" : L"false")
            << L",\"diff_used_bytes\":" << (diffUsed < 0 ? L"null" : std::to_wstring(diffUsed))
            << L",\"diff_allocated_bytes\":" << (diffAllocated < 0 ? L"null" : std::to_wstring(diffAllocated))
            << L",\"diff_maximum_bytes\":" << (diffMaximum < 0 ? L"null" : std::to_wstring(diffMaximum))
            << L",\"snapshot_create_seconds\":" << snapshotSeconds
            << L",\"child_seconds\":" << childSeconds
            << L",\"cleanup_seconds\":" << cleanupSeconds << L"}" << std::endl;
    }
};

struct Options {
    std::wstring volume, token, exe;
    DWORD serial = 0, vssMs = 120000, childMs = 300000;
    bool serialSet = false, storageAck = false, diffAllowed = false;
    std::vector<std::wstring> args;
};

unsigned hexDigit(wchar_t c) {
    if (c >= L'0' && c <= L'9') return static_cast<unsigned>(c - L'0');
    if (c >= L'a' && c <= L'f') return static_cast<unsigned>(c - L'a' + 10);
    if (c >= L'A' && c <= L'F') return static_cast<unsigned>(c - L'A' + 10);
    fail("Expected hexadecimal characters");
}
DWORD seconds(const std::wstring& text) {
    require(!text.empty() && text.size() <= 4, "Timeout must be 1..3600 seconds");
    DWORD n = 0;
    for (auto c : text) {
        require(c >= L'0' && c <= L'9', "Timeout must contain decimal digits only");
        n = n * 10 + static_cast<DWORD>(c - L'0');
    }
    require(n >= 1 && n <= 3600, "Timeout must be 1..3600 seconds");
    return n * 1000;
}
Options parse(int argc, wchar_t** argv) {
    Options o;
    std::set<std::wstring> seen;
    int i = 1;
    for (; i < argc; ++i) {
        std::wstring key = argv[i];
        if (key == L"--") { ++i; break; }
        require(seen.insert(key).second, "Duplicate option");
        if (key == L"--acknowledge-storage-snapshot-only") { o.storageAck = true; continue; }
        if (key == L"--allow-own-volume-diff-512m") { o.diffAllowed = true; continue; }
        require(i + 1 < argc, "Missing option value");
        const std::wstring value = argv[++i];
        if (key == L"--approved-synthetic-volume") o.volume = value;
        else if (key == L"--marker-token") o.token = value;
        else if (key == L"--child-exe") o.exe = value;
        else if (key == L"--vss-timeout-seconds") o.vssMs = seconds(value);
        else if (key == L"--child-timeout-seconds") o.childMs = seconds(value);
        else if (key == L"--expected-serial") {
            require(value.size() == 8, "Volume serial must be exactly 8 hex digits");
            for (auto c : value) o.serial = (o.serial << 4) | hexDigit(c);
            o.serialSet = true;
        } else fail("Unknown option");
    }
    require(o.storageAck && o.diffAllowed, "Both explicit storage-only and same-volume diff-area acknowledgments are required");
    require(o.serialSet, "--expected-serial is required");
    require(o.volume.size() == 49 && o.volume.substr(0, 11) == L"\\\\?\\Volume{" &&
            o.volume.substr(47) == L"}\\", "Input must be a canonical volume GUID root, not a drive or directory");
    GUID parsed{};
    ok(CLSIDFromString(o.volume.substr(10, 38).c_str(), &parsed), "Parse volume GUID");
    require(nonzero(parsed), "Null volume GUID rejected");
    require(o.token.size() == 32, "Marker token must be exactly 32 hex digits");
    for (auto c : o.token) (void)hexDigit(c);
    require(o.exe.size() > 7 && ((o.exe[0] >= L'A' && o.exe[0] <= L'Z') ||
            (o.exe[0] >= L'a' && o.exe[0] <= L'z')) && o.exe[1] == L':' && o.exe[2] == L'\\',
            "Child executable must be an absolute local drive path");
    require(same(o.exe.substr(o.exe.size() - 4), L".exe"), "Child must be an executable, not a script or shell command");
    unsigned substitutions = 0;
    for (; i < argc; ++i) {
        std::wstring arg = argv[i];
        require(arg.size() <= 8192 && o.args.size() < 64, "Child argument bound exceeded");
        if (arg == L"{snapshot-root}") ++substitutions;
        else require(arg.find(L"{snapshot-root}") == std::wstring::npos,
                     "Snapshot substitution must occupy one entire argument");
        o.args.push_back(arg);
    }
    require(substitutions == 1, "Exactly one child argument must be {snapshot-root}");
    return o;
}

// Guards are evidence of fixture identity, not a way to infer human approval.
void preflight(const Options& o) {
    wchar_t resolved[64]{};
    win(GetVolumeNameForVolumeMountPointW(o.volume.c_str(), resolved, 64) != FALSE, "Resolve volume GUID");
    require(same(o.volume, resolved), "Volume GUID identity changed");
    require(GetDriveTypeW(o.volume.c_str()) == DRIVE_FIXED, "Only a fixed synthetic NTFS volume is supported");
    wchar_t fs[64]{}, label[MAX_PATH]{};
    DWORD serial = 0, componentLength = 0, flags = 0;
    win(GetVolumeInformationW(o.volume.c_str(), label, MAX_PATH, &serial, &componentLength,
                             &flags, fs, 64) != FALSE, "Read volume information");
    require(same(fs, L"NTFS") && serial == o.serial, "NTFS or expected volume serial mismatch");
    wchar_t windows[MAX_PATH]{}, windowsRoot[MAX_PATH]{}, windowsVolume[64]{};
    UINT len = GetWindowsDirectoryW(windows, MAX_PATH);
    require(len != 0 && len < MAX_PATH, "Cannot identify Windows directory");
    win(GetVolumePathNameW(windows, windowsRoot, MAX_PATH) != FALSE, "Identify Windows volume root");
    win(GetVolumeNameForVolumeMountPointW(windowsRoot, windowsVolume, 64) != FALSE, "Identify Windows volume GUID");
    require(!same(o.volume, windowsVolume), "Refusing the Windows OS volume");
    const auto markerPath = o.volume + L".sprig-vss-synthetic-volume";
    Handle marker(CreateFileW(markerPath.c_str(), GENERIC_READ, FILE_SHARE_READ, nullptr,
                              OPEN_EXISTING, FILE_FLAG_OPEN_REPARSE_POINT, nullptr));
    win(static_cast<bool>(marker), "Open existing synthetic marker");
    BY_HANDLE_FILE_INFORMATION info{};
    win(GetFileInformationByHandle(marker.get(), &info) != FALSE, "Inspect marker identity");
    require((info.dwFileAttributes & (FILE_ATTRIBUTE_REPARSE_POINT | FILE_ATTRIBUTE_DIRECTORY)) == 0 &&
            info.dwVolumeSerialNumber == o.serial && info.nNumberOfLinks == 1,
            "Marker is not a single-link ordinary file on the expected volume");
    std::string expected = "sprig-vss-synthetic-only-v1\n";
    for (auto c : o.token) expected.push_back(static_cast<char>(c));
    expected.push_back('\n');
    require(info.nFileSizeHigh == 0 && info.nFileSizeLow == expected.size(), "Synthetic marker length mismatch");
    std::vector<char> bytes(expected.size());
    DWORD count = 0;
    win(ReadFile(marker.get(), bytes.data(), static_cast<DWORD>(bytes.size()), &count, nullptr) != FALSE, "Read synthetic marker");
    require(count == bytes.size() && std::string(bytes.begin(), bytes.end()) == expected, "Synthetic marker content mismatch");
    DWORD attrs = GetFileAttributesW(o.exe.c_str());
    require(attrs != INVALID_FILE_ATTRIBUTES &&
            (attrs & (FILE_ATTRIBUTE_DIRECTORY | FILE_ATTRIBUTE_REPARSE_POINT)) == 0,
            "Child executable missing, directory, or final-component reparse point");
    // This does not attempt to enable privileges or silently elevate.
    HANDLE rawToken = nullptr;
    win(OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &rawToken) != FALSE, "Query own token");
    Handle token(rawToken);
    TOKEN_ELEVATION elevation{}; DWORD returned = 0;
    win(GetTokenInformation(token.get(), TokenElevation, &elevation, sizeof(elevation), &returned) != FALSE, "Read token elevation");
    require(elevation.TokenIsElevated != 0, "An already elevated, explicitly approved test process is required");
}

class ComRuntime {
public:
    ComRuntime() {
        HRESULT hr = CoInitializeEx(nullptr, COINIT_MULTITHREADED);
        require(hr == S_OK || hr == S_FALSE, "CoInitializeEx failed or apartment mismatch");
        hr = CoInitializeSecurity(nullptr, -1, nullptr, nullptr, RPC_C_AUTHN_LEVEL_PKT_PRIVACY,
                                  RPC_C_IMP_LEVEL_IDENTIFY, nullptr, EOAC_NONE, nullptr);
        if (hr != S_OK) { CoUninitialize(); ok(hr, "Initialize process-local COM security"); }
    }
    ~ComRuntime() { CoUninitialize(); }
};

struct MgmtItem {
    VSS_MGMT_OBJECT_PROP p{};
    ~MgmtItem() {
        if (p.Type == VSS_MGMT_OBJECT_DIFF_AREA) {
            CoTaskMemFree(p.Obj.DiffArea.m_pwszVolumeName);
            CoTaskMemFree(p.Obj.DiffArea.m_pwszDiffAreaVolumeName);
        } else if (p.Type == VSS_MGMT_OBJECT_DIFF_VOLUME) {
            CoTaskMemFree(p.Obj.DiffVol.m_pwszVolumeName);
            CoTaskMemFree(p.Obj.DiffVol.m_pwszVolumeDisplayName);
        } else if (p.Type == VSS_MGMT_OBJECT_VOLUME) {
            CoTaskMemFree(p.Obj.Vol.m_pwszVolumeName);
            CoTaskMemFree(p.Obj.Vol.m_pwszVolumeDisplayName);
        }
    }
};

struct Area { std::wstring source, storage; LONGLONG maximum = 0, allocated = 0, used = 0; };

class Session {
    const Options& options_;
    Report& report_;
    ComPtr<IVssBackupComponents> backup_;
    ComPtr<IVssAsync> async_;
    ComPtr<IVssSnapshotMgmt> management_;
    ComPtr<IVssSnapshotMgmt2> management2_;
    ComPtr<IVssDifferentialSoftwareSnapshotMgmt> diff_;
    bool started_ = false, asyncSettled_ = true, committed_ = false, snapshotOwned_ = false;
    bool diffOwned_ = false, diffAddAttempted_ = false, cleaned_ = false;
    VSS_PWSZ volume() const { return const_cast<wchar_t*>(options_.volume.c_str()); }

    std::vector<Area> areas(bool on) {
        ComPtr<IVssEnumMgmtObject> items;
        HRESULT hr = on ? diff_->QueryDiffAreasOnVolume(volume(), items.put()) :
                          diff_->QueryDiffAreasForVolume(volume(), items.put());
        ok(hr, on ? "Query diff areas ON volume" : "Query diff areas FOR volume");
        require(static_cast<bool>(items), "Diff area query returned no enumerator");
        std::vector<Area> result;
        for (unsigned n = 0; n < 4096; ++n) {
            MgmtItem item; ULONG count = 0;
            hr = items->Next(1, &item.p, &count);
            if (hr == S_FALSE && count == 0) return result;
            ok(hr, "Enumerate diff area");
            require(count == 1 && item.p.Type == VSS_MGMT_OBJECT_DIFF_AREA, "Invalid diff area enumeration");
            const auto& a = item.p.Obj.DiffArea;
            require(a.m_pwszVolumeName && a.m_pwszDiffAreaVolumeName, "Missing diff area identity");
            result.push_back({a.m_pwszVolumeName, a.m_pwszDiffAreaVolumeName, a.m_llMaximumDiffSpace,
                              a.m_llAllocatedDiffSpace, a.m_llUsedDiffSpace});
        }
        fail("Diff-area enumeration exceeded bound");
    }
    void requireNoSnapshots() {
        BOOL present = TRUE; LONG capability = 0;
        ok(IsVolumeSnapshotted(volume(), &present, &capability), "Check snapshots on exact volume");
        require(present == FALSE, "Volume has a snapshot: preserve it and stop");
    }
    bool ownedAreaShape(const std::vector<Area>& a) const {
        return a.size() == 1 && same(a[0].source, options_.volume) &&
               same(a[0].storage, options_.volume) && a[0].maximum == kDiffBytes;
    }
    void requireOwnedArea() {
        require(ownedAreaShape(areas(false)) && ownedAreaShape(areas(true)),
                "Diff-area identity/quota differs or another association exists; preserve and stop");
    }
    bool queryAsync(HRESULT& status) noexcept {
        status = VSS_S_ASYNC_PENDING;
        HRESULT hr = async_->QueryStatus(&status, nullptr);
        if (hr != S_OK) { std::cerr << "QueryStatus: " << hexHr(hr) << '\n'; return false; }
        asyncSettled_ = status == VSS_S_ASYNC_FINISHED || status == VSS_S_ASYNC_CANCELLED || FAILED(status);
        return true;
    }
    bool snapshotAbsent() {
        VSS_SNAPSHOT_PROP p{};
        HRESULT hr = backup_->GetSnapshotProperties(report_.snapshotId, &p);
        if (hr == S_OK) {
            bool identity = IsEqualGUID(p.m_SnapshotId, report_.snapshotId) &&
                IsEqualGUID(p.m_SnapshotSetId, report_.setId) && IsEqualGUID(p.m_ProviderId, kProvider) &&
                p.m_pwszOriginalVolumeName && same(p.m_pwszOriginalVolumeName, options_.volume) &&
                p.m_lSnapshotsCount == 1;
            VssFreeSnapshotProperties(&p);
            require(identity, "Cleanup snapshot identity/source/provider/set mismatch; preserve it");
            return false;
        }
        if (hr == VSS_E_OBJECT_NOT_FOUND) return true;
        ok(hr, "Verify exact snapshot absence");
        return false;
    }
public:
    Session(const Options& o, Report& r) : options_(o), report_(r) {}
    ~Session() { if (!cleaned_) cleanup(); }
    void initialize() {
        ok(CreateVssBackupComponents(backup_.put()), "CreateVssBackupComponents");
        require(static_cast<bool>(backup_), "No backup components object");
        ok(backup_->InitializeForBackup(nullptr), "InitializeForBackup");
        ok(backup_->SetContext(VSS_CTX_FILE_SHARE_BACKUP), "Set writer-free ephemeral context");
        BOOL supported = FALSE;
        ok(backup_->IsVolumeSupported(kProvider, volume(), &supported), "Check explicit provider support");
        require(supported == TRUE, "Microsoft software provider does not support target volume");
        ok(CoCreateInstance(CLSID_VssSnapshotMgmt, nullptr, CLSCTX_ALL, __uuidof(IVssSnapshotMgmt),
                            reinterpret_cast<void**>(management_.put())), "Create VSS management interface");
        ok(management_->QueryInterface(__uuidof(IVssSnapshotMgmt2),
                                       reinterpret_cast<void**>(management2_.put())), "Query VSS management v2");
        ok(management_->GetProviderMgmtInterface(kProvider, __uuidof(IVssDifferentialSoftwareSnapshotMgmt),
                                                reinterpret_cast<IUnknown**>(diff_.put())), "Get system-provider diff management");
        LONGLONG minimum = 0;
        ok(management2_->GetMinDiffAreaSize(&minimum), "Read minimum diff-area size");
        require(minimum > 0 && minimum <= kDiffBytes, "Host minimum exceeds 512 MiB; no quota fallback allowed");
        requireNoSnapshots();
        require(areas(false).empty() && areas(true).empty(), "Target has existing FOR/ON association; no changes permitted");
        // Read-only checks are not a global provider transaction. This experiment
        // requires an exclusively owned fresh fixture. Never run with another
        // requester targeting it, and preserve rather than guess after any race.
        diffAddAttempted_ = true;
        report_.diffCleanup = L"unverified";
        ok(diff_->AddDiffArea(volume(), volume(), kDiffBytes), "Add own same-volume 512 MiB diff area");
        diffOwned_ = true;  // Only a confirmed successful Add grants removal ownership.
        std::wcout << L"SPRIG_VSS_DIFF_ADDED 536870912" << std::endl;
        requireOwnedArea();
        requireNoSnapshots();
    }
    std::wstring create() {
        // Repeat identity immediately before mutation; never substitute a volume.
        preflight(options_);
        requireOwnedArea();
        started_ = true;
        report_.snapshotCleanup = L"unverified";
        ok(backup_->StartSnapshotSet(&report_.setId), "StartSnapshotSet");
        require(nonzero(report_.setId), "Missing snapshot-set ID");
        std::wcout << L"SPRIG_VSS_SET_ID " << guidText(report_.setId) << std::endl;
        ok(backup_->AddToSnapshotSet(volume(), kProvider, &report_.snapshotId), "Add one approved volume");
        require(nonzero(report_.snapshotId), "Missing snapshot ID");
        snapshotOwned_ = true;
        std::wcout << L"SPRIG_VSS_SNAPSHOT_ID " << guidText(report_.snapshotId) << std::endl;
        asyncSettled_ = false;
        ok(backup_->DoSnapshotSet(async_.put()), "Begin DoSnapshotSet");
        require(static_cast<bool>(async_), "DoSnapshotSet did not return IVssAsync");
        HRESULT waitHr = async_->Wait(options_.vssMs);
        HRESULT status = VSS_S_ASYNC_PENDING;
        bool queryOk = queryAsync(status);
        ok(waitHr, "Wait for snapshot completion");
        require(queryOk, "Cannot verify async completion");
        require(status == VSS_S_ASYNC_FINISHED, "Snapshot did not finish successfully; pending/cancelled/failure is not success");
        committed_ = true;
        VSS_SNAPSHOT_PROP p{};
        ok(backup_->GetSnapshotProperties(report_.snapshotId, &p), "Read exact snapshot properties");
        struct Free { VSS_SNAPSHOT_PROP* p; ~Free() { VssFreeSnapshotProperties(p); } } free{&p};
        require(IsEqualGUID(p.m_SnapshotId, report_.snapshotId) &&
                IsEqualGUID(p.m_SnapshotSetId, report_.setId) && IsEqualGUID(p.m_ProviderId, kProvider),
                "Snapshot identity/set/provider mismatch");
        require(p.m_lSnapshotsCount == 1 && p.m_eStatus == VSS_SS_CREATED,
                "Snapshot count/state mismatch");
        require(p.m_pwszOriginalVolumeName && same(p.m_pwszOriginalVolumeName, options_.volume), "Snapshot source volume mismatch");
        require((p.m_lSnapshotAttributes & VSS_VOLSNAP_ATTR_NO_WRITERS) != 0 &&
                (p.m_lSnapshotAttributes & (VSS_VOLSNAP_ATTR_PERSISTENT | VSS_VOLSNAP_ATTR_NO_AUTO_RELEASE)) == 0,
                "Snapshot attributes are not writer-free, ephemeral, auto-release");
        require(p.m_pwszSnapshotDeviceObject && *p.m_pwszSnapshotDeviceObject, "Missing snapshot device root");
        std::wstring root = p.m_pwszSnapshotDeviceObject;
        require(root.rfind(L"\\\\?\\GLOBALROOT\\Device\\HarddiskVolumeShadowCopy", 0) == 0,
                "Unexpected system-provider snapshot device path");
        if (root.back() != L'\\') root.push_back(L'\\');
        requireOwnedArea();
        report_.snapshotCreated = true;
        std::wcout << L"SPRIG_VSS_SNAPSHOT_ROOT " << root << std::endl;
        std::wcout << L"SPRIG_VSS_CONSISTENCY independent_storage_no_writers" << std::endl;
        return root;
    }
    void sampleDiffWhileSnapshotExists() {
        require(committed_ && report_.snapshotCreated, "No completed snapshot available for diff measurement");
        require(!snapshotAbsent(), "Snapshot disappeared during child operation");
        const auto forAreas = areas(false);
        require(ownedAreaShape(forAreas) && ownedAreaShape(areas(true)), "Diff-area sample identity changed");
        const auto& a = forAreas[0];
        require(a.used >= 0 && a.allocated >= a.used && a.maximum >= a.allocated,
                "Invalid diff used/allocated/maximum metric");
        report_.diffUsed = a.used; report_.diffAllocated = a.allocated; report_.diffMaximum = a.maximum;
    }
    bool cleanup() noexcept {
        if (cleaned_) return report_.snapshotCleanup != L"unverified" && report_.diffCleanup != L"unverified";
        cleaned_ = true;
        const ULONGLONG cleanupStart = GetTickCount64();
        bool snapshotsClean = !started_;
        try {
            if (async_ && !asyncSettled_) {
                HRESULT hr = async_->Cancel();
                std::cerr << "Cancel async: " << hexHr(hr) << '\n';
                HRESULT waited = async_->Wait(kDrainMs);
                std::cerr << "Drain async: " << hexHr(waited) << '\n';
                HRESULT status = VSS_S_ASYNC_PENDING;
                (void)queryAsync(status);
            }
            bool abortOk = true;
            if (started_ && !committed_) {
                HRESULT hr = backup_->AbortBackup();
                abortOk = hr == S_OK;
                std::cerr << "Abort incomplete snapshot set: " << hexHr(hr) << '\n';
            }
            require(report_.childDrained, "Child termination not confirmed; snapshot/diff cleanup withheld");
            require(!started_ || (asyncSettled_ && abortOk), "VSS operation unsettled; explicit cleanup withheld");
            if (nonzero(report_.snapshotId)) {
                if (!snapshotAbsent()) {
                    require(snapshotOwned_, "Snapshot add did not succeed; deletion ownership not established");
                    LONG deleted = 0; VSS_ID undeleted = GUID_NULL;
                    HRESULT hr = backup_->DeleteSnapshots(report_.snapshotId, VSS_OBJECT_SNAPSHOT, FALSE, &deleted, &undeleted);
                    // Do not retry an uncertain deletion or widen to set/all.
                    ok(hr, "Delete exact owned snapshot");
                    require(deleted == 1 && !nonzero(undeleted), "Exact deletion count/undeleted ID mismatch");
                }
                require(snapshotAbsent(), "Exact snapshot still exists after cleanup");
                report_.snapshotCleanup = L"confirmed";
                snapshotsClean = true;
            } else if (started_) {
                requireNoSnapshots();
                report_.snapshotCleanup = L"confirmed";
                snapshotsClean = true;
            }
        } catch (const std::exception& e) { std::cerr << "Snapshot cleanup: " << e.what() << '\n'; }
        // Explicit deletion + absence precede release and any diff-area removal.
        async_.reset();
        backup_.reset();  // Auto-release is a safety net, never evidence of cleanup.
        try {
            if (diffAddAttempted_) {
                require(snapshotsClean && report_.childDrained, "Cannot remove diff area before verified snapshot/child completion");
                requireNoSnapshots();
                auto forAreas = areas(false), onAreas = areas(true);
                if (forAreas.empty() && onAreas.empty()) report_.diffCleanup = L"confirmed";
                else {
                    require(diffOwned_, "Add result uncertain; association ownership not established, preserve it");
                    require(ownedAreaShape(forAreas) && ownedAreaShape(onAreas), "Association changed or shared; preserve it");
                    ok(diff_->ChangeDiffAreaMaximumSize(volume(), volume(), 0), "Remove exact owned diff-area association");
                    require(areas(false).empty() && areas(true).empty(), "Diff-area removal not yet confirmed; preserve fixture");
                    report_.diffCleanup = L"confirmed";
                }
            }
        } catch (const std::exception& e) { std::cerr << "Diff-area cleanup: " << e.what() << '\n'; }
        report_.cleanupSeconds = static_cast<double>(GetTickCount64() - cleanupStart) / 1000.0;
        return snapshotsClean && report_.snapshotCleanup != L"unverified" && report_.diffCleanup != L"unverified";
    }
};

// MS C/C++ argv quoting: shell metacharacters are ordinary data. Passing an
// explicit lpApplicationName prevents executable selection from argument text.
std::wstring quoteArg(const std::wstring& arg) {
    std::wstring out = L"\""; size_t slashes = 0;
    for (wchar_t c : arg) {
        if (c == L'\\') { ++slashes; continue; }
        out.append(c == L'\"' ? slashes * 2 + 1 : slashes, L'\\');
        slashes = 0; out.push_back(c);
    }
    out.append(slashes * 2, L'\\');
    out.push_back(L'\"'); return out;
}

class Attributes {
    std::vector<unsigned char> storage_;
    LPPROC_THREAD_ATTRIBUTE_LIST p_ = nullptr;
public:
    Attributes() {
        SIZE_T size = 0;
        BOOL result = InitializeProcThreadAttributeList(nullptr, 2, 0, &size);
        require(!result && GetLastError() == ERROR_INSUFFICIENT_BUFFER && size > 0,
                "Cannot size process attribute list");
        storage_.resize(size);
        auto p = reinterpret_cast<LPPROC_THREAD_ATTRIBUTE_LIST>(storage_.data());
        win(InitializeProcThreadAttributeList(p, 2, 0, &size) != FALSE, "Initialize process attributes");
        p_ = p;
    }
    ~Attributes() { if (p_) DeleteProcThreadAttributeList(p_); }
    LPPROC_THREAD_ATTRIBUTE_LIST get() const { return p_; }
};

bool waitJobEmpty(HANDLE job, DWORD timeout) {
    const ULONGLONG start = GetTickCount64();
    for (;;) {
        JOBOBJECT_BASIC_ACCOUNTING_INFORMATION account{};
        win(QueryInformationJobObject(job, JobObjectBasicAccountingInformation, &account,
                                      sizeof(account), nullptr) != FALSE, "Query owned child job");
        if (account.ActiveProcesses == 0) return true;
        if (GetTickCount64() - start >= timeout) return false;
        Sleep(50);
    }
}

void runChild(const Options& o, const std::wstring& root, Report& r) {
    std::wstring command = quoteArg(o.exe);
    for (const auto& arg : o.args) command += L" " + quoteArg(arg == L"{snapshot-root}" ? root : arg);
    require(command.size() < 30000, "Child command line exceeds conservative bound");
    std::vector<wchar_t> mutableCommand(command.begin(), command.end());
    mutableCommand.push_back(L'\0');
    Handle job(CreateJobObjectW(nullptr, nullptr));
    win(static_cast<bool>(job), "Create private child job");
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION limits{};
    limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    win(SetInformationJobObject(job.get(), JobObjectExtendedLimitInformation, &limits,
                                sizeof(limits)) != FALSE, "Set owned-job lifetime");
    HANDLE jobForAttribute = job.get();
    SECURITY_ATTRIBUTES inheritable{sizeof(SECURITY_ATTRIBUTES), nullptr, TRUE};
    Handle nullOutput(CreateFileW(L"NUL", GENERIC_READ | GENERIC_WRITE,
                                  FILE_SHARE_READ | FILE_SHARE_WRITE, &inheritable, OPEN_EXISTING, 0, nullptr));
    win(static_cast<bool>(nullOutput), "Open isolated child standard streams");
    HANDLE inheritedNull = nullOutput.get();
    Attributes attributes;
    win(UpdateProcThreadAttribute(attributes.get(), 0, PROC_THREAD_ATTRIBUTE_JOB_LIST,
                                   &jobForAttribute, sizeof(jobForAttribute), nullptr, nullptr) != FALSE,
        "Assign job at child creation");
    win(UpdateProcThreadAttribute(attributes.get(), 0, PROC_THREAD_ATTRIBUTE_HANDLE_LIST,
                                   &inheritedNull, sizeof(inheritedNull), nullptr, nullptr) != FALSE,
        "Restrict child inherited handles");
    STARTUPINFOEXW startup{};
    startup.StartupInfo.cb = sizeof(startup);
    startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
    startup.StartupInfo.hStdInput = inheritedNull;
    startup.StartupInfo.hStdOutput = inheritedNull;
    startup.StartupInfo.hStdError = inheritedNull;
    startup.lpAttributeList = attributes.get();
    PROCESS_INFORMATION process{};
    // Child cwd is its executable directory, not the snapshot or live fixture.
    const std::wstring cwd = o.exe.substr(0, o.exe.find_last_of(L'\\') + 1);
    win(CreateProcessW(o.exe.c_str(), mutableCommand.data(), nullptr, nullptr, TRUE,
                        CREATE_SUSPENDED | EXTENDED_STARTUPINFO_PRESENT, nullptr, cwd.c_str(),
                        &startup.StartupInfo, &process) != FALSE, "Create specified child directly");
    Handle child(process.hProcess), thread(process.hThread);
    r.childStarted = true; r.childDrained = false;
    try {
        require(ResumeThread(thread.get()) != static_cast<DWORD>(-1), "Cannot resume child");
        std::wcout << L"SPRIG_VSS_CHILD_PID " << process.dwProcessId << std::endl;
        require(waitJobEmpty(job.get(), o.childMs), "Child/job timed out");
        r.childDrained = true;
        require(WaitForSingleObject(child.get(), 0) == WAIT_OBJECT_0, "Child exit not signalled");
        win(GetExitCodeProcess(child.get(), &r.childExit) != FALSE, "Get child exit code");
        r.childExitKnown = true;
        require(r.childExit == 0, "Child operation failed");
    } catch (...) {
        if (!r.childDrained) {
            if (!TerminateJobObject(job.get(), ERROR_CANCELLED))
                std::cerr << "Terminate owned job: " << hexHr(HRESULT_FROM_WIN32(GetLastError())) << '\n';
            try { r.childDrained = waitJobEmpty(job.get(), kDrainMs); }
            catch (const std::exception& e) { std::cerr << "Child drain: " << e.what() << '\n'; }
            if (WaitForSingleObject(child.get(), 0) == WAIT_OBJECT_0 && GetExitCodeProcess(child.get(), &r.childExit))
                r.childExitKnown = true;
        }
        // Kill-on-close is only a fallback; unconfirmed drain remains a failure.
        throw;
    }
}

void usage() {
    std::wcout << L"Experimental storage-only VSS requester, not a Sprig checkpoint.\n"
        L"Required: --approved-synthetic-volume \\\\?\\Volume{GUID}\\ --expected-serial 8HEX\n"
        L"  --marker-token 32HEX --acknowledge-storage-snapshot-only --allow-own-volume-diff-512m\n"
        L"  --child-exe C:\\absolute\\python.exe [--vss-timeout-seconds 120]\n"
        L"  [--child-timeout-seconds 300] -- child-arguments {snapshot-root}\n"
        L"Exactly one whole child argument must be {snapshot-root}. No shell is invoked.\n"
        L"Marker: .sprig-vss-synthetic-volume with ASCII sprig-vss-synthetic-only-v1 LF TOKEN LF.\n"
        L"Timeouts are 1..3600 seconds; API calls themselves are not hard real-time bounded.\n"
        L"Only exit 0 plus final ok=true and cleanup confirmations is a completed experiment.\n"
        L"Missing/unverified result means preserve fixture; never detach or blanket-clean it.\n";
}
} // namespace

int wmain(int argc, wchar_t** argv) {
    if (argc == 2 && std::wstring(argv[1]) == L"--help") { usage(); return 0; }
    Report report;
    bool success = false;
    try {
        Options options = parse(argc, argv);
        report.volume = options.volume;
        require(SPRIG_VSS_ENABLE_RUNTIME == 1, "Runtime disabled: source/compile-only build; separate approved runtime build required");
        preflight(options);
        ComRuntime com;
        Session session(options, report);
        try {
            session.initialize();
            const ULONGLONG snapshotStart = GetTickCount64();
            const auto root = session.create();
            report.snapshotSeconds = static_cast<double>(GetTickCount64() - snapshotStart) / 1000.0;
            // The IVssBackupComponents instance stays alive until the entire
            // bounded child operation and its ordinary descendants have ended.
            const ULONGLONG childStart = GetTickCount64();
            try { runChild(options, root, report); }
            catch (...) {
                report.childSeconds = static_cast<double>(GetTickCount64() - childStart) / 1000.0;
                throw;
            }
            report.childSeconds = static_cast<double>(GetTickCount64() - childStart) / 1000.0;
            session.sampleDiffWhileSnapshotExists();
            success = true;
        } catch (const std::exception& e) { std::cerr << "Experiment failed: " << e.what() << '\n'; }
        success = session.cleanup() && success;
    } catch (const std::exception& e) { std::cerr << "Requester refused/failed: " << e.what() << '\n'; }
    report.print(success);
    return success ? 0 : 1;
}
