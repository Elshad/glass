/*++

Module Name:

    loopback.h

Abstract:

    Glass Microphone's loopback: what's played into the "Glass Microphone feed" render
    endpoint comes out of the "Glass Microphone" capture endpoint. Glass plays a viewer's
    microphone into the feed; apps record it from the microphone.

    Audio is kept as 48 kHz stereo 32-bit integer frames, converted from and to whatever
    each stream uses (8/16/24/32-bit PCM or 32-bit float) with integer math only: kernel
    code doesn't touch the FPU.

--*/

#pragma once

#include "definitions.h"

NTSTATUS LoopbackInitialize();
VOID LoopbackCleanup();

// The render stream's data, `bytes` of it in `format`, into the loopback.
VOID LoopbackWrite(_In_reads_bytes_(bytes) const BYTE* data, _In_ ULONG bytes, _In_ const WAVEFORMATEXTENSIBLE* format);

// The capture stream's data, `bytes` of it in `format`, from the loopback (silence when
// nothing's been played).
VOID LoopbackRead(_Out_writes_bytes_(bytes) BYTE* data, _In_ ULONG bytes, _In_ const WAVEFORMATEXTENSIBLE* format);
