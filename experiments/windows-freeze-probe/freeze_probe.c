/* SPDX-License-Identifier: MIT
 * Copyright (c) 2026 M4n5ter
 * Original test harness. See HARNESS.md for the limited falsification claim.
 * Source preparation only: this file does not install, load, or build a driver.
 */
#define WIN32_LEAN_AND_MEAN
#define _WIN32_WINNT 0x0602
#include <windows.h>
#include <winioctl.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <wchar.h>
#include <string.h>
#include <errno.h>
#include "probe_protocol.h"

#if !defined(_WIN64) || (!defined(_M_X64) && !defined(__x86_64__))
#error This harness requires x64 Windows, not x86 or ARM64.
#endif
#ifdef _MSC_VER
#pragma comment(lib, "advapi32.lib")
#endif

typedef char request_size_is_56[(sizeof(SPRIG_PROBE_REQUEST) == 56) ? 1 : -1];
typedef char result_size_is_88[(sizeof(SPRIG_PROBE_RESULT) == 88) ? 1 : -1];
#define CONTROL_MAGIC 0x53505247u
#define CONTROL_VERSION 1u
#define CONTROL_BYTES 4096u
#define READY_TIMEOUT_MS 5000u
#define ACK_TIMEOUT_MS 2000u
#define JOIN_TIMEOUT_MS 5000u
#define OBSERVE_MS 40u
#define MIN_ATTEMPT_OBSERVE_MS 30u
#define NT_NOT_SUPPORTED ((LONG)0xc00000bbu)
#define INVALID_LOCAL_HANDLE(h) ((h) == NULL || (h) == INVALID_HANDLE_VALUE)

/* All interprocess fields below are naturally aligned in a page-aligned view.
 * Control traffic is pagefile-backed, NOT on the filesystem being tested.
 * Only the main writer touches the original file slot. A separate heartbeat
 * proves that the process is still scheduled while that particular store stalls.
 */
typedef struct SHARED_CONTROL {
    ULONG Magic, Version, Bytes, Ordinal;
    ULONGLONG Nonce;
    DWORD ParentPid, Reserved;
    volatile LONG Stop, Started, ChildError, Exited;
    volatile LONG64 Heartbeat;
    volatile LONG64 RequestToken;
    volatile LONG64 AttemptStarted;
    volatile LONG64 AttemptCounter;
    volatile LONG64 AckToken;
    volatile LONG64 AckCounter;
    volatile LONG64 Writes;
    volatile LONG64 Idle;
    volatile LONG64 BackgroundSequence, BackgroundWrites;
    volatile LONG64 BackgroundAttemptCounter, BackgroundAckCounter;
} SHARED_CONTROL;
typedef char control_fits_page[(sizeof(SHARED_CONTROL) <= CONTROL_BYTES) ? 1 : -1];

typedef struct FILE_FIXTURE {
    HANDLE File, Section, ControlSection, Process;
    void *View;
    SHARED_CONTROL *Control;
    BOOL Owned;
    ULONG Ordinal;
    DWORD Serial;
    LONG64 NextToken;
} FILE_FIXTURE;

typedef struct IO_CALL {
    HANDLE File, Ready, Release, Thread;
    SPRIG_PROBE_REQUEST Request;
    SPRIG_PROBE_RESULT Result;
    BOOL TransportOk;
    DWORD Error, Bytes;
    BOOL Submitted, ReadySeen, ReleasedByParent, Joined;
    LONGLONG ReleaseCounter;
} IO_CALL;

typedef struct TOKEN_OBSERVATION {
    LONG64 Token;
    LONGLONG IssuedCounter, ObservedCounter, AttemptCounter, AckCounter;
    BOOL AttemptStarted, Acknowledged;
} TOKEN_OBSERVATION;

typedef struct BACKGROUND_OBSERVATION {
    LONG64 Writes;
    LONGLONG AttemptCounter, AckCounter, ObservedCounter;
    BOOL Valid;
} BACKGROUND_OBSERVATION;
typedef struct BACKGROUND_CONTEXT {
    SHARED_CONTROL *Control;
    volatile LONG64 *Slot;
} BACKGROUND_CONTEXT;

typedef struct TRIAL {
    ULONG Mode;
    IO_CALL Io[2];
    TOKEN_OBSERVATION AFirst, BOverlap, AOverlap, Restored[2], PendingRecovered[2];
    BACKGROUND_OBSERVATION BackgroundFirst[2], BackgroundOverlap[2];
    LONG64 BackgroundBeforeSubmit[2], BackgroundAfterRecovery[2];
    LONG64 BackgroundBFirstBefore;
    BOOL BackgroundSamplesValid, BackgroundRecovered, BackgroundFalsified;
    BOOL AttemptWindowsValid;
    LONG64 HeartbeatFirstBefore[2], HeartbeatFirstAfter[2];
    LONG64 HeartbeatOverlapBefore[2], HeartbeatOverlapAfter[2];
    ULONG BProgress;
    LONGLONG BFirstAttempt, BLastAck;
    BOOL ReusedPendingA, Ran, ObservationCompleted, Recovered;
    BOOL KernelCompleted, TimingValid, HeartbeatsValid;
    BOOL NegativeValid, FreezeObserved, Falsified;
    const char *Reason;
    DWORD Error;
} TRIAL;

