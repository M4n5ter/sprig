/* SPDX-License-Identifier: GPL-3.0-only
 * Experimental addition to WinFsp ddca7bd5481857a65ba552f643b8776fd070836f.
 * This helper is part of the modified WinFsp driver, NOT the MIT Sprig product.
 * It is deliberately absent unless FSP_TEST_FREEZE_PROBE is defined.
 * No epoch publication, persistent storage or production freeze API is provided.
 */
#ifndef FSP_SPRIG_FREEZE_PROBE_H
#define FSP_SPRIG_FREEZE_PROBE_H

#include "../../../probe_protocol.h"

#if !defined(_WIN64) || defined(FSP_FILE_NODE_NO_PGIO)
#error The freeze experiment requires x64 and the real PagingIoResource.
#endif

FSP_FSCTL_STATIC_ASSERT(56 == sizeof(SPRIG_PROBE_REQUEST), "probe request ABI");
FSP_FSCTL_STATIC_ASSERT(88 == sizeof(SPRIG_PROBE_RESULT), "probe result ABI");

static NTSTATUS FspSprigFreezeProbe(
    PDEVICE_OBJECT FsvolDeviceObject, PIRP Irp, PIO_STACK_LOCATION IrpSp)
{
    SPRIG_PROBE_REQUEST Request;
    SPRIG_PROBE_RESULT Output;
    FSP_FILE_NODE *FileNode = 0;
    PFILE_OBJECT FileObject = 0;
    PKEVENT ReadyEvent = 0, ReleaseEvent = 0;
    BOOLEAN Acquired = FALSE;
    NTSTATUS Result;
    IO_STATUS_BLOCK FlushStatus;
    LARGE_INTEGER CounterFrequency, Deadline, Slice;
    UNICODE_STRING NameA = RTL_CONSTANT_STRING(L"\\" SPRIG_PROBE_FILE_A);
    UNICODE_STRING NameB = RTL_CONSTANT_STRING(L"\\" SPRIG_PROBE_FILE_B);
    FSP_FSVOL_DEVICE_EXTENSION *Volume = FspFsvolDeviceExtension(FsvolDeviceObject);

    /* The major dispatch has entered FsRtlEnterFileSystem and installed this
     * exact per-file IRP as TopLevelIrp. Do NOT queue to a foreign process or
     * acquire another FileNode under that IRP's resource flags. */
    Irp->IoStatus.Information = 0;
    if (PASSIVE_LEVEL != KeGetCurrentIrql() || UserMode != Irp->RequestorMode ||
        IoGetRequestorProcess(Irp) != PsGetCurrentProcess() ||
        IoGetTopLevelIrp() != Irp || 0 != FspIrpTopFlags(Irp) ||
        0 != FspIrpFlags(Irp) || 0 == IrpSp->FileObject)
        return STATUS_INVALID_DEVICE_STATE;
    if (IoIs32bitProcess(Irp))
        return STATUS_NOT_SUPPORTED;
    /* Same privilege boundary as WinFsp's privileged unload request. UserMode
     * is essential: KernelMode would make SeSinglePrivilegeCheck always pass.
     * The controller must already possess and explicitly enable this privilege. */
    if (!SeSinglePrivilegeCheck(RtlConvertLongToLuid(SE_LOAD_DRIVER_PRIVILEGE), UserMode))
        return STATUS_PRIVILEGE_NOT_HELD;
    if (sizeof Request != IrpSp->Parameters.FileSystemControl.InputBufferLength ||
        sizeof Output != IrpSp->Parameters.FileSystemControl.OutputBufferLength ||
        0 == Irp->AssociatedIrp.SystemBuffer)
        return STATUS_INVALID_PARAMETER;
    RtlCopyMemory(&Request, Irp->AssociatedIrp.SystemBuffer, sizeof Request);
    if (SPRIG_PROBE_VERSION != Request.Version || sizeof Request != Request.Size ||
        (SPRIG_PROBE_MODE_LOCK_ONLY != Request.Mode && SPRIG_PROBE_MODE_COHERENCY != Request.Mode) ||
        0 == Request.HoldMilliseconds || SPRIG_PROBE_MAX_HOLD_MS < Request.HoldMilliseconds ||
        0 != Request.Reserved || 0 == Request.RunNonce ||
        0 >= (LONG_PTR)Request.ReadyEventHandle || 0 >= (LONG_PTR)Request.ReleaseEventHandle ||
        0 >= (LONG_PTR)Request.FileHandle)
        return STATUS_INVALID_PARAMETER;
    /* The kernel's immutable volume parameters, not a user-provided path,
     * identify an explicitly named disposable experiment volume. This is a
     * misuse guard, not a claim to prove that a backend is RAM-backed. */
    if (0 != Volume->VolumeParams.Prefix[0] ||
        Request.VolumeSerialNumber != Volume->VolumeParams.VolumeSerialNumber ||
        !RtlEqualMemory(Volume->VolumeParams.FileSystemName,
            SPRIG_PROBE_FS_NAME, sizeof SPRIG_PROBE_FS_NAME))
        return STATUS_INVALID_DEVICE_REQUEST;

    RtlZeroMemory(&Output, sizeof Output);
    Output.Version = SPRIG_PROBE_VERSION;
    Output.Size = sizeof Output;
    Output.Mode = Request.Mode;
    Output.Phase = SPRIG_PROBE_PHASE_VALIDATED;
    Output.RunNonce = Request.RunNonce;
    Output.VolumeSerialNumber = Request.VolumeSerialNumber;
    Output.ResultStatus = STATUS_UNSUCCESSFUL;
    Output.FlushStatus = STATUS_NOT_SUPPORTED;
    Output.WaitStatus = STATUS_UNSUCCESSFUL;
    Output.StartCounter = KeQueryPerformanceCounter(&CounterFrequency).QuadPart;
    Output.CounterFrequency = CounterFrequency.QuadPart;

    /* Every reference uses the caller's actual rights and the expected object
     * type. The IRP retains its file object too; this explicit reference also
     * checks that the supplied writable handle is that very same FILE_OBJECT. */
    try
    {
        try
        {
            Result = ObReferenceObjectByHandle((HANDLE)(ULONG_PTR)Request.FileHandle,
                FILE_READ_DATA | FILE_WRITE_DATA, *IoFileObjectType, UserMode,
                (PVOID *)&FileObject, 0);
            if (STATUS_SUCCESS != Result)
                __leave;
            if (FileObject != IrpSp->FileObject || !FileObject->WriteAccess ||
                !FspFileNodeIsValid(FileObject->FsContext))
            {
                Result = STATUS_INVALID_HANDLE;
                __leave;
            }
            FileNode = FileObject->FsContext;
            if (FileNode->FsvolDeviceObject != FsvolDeviceObject ||
                FileNode->IsDirectory || 0 != FileNode->MainFileNode)
            {
                Result = STATUS_INVALID_DEVICE_REQUEST;
                __leave;
            }
            Result = ObReferenceObjectByHandle((HANDLE)(ULONG_PTR)Request.ReadyEventHandle,
                EVENT_MODIFY_STATE, *ExEventObjectType, UserMode, (PVOID *)&ReadyEvent, 0);
            if (STATUS_SUCCESS != Result)
                __leave;
            Result = ObReferenceObjectByHandle((HANDLE)(ULONG_PTR)Request.ReleaseEventHandle,
                SYNCHRONIZE, *ExEventObjectType, UserMode, (PVOID *)&ReleaseEvent, 0);
            if (STATUS_SUCCESS != Result)
                __leave;
            if (ReadyEvent == ReleaseEvent || 0 != KeReadStateEvent(ReadyEvent) ||
                0 != KeReadStateEvent(ReleaseEvent))
            {
                Result = STATUS_INVALID_PARAMETER;
                __leave;
            }
            /* No unbounded wait merely to acquire the resources. TryFull
             * acquires Main then Paging and unwinds Main if Paging is busy. */
            if (!FspFileNodeTryAcquireExclusive(FileNode, Full))
            {
                Result = STATUS_DEVICE_BUSY;
                __leave;
            }
            Acquired = TRUE;
            Output.Phase = SPRIG_PROBE_PHASE_LOCKED;
            if (!ExIsResourceAcquiredExclusiveLite(FileNode->Header.Resource) ||
                !ExIsResourceAcquiredExclusiveLite(FileNode->Header.PagingIoResource))
            {
                Result = STATUS_INTERNAL_ERROR;
                __leave;
            }
            Output.FileSize = FileNode->Header.FileSize.QuadPart;
            if (SPRIG_PROBE_FILE_BYTES != Output.FileSize ||
                0 == FileNode->NonPaged->SectionObjectPointers.DataSectionObject ||
                (!RtlEqualUnicodeString(&FileNode->FileName, &NameA, FALSE) &&
                 !RtlEqualUnicodeString(&FileNode->FileName, &NameB, FALSE)))
            {
                Result = STATUS_INVALID_PARAMETER;
                __leave;
            }
            if (Irp->Cancel || 0 != KeReadStateEvent(ReleaseEvent))
            {
                Result = STATUS_CANCELLED;
                __leave;
            }
            if (SPRIG_PROBE_MODE_COHERENCY == Request.Mode)
            {
                if (0 == FspMvCcCoherencyFlushAndPurgeCache)
                {
                    Result = STATUS_NOT_SUPPORTED;
                    __leave;
                }
                FlushStatus.Status = STATUS_UNSUCCESSFUL;
                FlushStatus.Information = 0;
                /* Synchronous and not safely cancellable. If this call hangs,
                 * only the isolated guest supervisor can diagnose it. Never
                 * pretend a timeout has cancelled Cc or drop locks elsewhere. */
                FspMvCcCoherencyFlushAndPurgeCache(
                    &FileNode->NonPaged->SectionObjectPointers, 0, 0, &FlushStatus, 0);
                Output.FlushStatus = FlushStatus.Status;
                if (STATUS_SUCCESS != FlushStatus.Status)
                {
                    /* Includes informational STATUS_CACHE_PAGE_LOCKED. */
                    Result = FlushStatus.Status;
                    __leave;
                }
            }
            if (Irp->Cancel || 0 != KeReadStateEvent(ReleaseEvent))
            {
                Result = STATUS_CANCELLED;
                __leave;
            }
            /* The limit bounds only this observation hold, not Cc's preceding
             * synchronous work. A 10-ms cancellation poll is not a cancel
             * routine and never releases another thread's ERESOURCE. */
            Deadline.QuadPart = KeQueryInterruptTime() +
                (ULONGLONG)Request.HoldMilliseconds * 10000;
            Output.ReadyCounter = KeQueryPerformanceCounter(0).QuadPart;
            Output.Phase = SPRIG_PROBE_PHASE_READY;
            KeSetEvent(ReadyEvent, IO_NO_INCREMENT, FALSE);
            for (;;)
            {
                ULONGLONG Now = KeQueryInterruptTime();
                if (Irp->Cancel)
                {
                    Result = STATUS_CANCELLED;
                    Output.Phase = SPRIG_PROBE_PHASE_CANCELLED;
                    break;
                }
                if (Now >= (ULONGLONG)Deadline.QuadPart)
                {
                    Result = STATUS_TIMEOUT;
                    Output.WaitStatus = STATUS_TIMEOUT;
                    Output.Phase = SPRIG_PROBE_PHASE_EXPIRED;
                    break;
                }
                Slice.QuadPart = -(LONGLONG)min((ULONGLONG)100000,
                    (ULONGLONG)Deadline.QuadPart - Now);
                Result = KeWaitForSingleObject(ReleaseEvent, Executive, KernelMode, FALSE, &Slice);
                Output.WaitStatus = Result;
                if (STATUS_SUCCESS == Result)
                {
                    if (Irp->Cancel)
                    {
                        Result = STATUS_CANCELLED;
                        Output.Phase = SPRIG_PROBE_PHASE_CANCELLED;
                        break;
                    }
                    if (KeQueryInterruptTime() >= (ULONGLONG)Deadline.QuadPart)
                    {
                        Result = STATUS_TIMEOUT;
                        Output.WaitStatus = STATUS_TIMEOUT;
                        Output.Phase = SPRIG_PROBE_PHASE_EXPIRED;
                        break;
                    }
                    Output.Phase = SPRIG_PROBE_PHASE_RELEASED;
                    break;
                }
                if (STATUS_TIMEOUT != Result)
                    break;
            }
        }
        except (EXCEPTION_EXECUTE_HANDLER)
        {
            Result = GetExceptionCode();
            /* An exception is never normalized into a usable frozen state. */
        }
    }
    finally
    {
        /* Conservative end of the measured hold: sampled BEFORE unlocking.
         * An ack timestamp below this value was definitely not after release. */
        Output.EndCounter = KeQueryPerformanceCounter(0).QuadPart;
        if (Acquired)
            FspFileNodeRelease(FileNode, Full); /* original owner, reverse order */
        if (0 != ReleaseEvent)
            ObDereferenceObject(ReleaseEvent);
        if (0 != ReadyEvent)
            ObDereferenceObject(ReadyEvent);
        if (0 != FileObject)
            ObDereferenceObject(FileObject);
    }
    Output.ResultStatus = Result;
    if (SPRIG_PROBE_PHASE_RELEASED != Output.Phase &&
        SPRIG_PROBE_PHASE_EXPIRED != Output.Phase &&
        SPRIG_PROBE_PHASE_CANCELLED != Output.Phase)
        Output.Phase = STATUS_CANCELLED == Result ?
            SPRIG_PROBE_PHASE_CANCELLED : SPRIG_PROBE_PHASE_REFUSED;
    RtlCopyMemory(Irp->AssociatedIrp.SystemBuffer, &Output, sizeof Output);
    Irp->IoStatus.Information = sizeof Output;
    /* Transport success preserves negative diagnostic statuses in the result.
     * It is emphatically NOT a successful freeze; the controller must inspect
     * the result, hold window and writer observations independently. */
    return STATUS_SUCCESS;
}

#endif
