/* SPDX-License-Identifier: MIT
 * Original, test-only ABI. This is not a supported WinFsp or Sprig API.
 * Include after Windows.h/winioctl.h (user) or sys/driver.h (kernel).
 */
#ifndef SPRIG_FREEZE_PROBE_PROTOCOL_H
#define SPRIG_FREEZE_PROBE_PROTOCOL_H

#define SPRIG_PROBE_VERSION 1u
#define SPRIG_PROBE_FILE_BYTES 65536u
#define SPRIG_PROBE_MAX_HOLD_MS 1000u
#define SPRIG_PROBE_DEFAULT_HOLD_MS 250u
#define SPRIG_PROBE_FS_NAME L"-SPRIGPROBE"
#define SPRIG_PROBE_FILE_A L"sprig-freeze-A.bin"
#define SPRIG_PROBE_FILE_B L"sprig-freeze-B.bin"
#define SPRIG_PROBE_MODE_LOCK_ONLY 1u
#define SPRIG_PROBE_MODE_COHERENCY 2u
#define SPRIG_PROBE_PHASE_VALIDATED 1u
#define SPRIG_PROBE_PHASE_LOCKED 2u
#define SPRIG_PROBE_PHASE_READY 3u
#define SPRIG_PROBE_PHASE_RELEASED 4u
#define SPRIG_PROBE_PHASE_REFUSED 5u
#define SPRIG_PROBE_PHASE_EXPIRED 6u
#define SPRIG_PROBE_PHASE_CANCELLED 7u

/* Private function range, METHOD_BUFFERED, writable handle required. */
#define SPRIG_FSCTL_FREEZE_PROBE \
    CTL_CODE(FILE_DEVICE_FILE_SYSTEM, 0x9e0, METHOD_BUFFERED, FILE_WRITE_ACCESS)

/* Fixed x64 ABI, no user pointers. Each handle is validated in UserMode. */
typedef struct SPRIG_PROBE_REQUEST {
    ULONG Version;
    ULONG Size;
    ULONG Mode;
    ULONG HoldMilliseconds;
    ULONGLONG ReadyEventHandle;
    ULONGLONG ReleaseEventHandle;
    ULONGLONG FileHandle;
    ULONGLONG RunNonce;
    ULONG VolumeSerialNumber;
    ULONG Reserved;
} SPRIG_PROBE_REQUEST;

/* ResultStatus is the probe result; FlushStatus is the unmodified Cc status.
 * The IOCTL transport succeeds after a validated request so this diagnostic
 * structure survives negative NTSTATUS values. Only Phase=RELEASED AND both
 * ResultStatus/WaitStatus=STATUS_SUCCESS constitute a completed hold.
 * LOCK_ONLY has no Cc call: FlushStatus=STATUS_NOT_SUPPORTED, never "success".
 * EndCounter is sampled BEFORE the original owner unlocks, a conservative
 * end of the hold window. Timestamps are not a data durability claim.
 */
typedef struct SPRIG_PROBE_RESULT {
    ULONG Version;
    ULONG Size;
    ULONG Mode;
    ULONG Phase;
    ULONGLONG RunNonce;
    LONG ResultStatus;
    LONG FlushStatus;
    LONG WaitStatus;
    ULONG Reserved;
    ULONGLONG StartCounter;
    ULONGLONG ReadyCounter;
    ULONGLONG EndCounter;
    ULONGLONG CounterFrequency;
    ULONGLONG FileSize;
    ULONG VolumeSerialNumber;
    ULONG Reserved2;
} SPRIG_PROBE_RESULT;

#endif