static volatile LONG gAbort;
static LARGE_INTEGER gFrequency;
static BOOL WINAPI on_console(DWORD type)
{
    if (type == CTRL_C_EVENT || type == CTRL_BREAK_EVENT ||
        type == CTRL_CLOSE_EVENT || type == CTRL_LOGOFF_EVENT ||
        type == CTRL_SHUTDOWN_EVENT) {
        InterlockedExchange(&gAbort, 1);
        return TRUE;
    }
    return FALSE;
}
static LONGLONG counter_now(void)
{
    LARGE_INTEGER q;
    QueryPerformanceCounter(&q);
    return q.QuadPart;
}
static LONG64 load64(volatile LONG64 *value)
{
    /* Control mapping only. No compare-and-swap is performed on a file view. */
    return InterlockedOr64(value, 0);
}
static LONG load32(volatile LONG *value)
{
    return InterlockedOr(value, 0);
}
static BOOL elapsed(ULONGLONG start, DWORD ms)
{
    return GetTickCount64() - start >= ms;
}
static BOOL process_alive(FILE_FIXTURE *f)
{
    return f->Process && WaitForSingleObject(f->Process, 0) == WAIT_TIMEOUT;
}
static void fixture_init(FILE_FIXTURE *f, ULONG ordinal)
{
    ZeroMemory(f, sizeof(*f));
    f->File = INVALID_HANDLE_VALUE;
    f->Ordinal = ordinal;
}
static BOOL parse_u64(const wchar_t *s, ULONGLONG *out)
{
    wchar_t *end;
    unsigned long long v;
    if (!s || !*s || *s == L'-' || *s == L'+') return FALSE;
    errno = 0;
    v = wcstoull(s, &end, 10);
    if (errno || *end) return FALSE;
    *out = (ULONGLONG)v;
    return TRUE;
}
static DWORD WINAPI heartbeat_thread(void *arg)
{
    SHARED_CONTROL *c = (SHARED_CONTROL *)arg;
    while (!load32(&c->Stop)) {
        InterlockedIncrement64(&c->Heartbeat);
        Sleep(1);
    }
    return 0;
}
static DWORD WINAPI background_writer_thread(void *arg)
{
    BACKGROUND_CONTEXT *context = (BACKGROUND_CONTEXT *)arg;
    SHARED_CONTROL *c = context->Control;
    LONG64 value = 0;
    while (!load32(&c->Stop)) {
        LONGLONG attempted = counter_now(), acknowledged;
        /* Separate continuously active resident-store lane. It is never
         * paused for Cc invocation or token commands, and uses the second
         * page of this SAME original, pre-faulted writable mapping. */
        InterlockedExchange64(context->Slot, ++value);
        acknowledged = counter_now();
        /* Even/odd sequence protects a small completed-store observation.
         * A snapshot may still miss stores between samples; this is stress
         * coverage, not an exhaustive trace or backing-store oracle. */
        InterlockedIncrement64(&c->BackgroundSequence);
        InterlockedExchange64(&c->BackgroundAttemptCounter, attempted);
        InterlockedExchange64(&c->BackgroundAckCounter, acknowledged);
        InterlockedIncrement64(&c->BackgroundWrites);
        InterlockedIncrement64(&c->BackgroundSequence);
        Sleep(0);
    }
    return 0;
}
static void retain_forever(void)
{
    /* Intentional quarantine. Never TerminateThread/TerminateProcess, close a
     * live IOCTL's handle, or return from main to tear down stuck mappings.
     * A separate, authorized disposable-guest supervisor must handle this.
     */
    for (;;) Sleep(1000);
}
static int writer_main(int argc, wchar_t **argv)
{
    ULONGLONG n[5];
    HANDLE file, section, controlSection, heartbeat = NULL, background = NULL;
    BACKGROUND_CONTEXT backgroundContext;
    SHARED_CONTROL *c = NULL;
    unsigned char *view = NULL;
    volatile LONG64 *slot;
    LARGE_INTEGER size;
    LONG64 last = 0;
    unsigned int i;
    int result = 1;
    if (argc != 7) return 2;
    for (i = 0; i < 5; ++i)
        if (!parse_u64(argv[i + 2], &n[i])) return 2;
    if (n[0] > 1 || !n[1] || !n[2] || !n[3] || !n[4]) return 2;
    file = (HANDLE)(ULONG_PTR)n[1];
    section = (HANDLE)(ULONG_PTR)n[2];
    controlSection = (HANDLE)(ULONG_PTR)n[3];
    c = (SHARED_CONTROL *)MapViewOfFile(controlSection, FILE_MAP_ALL_ACCESS,
                                      0, 0, CONTROL_BYTES);
    if (!c) return 2;
    if (c->Magic != CONTROL_MAGIC || c->Version != CONTROL_VERSION ||
        c->Bytes != sizeof(*c) || c->Ordinal != (ULONG)n[0] ||
        c->Nonce != n[4] || !c->ParentPid || c->ParentPid == GetCurrentProcessId()) {
        UnmapViewOfFile(c);
        return 2;
    }
    if (!GetFileSizeEx(file, &size) || size.QuadPart != SPRIG_PROBE_FILE_BYTES) {
        InterlockedExchange(&c->ChildError, ERROR_BAD_LENGTH);
        goto cleanup;
    }
    view = (unsigned char *)MapViewOfFile(section, FILE_MAP_WRITE, 0, 0,
                                         SPRIG_PROBE_FILE_BYTES);
    if (!view) {
        InterlockedExchange(&c->ChildError, (LONG)GetLastError());
        goto cleanup;
    }
    /* Pre-fault every page of this original writable view before announcing
     * Started. File bytes are initially zero. The exact same view and aligned
     * slot remain live across both experiments; no remap or reopen is used.
     */
    for (i = 0; i < SPRIG_PROBE_FILE_BYTES; i += 4096)
        InterlockedExchange64((volatile LONG64 *)(view + i), 0);
    slot = (volatile LONG64 *)view;
    if (((ULONG_PTR)slot & 7u) != 0) {
        InterlockedExchange(&c->ChildError, ERROR_DATATYPE_MISMATCH);
        goto cleanup;
    }
    heartbeat = CreateThread(NULL, 0, heartbeat_thread, c, 0, NULL);
    if (!heartbeat) {
        InterlockedExchange(&c->ChildError, (LONG)GetLastError());
        goto cleanup;
    }
    backgroundContext.Control = c;
    backgroundContext.Slot = (volatile LONG64 *)(view + 4096);
    background = CreateThread(NULL, 0, background_writer_thread, &backgroundContext, 0, NULL);
    if (!background) {
        InterlockedExchange(&c->ChildError, (LONG)GetLastError());
        goto cleanup;
    }
    InterlockedExchange64(&c->Idle, 1);
    InterlockedExchange(&c->Started, 1);
    while (!load32(&c->Stop)) {
        LONG64 token = load64(&c->RequestToken);
        if (token != 0 && token != last) {
            InterlockedExchange64(&c->Idle, 0);
            InterlockedExchange64(&c->AttemptCounter, counter_now());
            InterlockedExchange64(&c->AttemptStarted, token);
            /* This is the operation under test. attempt_started is not an ack.
             * A lack of ack alone never establishes success in this harness.
             */
            InterlockedExchange64(slot, (token << 2) | ((LONG64)c->Ordinal + 1));
            InterlockedExchange64(&c->AckCounter, counter_now());
            InterlockedIncrement64(&c->Writes);
            InterlockedExchange64(&c->AckToken, token);
            last = token;
            InterlockedExchange64(&c->Idle, 1);
        } else {
            Sleep(1);
        }
    }
    result = 0;
cleanup:
    InterlockedExchange(&c->Stop, 1);
    if (background) {
        if (WaitForSingleObject(background, JOIN_TIMEOUT_MS) != WAIT_OBJECT_0)
            retain_forever();
        CloseHandle(background);
    }
    if (heartbeat) {
        if (WaitForSingleObject(heartbeat, JOIN_TIMEOUT_MS) != WAIT_OBJECT_0)
            retain_forever();
        CloseHandle(heartbeat);
    }
    /* This path is reached only after the main thread's last store returned. */
    if (view) UnmapViewOfFile(view);
    InterlockedExchange(&c->Exited, 1);
    UnmapViewOfFile(c);
    CloseHandle(section);
    CloseHandle(controlSection);
    CloseHandle(file);
    return result;
}

static BOOL enable_existing_load_privilege(DWORD *error)
{
    HANDLE token = NULL;
    PSID admin = NULL;
    SID_IDENTIFIER_AUTHORITY nt = SECURITY_NT_AUTHORITY;
    BOOL isAdmin = FALSE, enabled = FALSE;
    TOKEN_PRIVILEGES request;
    BYTE *buffer = NULL;
    DWORD needed = 0, i;
    *error = ERROR_SUCCESS;
    if (!AllocateAndInitializeSid(&nt, 2, SECURITY_BUILTIN_DOMAIN_RID,
        DOMAIN_ALIAS_RID_ADMINS, 0, 0, 0, 0, 0, 0, &admin)) goto fail;
    if (!CheckTokenMembership(NULL, admin, &isAdmin)) goto fail;
    FreeSid(admin); admin = NULL;
    if (!isAdmin) { *error = ERROR_ELEVATION_REQUIRED; return FALSE; }
    if (!OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY | TOKEN_ADJUST_PRIVILEGES,
                          &token)) goto fail;
    ZeroMemory(&request, sizeof(request));
    request.PrivilegeCount = 1;
    if (!LookupPrivilegeValueW(NULL, SE_LOAD_DRIVER_NAME,
                               &request.Privileges[0].Luid)) goto fail;
    request.Privileges[0].Attributes = SE_PRIVILEGE_ENABLED;
    SetLastError(ERROR_SUCCESS);
    if (!AdjustTokenPrivileges(token, FALSE, &request, 0, NULL, NULL)) goto fail;
    if (GetLastError() != ERROR_SUCCESS) goto fail;
    /* AdjustTokenPrivileges cannot grant a missing privilege. Verify that the
     * already-assigned privilege is now present and enabled, rather than
     * treating its BOOL return alone as proof. No security policy is changed.
     */
    GetTokenInformation(token, TokenPrivileges, NULL, 0, &needed);
    if (GetLastError() != ERROR_INSUFFICIENT_BUFFER || needed == 0) goto fail;
    buffer = (BYTE *)HeapAlloc(GetProcessHeap(), 0, needed);
    if (!buffer) { *error = ERROR_NOT_ENOUGH_MEMORY; goto done; }
    if (!GetTokenInformation(token, TokenPrivileges, buffer, needed, &needed)) goto fail;
    for (i = 0; i < ((TOKEN_PRIVILEGES *)buffer)->PrivilegeCount; ++i) {
        LUID_AND_ATTRIBUTES *p = &((TOKEN_PRIVILEGES *)buffer)->Privileges[i];
        if (p->Luid.LowPart == request.Privileges[0].Luid.LowPart &&
            p->Luid.HighPart == request.Privileges[0].Luid.HighPart &&
            (p->Attributes & SE_PRIVILEGE_ENABLED)) enabled = TRUE;
    }
    if (!enabled) *error = ERROR_PRIVILEGE_NOT_HELD;
    goto done;
fail:
    *error = GetLastError();
    if (*error == ERROR_SUCCESS) *error = ERROR_PRIVILEGE_NOT_HELD;
done:
    if (admin) FreeSid(admin);
    if (buffer) HeapFree(GetProcessHeap(), 0, buffer);
    if (token) CloseHandle(token);
    return enabled;
}
static BOOL has_probe_marker(const wchar_t *name)
{
    static const wchar_t marker[] = L"SPRIGPROBE";
    size_t i, len = wcslen(name);
    for (i = 0; i + (sizeof(marker)/sizeof(marker[0]) - 1) <= len; ++i)
        if (_wcsnicmp(name + i, marker, sizeof(marker)/sizeof(marker[0]) - 1) == 0)
            return TRUE;
    return FALSE;
}
static const char *gRootValidation = "not_started";
static BOOL check_mount_root(const wchar_t *argument, wchar_t *root,
                             DWORD rootCount, DWORD *serial, DWORD *error)
{
    wchar_t fs[256];
    DWORD n, flags, maxComponent;
    BY_HANDLE_FILE_INFORMATION info;
    HANDLE handle;
    BOOL ok = FALSE;
    *error = ERROR_INVALID_NAME;
    gRootValidation = "exact_drive_root";
    /* Only the exact newly owned DOS drive root is supported. No relative,
     * directory mount, UNC, device namespace or ADS path is accepted. */
    if (!argument || wcslen(argument) != 3 ||
        !((argument[0] >= L'A' && argument[0] <= L'Z') ||
          (argument[0] >= L'a' && argument[0] <= L'z')) ||
        argument[1] != L':' || argument[2] != L'\\') return FALSE;
    n = GetFullPathNameW(argument, rootCount, root, NULL);
    if (n != 3 || n >= rootCount) { *error = GetLastError(); return FALSE; }
    /* Upstream volpath-test.c documents GetVolumePathName as unreliable for
     * non-MountManager mounts. Bind the actual open root instead, exactly as
     * each subsequent file handle is bound to its volume below. */
    gRootValidation = "open_root_handle";
    handle = CreateFileW(root, FILE_READ_ATTRIBUTES,
        FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE, NULL,
        OPEN_EXISTING, FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT, NULL);
    if (handle == INVALID_HANDLE_VALUE) { *error = GetLastError(); return FALSE; }
    gRootValidation = "root_handle_metadata";
    if (!GetFileInformationByHandle(handle, &info)) { *error = GetLastError(); goto done; }
    if (!(info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY) ||
        (info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT)) {
        *error = ERROR_DIRECTORY; goto done;
    }
    gRootValidation = "root_volume_information_by_handle";
    if (!GetVolumeInformationByHandleW(handle, NULL, 0, serial,
                                      &maxComponent, &flags, fs, 256)) {
        *error = GetLastError(); goto done;
    }
    gRootValidation = "root_volume_binding";
    if (info.dwVolumeSerialNumber != *serial || !has_probe_marker(fs)) {
        *error = ERROR_NOT_SUPPORTED; goto done;
    }
    gRootValidation = "passed";
    ok = TRUE;
done:
    CloseHandle(handle);
    return ok;
}
static BOOL create_fixture(FILE_FIXTURE *f, const wchar_t *root, DWORD serial,
                           ULONGLONG nonce, DWORD *error)
{
    wchar_t path[32768], fs[256];
    const wchar_t *name = f->Ordinal == 0 ? SPRIG_PROBE_FILE_A : SPRIG_PROBE_FILE_B;
    SECURITY_ATTRIBUTES sa = {sizeof(sa), NULL, TRUE};
    BYTE zeros[SPRIG_PROBE_FILE_BYTES];
    BY_HANDLE_FILE_INFORMATION info;
    DWORD written, flags, component, actualSerial;
    LARGE_INTEGER size;
    size_t len = wcslen(root), nameLen = wcslen(name);
    unsigned int i;
    if (len + nameLen + 1 > 32768) { *error = ERROR_FILENAME_EXCED_RANGE; return FALSE; }
    memcpy(path, root, len * sizeof(wchar_t));
    memcpy(path + len, name, (nameLen + 1) * sizeof(wchar_t));
    f->File = CreateFileW(path, GENERIC_READ | GENERIC_WRITE | DELETE,
        FILE_SHARE_READ | FILE_SHARE_WRITE, &sa, CREATE_NEW,
        FILE_ATTRIBUTE_TEMPORARY, NULL);
    if (f->File == INVALID_HANDLE_VALUE) { *error = GetLastError(); return FALSE; }
    f->Owned = TRUE; /* Set only after this invocation wins CREATE_NEW. */
    if (!GetFileInformationByHandle(f->File, &info) ||
        !GetVolumeInformationByHandleW(f->File, NULL, 0, &actualSerial,
                                       &component, &flags, fs, 256)) goto fail;
    if (actualSerial != serial || info.dwVolumeSerialNumber != serial ||
        (info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY) || !has_probe_marker(fs)) {
        *error = ERROR_NOT_SAME_DEVICE; return FALSE;
    }
    f->Serial = serial;
    ZeroMemory(zeros, sizeof(zeros));
    if (!WriteFile(f->File, zeros, sizeof(zeros), &written, NULL)) goto fail;
    if (written != sizeof(zeros)) { *error = ERROR_WRITE_FAULT; return FALSE; }
    if (!GetFileSizeEx(f->File, &size)) goto fail;
    if (size.QuadPart != SPRIG_PROBE_FILE_BYTES) { *error = ERROR_BAD_LENGTH; return FALSE; }
    f->Section = CreateFileMappingW(f->File, &sa, PAGE_READWRITE, 0,
                                   SPRIG_PROBE_FILE_BYTES, NULL);
    if (!f->Section) goto fail;
    f->View = MapViewOfFile(f->Section, FILE_MAP_WRITE, 0, 0, SPRIG_PROBE_FILE_BYTES);
    if (!f->View) goto fail;
    /* Parent keeps its own original mapping alive too. Only child stores are
     * measured. Reading a mapped slot is not a backing-store snapshot. */
    for (i = 0; i < SPRIG_PROBE_FILE_BYTES; i += 4096) {
        volatile BYTE touched = ((volatile BYTE *)f->View)[i];
        (void)touched;
    }
    f->ControlSection = CreateFileMappingW(INVALID_HANDLE_VALUE, &sa, PAGE_READWRITE,
                                          0, CONTROL_BYTES, NULL);
    if (!f->ControlSection) goto fail;
    f->Control = (SHARED_CONTROL *)MapViewOfFile(f->ControlSection,
        FILE_MAP_ALL_ACCESS, 0, 0, CONTROL_BYTES);
    if (!f->Control) goto fail;
    ZeroMemory(f->Control, CONTROL_BYTES);
    f->Control->Magic = CONTROL_MAGIC;
    f->Control->Version = CONTROL_VERSION;
    f->Control->Bytes = sizeof(*f->Control);
    f->Control->Ordinal = f->Ordinal;
    f->Control->Nonce = nonce;
    f->Control->ParentPid = GetCurrentProcessId();
    return TRUE;
fail:
    *error = GetLastError();
    return FALSE;
}
static BOOL start_writer(FILE_FIXTURE *f, DWORD *error)
{
    wchar_t executable[32768], command[32768];
    STARTUPINFOEXW si;
    PROCESS_INFORMATION pi;
    HANDLE inherited[3];
    SIZE_T attrSize = 0;
    BOOL ok;
    DWORD count;
    ZeroMemory(&si, sizeof(si));
    ZeroMemory(&pi, sizeof(pi));
    si.StartupInfo.cb = sizeof(si);
    count = GetModuleFileNameW(NULL, executable, 32768);
    if (!count || count >= 32768) { *error = ERROR_FILENAME_EXCED_RANGE; return FALSE; }
    if (_snwprintf_s(command, 32768, _TRUNCATE,
        L"\"%ls\" --writer %lu %llu %llu %llu %llu", executable,
        (unsigned long)f->Ordinal, (unsigned long long)(ULONG_PTR)f->File,
        (unsigned long long)(ULONG_PTR)f->Section,
        (unsigned long long)(ULONG_PTR)f->ControlSection,
        (unsigned long long)f->Control->Nonce) < 0) {
        *error = ERROR_FILENAME_EXCED_RANGE; return FALSE;
    }
    InitializeProcThreadAttributeList(NULL, 1, 0, &attrSize);
    if (!attrSize) { *error = GetLastError(); return FALSE; }
    si.lpAttributeList = (LPPROC_THREAD_ATTRIBUTE_LIST)HeapAlloc(GetProcessHeap(), 0, attrSize);
    if (!si.lpAttributeList) { *error = ERROR_NOT_ENOUGH_MEMORY; return FALSE; }
    if (!InitializeProcThreadAttributeList(si.lpAttributeList, 1, 0, &attrSize)) {
        *error = GetLastError(); HeapFree(GetProcessHeap(), 0, si.lpAttributeList); return FALSE;
    }
    inherited[0] = f->File; inherited[1] = f->Section; inherited[2] = f->ControlSection;
    ok = UpdateProcThreadAttribute(si.lpAttributeList, 0,
        PROC_THREAD_ATTRIBUTE_HANDLE_LIST, inherited, sizeof(inherited), NULL, NULL);
    if (ok) ok = CreateProcessW(executable, command, NULL, NULL, TRUE,
        EXTENDED_STARTUPINFO_PRESENT | CREATE_NO_WINDOW, NULL, NULL,
        &si.StartupInfo, &pi);
    *error = ok ? ERROR_SUCCESS : GetLastError();
    DeleteProcThreadAttributeList(si.lpAttributeList);
    HeapFree(GetProcessHeap(), 0, si.lpAttributeList);
    if (!ok) return FALSE;
    f->Process = pi.hProcess;
    CloseHandle(pi.hThread);
    return TRUE;
}
static BOOL wait_started(FILE_FIXTURE *f)
{
    ULONGLONG start = GetTickCount64();
    while (!elapsed(start, READY_TIMEOUT_MS)) {
        if (load32(&gAbort) || !process_alive(f) || load32(&f->Control->ChildError)) return FALSE;
        if (load32(&f->Control->Started) && load64(&f->Control->Heartbeat) > 0 &&
            load64(&f->Control->BackgroundWrites) > 0) return TRUE;
        Sleep(1);
    }
    return FALSE;
}
static void capture_token(FILE_FIXTURE *f, TOKEN_OBSERVATION *o)
{
    o->AttemptStarted = load64(&f->Control->AttemptStarted) == o->Token;
    o->AttemptCounter = o->AttemptStarted ? load64(&f->Control->AttemptCounter) : 0;
    o->Acknowledged = load64(&f->Control->AckToken) == o->Token;
    o->AckCounter = o->Acknowledged ? load64(&f->Control->AckCounter) : 0;
    o->ObservedCounter = counter_now();
}
static TOKEN_OBSERVATION issue_token(FILE_FIXTURE *f)
{
    TOKEN_OBSERVATION o;
    ZeroMemory(&o, sizeof(o));
    o.Token = ++f->NextToken;
    o.IssuedCounter = counter_now();
    InterlockedExchange64(&f->Control->RequestToken, o.Token);
    return o;
}
static BOOL wait_ack(FILE_FIXTURE *f, TOKEN_OBSERVATION *o, DWORD timeout)
{
    ULONGLONG start = GetTickCount64();
    do {
        capture_token(f, o);
        if (o->Acknowledged && o->AttemptStarted && load64(&f->Control->Idle)) return TRUE;
        if (!process_alive(f) || load32(&f->Control->ChildError)) return FALSE;
        Sleep(1);
    } while (!elapsed(start, timeout));
    return FALSE;
}
static DWORD WINAPI ioctl_thread(void *arg)
{
    IO_CALL *io = (IO_CALL *)arg;
    io->TransportOk = DeviceIoControl(io->File, SPRIG_FSCTL_FREEZE_PROBE,
        &io->Request, sizeof(io->Request), &io->Result, sizeof(io->Result),
        &io->Bytes, NULL);
    io->Error = io->TransportOk ? ERROR_SUCCESS : GetLastError();
    return 0;
}
static BOOL start_ioctl(IO_CALL *io, FILE_FIXTURE *f, ULONG mode,
                        ULONGLONG nonce, DWORD *error)
{
    ZeroMemory(io, sizeof(*io));
    io->File = f->File;
    io->Ready = CreateEventW(NULL, TRUE, FALSE, NULL);
    if (!io->Ready) { *error = GetLastError(); return FALSE; }
    io->Release = CreateEventW(NULL, TRUE, FALSE, NULL);
    if (!io->Release) { *error = GetLastError(); return FALSE; }
    io->Request.Version = SPRIG_PROBE_VERSION;
    io->Request.Size = sizeof(io->Request);
    io->Request.Mode = mode;
    io->Request.HoldMilliseconds = SPRIG_PROBE_MAX_HOLD_MS;
    io->Request.ReadyEventHandle = (ULONGLONG)(ULONG_PTR)io->Ready;
    io->Request.ReleaseEventHandle = (ULONGLONG)(ULONG_PTR)io->Release;
    /* Exact handle used by THIS thread's DeviceIoControl, same FILE_OBJECT.
     * Never open a control device or reopen the file for the request field.
     */
    io->Request.FileHandle = (ULONGLONG)(ULONG_PTR)f->File;
    io->Request.RunNonce = nonce;
    io->Request.VolumeSerialNumber = f->Serial;
    io->Thread = CreateThread(NULL, 0, ioctl_thread, io, 0, NULL);
    if (!io->Thread) { *error = GetLastError(); return FALSE; }
    io->Submitted = TRUE;
    return TRUE;
}
static BOOL io_pending(IO_CALL *io)
{
    return io->Submitted && WaitForSingleObject(io->Thread, 0) == WAIT_TIMEOUT;
}
static BOOL wait_ready(IO_CALL *io, IO_CALL *other)
{
    ULONGLONG start = GetTickCount64();
    while (!elapsed(start, READY_TIMEOUT_MS)) {
        if (load32(&gAbort) || !io_pending(io) ||
            (other && !io_pending(other))) return FALSE;
        if (WaitForSingleObject(io->Ready, 0) == WAIT_OBJECT_0) {
            io->ReadySeen = TRUE;
            return TRUE;
        }
        Sleep(1);
    }
    return FALSE;
}
static BOOL release_and_join(IO_CALL *io)
{
    if (io->Release && io->Submitted && !io->ReleasedByParent) {
        io->ReleaseCounter = counter_now();
        io->ReleasedByParent = SetEvent(io->Release);
    }
    if (!io->Submitted) return TRUE;
    if (WaitForSingleObject(io->Thread, JOIN_TIMEOUT_MS) != WAIT_OBJECT_0) return FALSE;
    io->Joined = TRUE;
    return TRUE;
}
static BOOL envelope_valid(const IO_CALL *io)
{
    return io->Submitted && io->Joined && io->TransportOk &&
        io->Bytes == sizeof(io->Result) &&
        io->Result.Version == SPRIG_PROBE_VERSION &&
        io->Result.Size == sizeof(io->Result) &&
        io->Result.Mode == io->Request.Mode &&
        io->Result.RunNonce == io->Request.RunNonce &&
        io->Result.FileSize == SPRIG_PROBE_FILE_BYTES &&
        io->Result.VolumeSerialNumber == io->Request.VolumeSerialNumber;
}
static BOOL hold_completed(const IO_CALL *io)
{
    return envelope_valid(io) && io->ReadySeen && io->ReleasedByParent &&
        io->Result.Phase == SPRIG_PROBE_PHASE_RELEASED &&
        io->Result.ResultStatus == 0 && io->Result.WaitStatus == 0 &&
        io->Result.FlushStatus == (io->Request.Mode == SPRIG_PROBE_MODE_LOCK_ONLY ?
                                   NT_NOT_SUPPORTED : 0) &&
        io->Result.CounterFrequency == (ULONGLONG)gFrequency.QuadPart &&
        io->Result.StartCounter <= io->Result.ReadyCounter &&
        io->Result.ReadyCounter <= (ULONGLONG)io->ReleaseCounter &&
        (ULONGLONG)io->ReleaseCounter <= io->Result.EndCounter;
}
static BOOL in_hold(const IO_CALL *io, LONGLONG counter)
{
    return counter > 0 && (ULONGLONG)counter > io->Result.ReadyCounter &&
        (ULONGLONG)counter < io->Result.EndCounter;
}
static BOOL token_timing(const IO_CALL *io, const TOKEN_OBSERVATION *o)
{
    return o->Token > 0 && o->AttemptStarted &&
        in_hold(io, o->IssuedCounter) && in_hold(io, o->AttemptCounter) &&
        in_hold(io, o->ObservedCounter) &&
        o->AttemptCounter >= o->IssuedCounter &&
        (!o->Acknowledged || (in_hold(io, o->AckCounter) &&
                             o->AckCounter >= o->AttemptCounter));
}
static BOOL attempt_observation_long_enough(const TOKEN_OBSERVATION *o)
{
    return o->Acknowledged || (o->AttemptStarted &&
        o->ObservedCounter - o->AttemptCounter >=
        (gFrequency.QuadPart * MIN_ATTEMPT_OBSERVE_MS) / 1000);
}
static BOOL final_pending_timing(const IO_CALL *io, const TOKEN_OBSERVATION *o)
{
    return o->Token > 0 && o->AttemptStarted && o->Acknowledged &&
        in_hold(io, o->IssuedCounter) && in_hold(io, o->AttemptCounter) &&
        o->AttemptCounter >= o->IssuedCounter && o->AckCounter >= o->AttemptCounter;
}
static BACKGROUND_OBSERVATION capture_background(FILE_FIXTURE *f)
{
    BACKGROUND_OBSERVATION o;
    unsigned int retry;
    ZeroMemory(&o, sizeof(o));
    for (retry = 0; retry < 1000; ++retry) {
        LONG64 sequence = load64(&f->Control->BackgroundSequence);
        if (sequence & 1) { YieldProcessor(); continue; }
        o.Writes = load64(&f->Control->BackgroundWrites);
        o.AttemptCounter = load64(&f->Control->BackgroundAttemptCounter);
        o.AckCounter = load64(&f->Control->BackgroundAckCounter);
        if (sequence == load64(&f->Control->BackgroundSequence)) {
            o.ObservedCounter = counter_now();
            o.Valid = o.Writes > 0 && o.AttemptCounter > 0 &&
                      o.AckCounter >= o.AttemptCounter;
            return o;
        }
    }
    ZeroMemory(&o, sizeof(o));
    return o;
}
static BOOL background_in_hold(const IO_CALL *io, const BACKGROUND_OBSERVATION *o)
{
    return o->Valid && in_hold(io, o->AttemptCounter) &&
           in_hold(io, o->AckCounter) && in_hold(io, o->ObservedCounter);
}
static void sample_heartbeats(FILE_FIXTURE *f, LONG64 values[2])
{
    values[0] = load64(&f[0].Control->Heartbeat);
    values[1] = load64(&f[1].Control->Heartbeat);
}
static BOOL stage_a(TRIAL *t, FILE_FIXTURE *f)
{
    ULONGLONG start;
    TOKEN_OBSERVATION b;
    sample_heartbeats(f, t->HeartbeatFirstBefore);
    t->BackgroundBFirstBefore = load64(&f[1].Control->BackgroundWrites);
    t->AFirst = issue_token(&f[0]);
    b = issue_token(&f[1]);
    start = GetTickCount64();
    /* B is deliberately NOT frozen yet. Sequential commands show continued
     * progress, not a single unrelated heartbeat or stale pre-ready ack.
     */
    while (!elapsed(start, OBSERVE_MS)) {
        if (!io_pending(&t->Io[0]) || load32(&gAbort) ||
            !process_alive(&f[0]) || !process_alive(&f[1])) return FALSE;
        capture_token(&f[1], &b);
        if (b.Acknowledged && load64(&f[1].Control->Idle)) {
            if (!t->BProgress) t->BFirstAttempt = b.AttemptCounter;
            t->BLastAck = b.AckCounter;
            ++t->BProgress;
            b = issue_token(&f[1]);
        }
        Sleep(1);
    }
    /* Finish B's last command before requesting its freeze. A deliberately
     * remains outstanding if its first post-ready store is blocked. */
    if (!wait_ack(&f[1], &b, 50)) return FALSE;
    if (!t->BProgress) t->BFirstAttempt = b.AttemptCounter;
    t->BLastAck = b.AckCounter;
    ++t->BProgress;
    capture_token(&f[0], &t->AFirst);
    sample_heartbeats(f, t->HeartbeatFirstAfter);
    t->BackgroundFirst[0] = capture_background(&f[0]);
    t->BackgroundFirst[1] = capture_background(&f[1]);
    return io_pending(&t->Io[0]);
}
static BOOL stage_overlap(TRIAL *t, FILE_FIXTURE *f)
{
    ULONGLONG start;
    capture_token(&f[0], &t->AFirst);
    t->ReusedPendingA = !t->AFirst.Acknowledged;
    t->AOverlap = t->ReusedPendingA ? t->AFirst : issue_token(&f[0]);
    sample_heartbeats(f, t->HeartbeatOverlapBefore);
    t->BOverlap = issue_token(&f[1]);
    start = GetTickCount64();
    while (!elapsed(start, OBSERVE_MS)) {
        if (!io_pending(&t->Io[0]) || !io_pending(&t->Io[1]) || load32(&gAbort) ||
            !process_alive(&f[0]) || !process_alive(&f[1])) return FALSE;
        Sleep(1);
    }
    capture_token(&f[0], &t->AOverlap);
    capture_token(&f[1], &t->BOverlap);
    sample_heartbeats(f, t->HeartbeatOverlapAfter);
    t->BackgroundOverlap[0] = capture_background(&f[0]);
    t->BackgroundOverlap[1] = capture_background(&f[1]);
    return io_pending(&t->Io[0]) && io_pending(&t->Io[1]);
}
static BOOL recover_writers(TRIAL *t, FILE_FIXTURE *f)
{
    unsigned int i;
    for (i = 0; i < 2; ++i) {
        TOKEN_OBSERVATION pending;
        LONG64 backgroundBefore;
        ULONGLONG start;
        ZeroMemory(&pending, sizeof(pending));
        pending.Token = load64(&f[i].Control->RequestToken);
        if (i == 0 && pending.Token == t->AOverlap.Token)
            pending.IssuedCounter = t->AOverlap.IssuedCounter;
        else if (i == 0 && pending.Token == t->AFirst.Token)
            pending.IssuedCounter = t->AFirst.IssuedCounter;
        else if (i == 1 && pending.Token == t->BOverlap.Token)
            pending.IssuedCounter = t->BOverlap.IssuedCounter;
        if (pending.Token && !wait_ack(&f[i], &pending, ACK_TIMEOUT_MS)) return FALSE;
        t->PendingRecovered[i] = pending;
        backgroundBefore = load64(&f[i].Control->BackgroundWrites);
        start = GetTickCount64();
        while (load64(&f[i].Control->BackgroundWrites) <= backgroundBefore) {
            if (elapsed(start, ACK_TIMEOUT_MS) || !process_alive(&f[i])) return FALSE;
            Sleep(1);
        }
        t->BackgroundAfterRecovery[i] = load64(&f[i].Control->BackgroundWrites);
        t->Restored[i] = issue_token(&f[i]);
        if (!wait_ack(&f[i], &t->Restored[i], ACK_TIMEOUT_MS)) return FALSE;
        /* x64 naturally aligned 64-bit read, no CAS, readback, remap, or flush
         * used as an alleged independent backing-store oracle. */
        MemoryBarrier();
        if (*(volatile LONG64 *)f[i].View !=
            ((t->Restored[i].Token << 2) | ((LONG64)i + 1))) return FALSE;
    }
    t->BackgroundRecovered = TRUE;
    return TRUE;
}
static BOOL run_trial(TRIAL *t, FILE_FIXTURE *f, ULONG mode, ULONGLONG nonce)
{
    BOOL joinedA, joinedB;
    unsigned int i;
    ZeroMemory(t, sizeof(*t));
    t->Mode = mode;
    t->Ran = TRUE;
    t->Reason = "observation_incomplete";
    /* Every new trial starts with both writers at an acknowledged idle point.
     * This forces the measured first write to be requested AFTER A ready. */
    for (i = 0; i < 2; ++i) {
        TOKEN_OBSERVATION baseline = issue_token(&f[i]);
        if (!wait_ack(&f[i], &baseline, ACK_TIMEOUT_MS)) {
            t->Reason = "baseline_writer_failed"; goto release;
        }
    }
    t->BackgroundBeforeSubmit[0] = load64(&f[0].Control->BackgroundWrites);
    if (!start_ioctl(&t->Io[0], &f[0], mode, nonce, &t->Error)) {
        t->Reason = "a_ioctl_start_failed"; goto release;
    }
    if (!wait_ready(&t->Io[0], NULL)) {
        t->Reason = "a_ready_failed_or_timed_out"; goto release;
    }
    if (!stage_a(t, f)) { t->Reason = "a_hold_observation_failed"; goto release; }
    t->BackgroundBeforeSubmit[1] = load64(&f[1].Control->BackgroundWrites);
    if (!start_ioctl(&t->Io[1], &f[1], mode, nonce, &t->Error)) {
        t->Reason = "b_ioctl_start_failed"; goto release;
    }
    if (!wait_ready(&t->Io[1], &t->Io[0])) {
        t->Reason = "b_ready_failed_or_timed_out"; goto release;
    }
    if (!stage_overlap(t, f)) { t->Reason = "overlap_observation_failed"; goto release; }
    t->ObservationCompleted = TRUE;
release:
    /* Signal BOTH before waiting for either: do not spend B's hold waiting for
     * A's driver path. Even on an early error, release any submitted request. */
    for (i = 0; i < 2; ++i) {
        if (t->Io[i].Release && t->Io[i].Submitted) {
            t->Io[i].ReleaseCounter = counter_now();
            t->Io[i].ReleasedByParent = SetEvent(t->Io[i].Release);
        }
    }
    joinedA = release_and_join(&t->Io[0]);
    joinedB = release_and_join(&t->Io[1]);
    if (!joinedA || !joinedB) { t->Reason = "ioctl_not_quiescent"; return FALSE; }
    t->Recovered = recover_writers(t, f);
    if (!t->Recovered) { t->Reason = "writer_did_not_recover"; return FALSE; }
    t->KernelCompleted = hold_completed(&t->Io[0]) && hold_completed(&t->Io[1]);
    if (!t->ObservationCompleted) return TRUE;
    t->TimingValid = t->KernelCompleted && token_timing(&t->Io[0], &t->AFirst) &&
        token_timing(&t->Io[0], &t->AOverlap) && token_timing(&t->Io[1], &t->BOverlap) &&
        in_hold(&t->Io[0], t->BFirstAttempt) && in_hold(&t->Io[0], t->BLastAck) &&
        in_hold(&t->Io[0], t->BOverlap.ObservedCounter) &&
        in_hold(&t->Io[1], t->AOverlap.ObservedCounter) &&
        final_pending_timing(&t->Io[0], &t->PendingRecovered[0]) &&
        final_pending_timing(&t->Io[1], &t->PendingRecovered[1]);
    t->AttemptWindowsValid = attempt_observation_long_enough(&t->AFirst) &&
        attempt_observation_long_enough(&t->AOverlap) &&
        attempt_observation_long_enough(&t->BOverlap);
    t->BackgroundSamplesValid = t->BackgroundFirst[0].Valid &&
        t->BackgroundFirst[1].Valid && t->BackgroundOverlap[0].Valid &&
        t->BackgroundOverlap[1].Valid && t->BackgroundBeforeSubmit[0] > 0 &&
        t->BackgroundBeforeSubmit[1] > 0 &&
        t->BackgroundFirst[1].Writes > t->BackgroundBFirstBefore;
    t->BackgroundFalsified = t->KernelCompleted &&
        (background_in_hold(&t->Io[0], &t->BackgroundFirst[0]) ||
         background_in_hold(&t->Io[0], &t->BackgroundOverlap[0]) ||
         background_in_hold(&t->Io[1], &t->BackgroundOverlap[1]));
    t->HeartbeatsValid = TRUE;
    for (i = 0; i < 2; ++i) {
        if (t->HeartbeatFirstAfter[i] <= t->HeartbeatFirstBefore[i] ||
            t->HeartbeatOverlapAfter[i] <= t->HeartbeatOverlapBefore[i])
            t->HeartbeatsValid = FALSE;
    }
    if (!t->KernelCompleted) t->Reason = "kernel_hold_not_released_successfully";
    else if (!t->TimingValid) t->Reason = "token_timing_not_in_hold";
    else if (!t->HeartbeatsValid) t->Reason = "heartbeat_did_not_progress";
    else if (!t->AttemptWindowsValid) t->Reason = "attempt_observation_too_short";
    else if (!t->BackgroundSamplesValid || !t->BackgroundRecovered)
        t->Reason = "continuous_store_evidence_incomplete";
    else if (t->BProgress < 2) t->Reason = "unfrozen_b_insufficient_progress";
    else if (mode == SPRIG_PROBE_MODE_LOCK_ONLY) {
        t->NegativeValid = t->AFirst.Acknowledged && t->AOverlap.Acknowledged &&
                           t->BOverlap.Acknowledged;
        t->Reason = t->NegativeValid ? "negative_control_valid" : "negative_control_failed";
    } else {
        t->Falsified = t->AFirst.Acknowledged || t->AOverlap.Acknowledged ||
                       t->BOverlap.Acknowledged || t->BackgroundFalsified ||
                       in_hold(&t->Io[0], t->PendingRecovered[0].AckCounter) ||
                       in_hold(&t->Io[1], t->PendingRecovered[1].AckCounter);
        /* This is a bounded no-ack observation, NOT proof that the writer
         * instruction was executing rather than descheduled. Recovered old
         * tokens are checked too, closing the final-sample-to-release gap. */
        t->FreezeObserved = !t->Falsified;
        t->Reason = t->Falsified ? "live_store_acked_during_hold" : "no_in_hold_ack_observed";
    }
    return TRUE;
}

static const char *boolean(BOOL b) { return b ? "true" : "false"; }
static void print_token(const TOKEN_OBSERVATION *o)
{
    printf("{\"attempt_started\":%s,\"acknowledged\":%s}",
           boolean(o->AttemptStarted), boolean(o->Acknowledged));
}
static void print_io(const IO_CALL *io)
{
    /* Fixed whitelist only. Never emit filenames, command lines, tokens,
     * credentials, raw buffers, pointers, or kernel addresses. Numeric NTSTATUS
     * values are preserved even when IOCTL transport itself succeeds. */
    printf("{\"submitted\":%s,\"ready_seen\":%s,\"joined\":%s,"
           "\"transport_success\":%s,\"win32_error\":%lu,\"returned_bytes\":%lu,"
           "\"envelope_valid\":%s,\"phase\":%lu,\"result_status\":%ld,"
           "\"flush_status\":%ld,\"wait_status\":%ld,\"completed_hold\":%s}",
           boolean(io->Submitted), boolean(io->ReadySeen), boolean(io->Joined),
           boolean(io->Joined && io->TransportOk), (unsigned long)(io->Joined ? io->Error : 0),
           (unsigned long)(io->Joined ? io->Bytes : 0), boolean(envelope_valid(io)),
           (unsigned long)(io->Joined ? io->Result.Phase : 0),
           (long)(io->Joined ? io->Result.ResultStatus : 0),
           (long)(io->Joined ? io->Result.FlushStatus : 0),
           (long)(io->Joined ? io->Result.WaitStatus : 0), boolean(hold_completed(io)));
}
static void print_trial(const TRIAL *t)
{
    printf("{\"ran\":%s,\"mode\":%lu,\"reason\":\"%s\",\"win32_error\":%lu,"
           "\"observation_completed\":%s,\"timing_valid\":%s,\"heartbeats_valid\":%s,"
           "\"unfrozen_b_acks\":%lu,\"a_overlap_reuses_pending_attempt\":%s,"
           "\"writers_recovered\":%s,\"negative_control_valid\":%s,"
           "\"no_in_hold_ack_observed\":%s,\"falsified_in_this_trial\":%s,\"a_first\":",
           boolean(t->Ran), (unsigned long)t->Mode, t->Reason ? t->Reason : "not_run",
           (unsigned long)t->Error, boolean(t->ObservationCompleted), boolean(t->TimingValid),
           boolean(t->HeartbeatsValid), (unsigned long)t->BProgress, boolean(t->ReusedPendingA),
           boolean(t->Recovered), boolean(t->NegativeValid), boolean(t->FreezeObserved),
           boolean(t->Falsified));
    print_token(&t->AFirst);
    printf(",\"attempt_windows_valid\":%s,\"continuous_store_samples_valid\":%s,"
           "\"continuous_stores_recovered\":%s,\"continuous_store_counterexample\":%s,"
           "\"a_pending_ack_at_or_after_end\":%s,\"b_pending_ack_at_or_after_end\":%s",
           boolean(t->AttemptWindowsValid), boolean(t->BackgroundSamplesValid),
           boolean(t->BackgroundRecovered), boolean(t->BackgroundFalsified),
           boolean(t->KernelCompleted && t->PendingRecovered[0].Acknowledged &&
                   (ULONGLONG)t->PendingRecovered[0].AckCounter >= t->Io[0].Result.EndCounter),
           boolean(t->KernelCompleted && t->PendingRecovered[1].Acknowledged &&
                   (ULONGLONG)t->PendingRecovered[1].AckCounter >= t->Io[1].Result.EndCounter));
    printf(",\"a_overlap\":"); print_token(&t->AOverlap);
    printf(",\"b_overlap\":"); print_token(&t->BOverlap);
    printf(",\"a_ioctl\":"); print_io(&t->Io[0]);
    printf(",\"b_ioctl\":"); print_io(&t->Io[1]);
    printf("}");
}
static void print_report(const char *outcome, const char *reason, DWORD error,
                         BOOL retention, BOOL cleaned, const TRIAL *negative,
                         const TRIAL *positive)
{
    printf("{\"schema\":\"sprig.mapping-freeze.v1\",\"architecture\":\"x64\","
           "\"outcome\":\"%s\",\"reason\":\"%s\",\"win32_error\":%lu,\"root_validation_call\":\"%s\","
           "\"retention_required\":%s,\"owned_fixture_cleanup_completed\":%s,"
           "\"backing_store_snapshot_verified\":false,\"whole_tree_snapshot_verified\":false,"
           "\"negative\":", outcome, reason, (unsigned long)error, gRootValidation,
           boolean(retention), boolean(cleaned));
    print_trial(negative);
    printf(",\"coherency\":"); print_trial(positive);
    printf("}\n");
    fflush(stdout);
}
static BOOL stop_and_join_children(FILE_FIXTURE *f)
{
    BOOL safe = TRUE;
    unsigned int i;
    for (i = 0; i < 2; ++i)
        if (f[i].Control) InterlockedExchange(&f[i].Control->Stop, 1);
    for (i = 0; i < 2; ++i) {
        if (f[i].Process && WaitForSingleObject(f[i].Process, JOIN_TIMEOUT_MS) != WAIT_OBJECT_0)
            safe = FALSE;
    }
    return safe;
}
static void close_completed_ioctl(IO_CALL *io)
{
    if (io->Thread && io->Joined) { CloseHandle(io->Thread); io->Thread = NULL; }
    /* Call only after all submitted IOCTL threads are confirmed joined. */
    if (io->Ready) { CloseHandle(io->Ready); io->Ready = NULL; }
    if (io->Release) { CloseHandle(io->Release); io->Release = NULL; }
}
static BOOL clean_owned_fixture(FILE_FIXTURE *f, DWORD *error)
{
    BOOL ok = TRUE;
    FILE_DISPOSITION_INFO disposition;
    if (f->Process) { CloseHandle(f->Process); f->Process = NULL; }
    if (f->View) { if (!UnmapViewOfFile(f->View)) { *error = GetLastError(); ok = FALSE; } f->View = NULL; }
    if (f->Section) { CloseHandle(f->Section); f->Section = NULL; }
    if (f->Control) { UnmapViewOfFile(f->Control); f->Control = NULL; }
    if (f->ControlSection) { CloseHandle(f->ControlSection); f->ControlSection = NULL; }
    if (!INVALID_LOCAL_HANDLE(f->File)) {
        /* Delete by the retained owning handle, never by a potentially replaced
         * pathname. No FILE_SHARE_DELETE was granted. Existing collisions were
         * never owned and are never removed. */
        if (f->Owned && ok) {
            disposition.DeleteFile = TRUE;
            if (!SetFileInformationByHandle(f->File, FileDispositionInfo,
                                             &disposition, sizeof(disposition))) {
                *error = GetLastError(); ok = FALSE;
            }
        }
        CloseHandle(f->File); f->File = INVALID_HANDLE_VALUE;
    }
    return ok;
}
int wmain(int argc, wchar_t **argv)
{
    FILE_FIXTURE files[2];
    TRIAL negative, positive;
    wchar_t root[32768];
    DWORD serial = 0, error = ERROR_SUCCESS, cleanupError = ERROR_SUCCESS;
    ULONGLONG nonce;
    BOOL safe = TRUE, cleaned = TRUE;
    const char *reason = "setup_incomplete", *outcome = "inconclusive";
    int exitCode = 2;
    unsigned int i;
    if (argc > 1 && wcscmp(argv[1], L"--writer") == 0) return writer_main(argc, argv);
    fixture_init(&files[0], 0); fixture_init(&files[1], 1);
    ZeroMemory(&negative, sizeof(negative)); ZeroMemory(&positive, sizeof(positive));
    if (argc != 3 || wcscmp(argv[1], L"--allow-experiment") != 0) {
        print_report("not_run", "explicit_owned_ram_mount_required", ERROR_INVALID_PARAMETER,
                     FALSE, FALSE, &negative, &positive);
        return 2;
    }
    if (!QueryPerformanceFrequency(&gFrequency) || gFrequency.QuadPart <= 0) {
        print_report("not_run", "performance_counter_unavailable", GetLastError(),
                     FALSE, FALSE, &negative, &positive);
        return 2;
    }
    SetConsoleCtrlHandler(on_console, TRUE);
    if (!check_mount_root(argv[2], root, 32768, &serial, &error)) {
        reason = "owned_probe_mount_validation_failed"; goto finish;
    }
    if (!enable_existing_load_privilege(&error)) {
        reason = "elevated_admin_existing_privilege_required"; goto finish;
    }
    nonce = (ULONGLONG)counter_now() ^ ((ULONGLONG)GetCurrentProcessId() << 32);
    if (!nonce) nonce = 1;
    for (i = 0; i < 2; ++i) {
        if (!create_fixture(&files[i], root, serial, nonce, &error)) {
            reason = "exclusive_fixture_creation_failed"; goto finish;
        }
    }
    for (i = 0; i < 2; ++i) {
        if (!start_writer(&files[i], &error)) { reason = "writer_creation_failed"; goto finish; }
    }
    for (i = 0; i < 2; ++i) {
        if (!wait_started(&files[i])) { reason = "writer_startup_failed"; goto finish; }
    }
    safe = run_trial(&negative, files, SPRIG_PROBE_MODE_LOCK_ONLY, nonce);
    if (!safe) { reason = negative.Reason; goto finish; }
    if (load32(&gAbort)) { reason = "interrupted"; goto finish; }
    /* A safe but invalid negative control still permits one diagnostic
     * coherency trial. It can never yield the combined successful outcome. */
    nonce ^= 0x100000000ull;
    if (!nonce) nonce = 2;
    safe = run_trial(&positive, files, SPRIG_PROBE_MODE_COHERENCY, nonce);
    if (!safe) { reason = positive.Reason; goto finish; }
    if (!negative.NegativeValid) reason = "negative_control_invalid";
    else if (positive.Falsified) {
        outcome = "falsified"; reason = "live_store_acked_during_valid_hold"; exitCode = 1;
    } else if (positive.FreezeObserved) {
        outcome = "no_in_hold_ack_observed"; reason = "bounded_two_file_observation_only"; exitCode = 0;
    } else reason = positive.Reason;
finish:
    /* If a Cc call is stuck, do not even stop the writers' independent
     * heartbeats: retain all state for the external guest supervisor. */
    if (!safe) {
        print_report("inconclusive", reason, error, TRUE, FALSE, &negative, &positive);
        retain_forever();
    }
    if (!stop_and_join_children(files)) {
        print_report("inconclusive", "child_not_quiescent", error, TRUE, FALSE,
                     &negative, &positive);
        retain_forever();
    }
    for (i = 0; i < 2; ++i) {
        close_completed_ioctl(&negative.Io[i]);
        close_completed_ioctl(&positive.Io[i]);
    }
    for (i = 0; i < 2; ++i)
        if (!clean_owned_fixture(&files[i], &cleanupError)) cleaned = FALSE;
    if (!cleaned) { outcome = "inconclusive"; reason = "owned_fixture_cleanup_failed";
                    error = cleanupError; exitCode = 2; }
    print_report(outcome, reason, error, FALSE, cleaned, &negative, &positive);
    return exitCode;
}
