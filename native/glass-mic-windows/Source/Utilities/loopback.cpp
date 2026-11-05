/*++

Module Name:

    loopback.cpp

Abstract:

    The ring buffer between the feed (render) and the microphone (capture). See loopback.h.

--*/

#include "loopback.h"

#define LOOPBACK_POOLTAG 'bLmG'

// One second of frames; at most MAX_FILL are kept, so a microphone opened late doesn't
// start with stale audio, and the delay stays short.
#define CAPACITY_FRAMES 48000
#define MAX_FILL_FRAMES 9600 // 200 ms

typedef struct _FRAME
{
    LONG Left;
    LONG Right;
} FRAME;

static FRAME* g_Ring = NULL;
static ULONG g_Read = 0;  // next frame to read
static ULONG g_Fill = 0;  // frames waiting
static KSPIN_LOCK g_Lock;

NTSTATUS LoopbackInitialize()
{
    KeInitializeSpinLock(&g_Lock);
    g_Ring = (FRAME*)ExAllocatePool2(POOL_FLAG_NON_PAGED, CAPACITY_FRAMES * sizeof(FRAME), LOOPBACK_POOLTAG);
    if (g_Ring == NULL)
    {
        return STATUS_INSUFFICIENT_RESOURCES;
    }
    g_Read = 0;
    g_Fill = 0;
    return STATUS_SUCCESS;
}

VOID LoopbackCleanup()
{
    if (g_Ring != NULL)
    {
        ExFreePoolWithTag(g_Ring, LOOPBACK_POOLTAG);
        g_Ring = NULL;
    }
}

// ---- Formats ---------------------------------------------------------------------------

typedef struct _LAYOUT
{
    ULONG Channels;
    ULONG BytesPerSample; // container size
    BOOLEAN Float;
} LAYOUT;

static BOOLEAN LayoutOf(_In_ const WAVEFORMATEXTENSIBLE* format, _Out_ LAYOUT* layout)
{
    const WAVEFORMATEX* wf = &format->Format;
    if (wf->nChannels == 0 || wf->nBlockAlign == 0 || wf->nBlockAlign % wf->nChannels != 0)
    {
        return FALSE;
    }
    layout->Channels = wf->nChannels;
    layout->BytesPerSample = wf->nBlockAlign / wf->nChannels;
    layout->Float = wf->wFormatTag == WAVE_FORMAT_IEEE_FLOAT ||
        (wf->wFormatTag == WAVE_FORMAT_EXTENSIBLE && IsEqualGUIDAligned(format->SubFormat, KSDATAFORMAT_SUBTYPE_IEEE_FLOAT));
    if (layout->Float && layout->BytesPerSample != 4)
    {
        return FALSE;
    }
    return layout->BytesPerSample >= 1 && layout->BytesPerSample <= 4;
}

// An IEEE single (as its bits) to a 32-bit fixed-point sample (full scale = 1.0), clamped.
static LONG FloatBitsToQ31(ULONG bits)
{
    ULONG exponent = (bits >> 23) & 0xFF;
    if (exponent == 0)
    {
        return 0; // zero, or too small to matter
    }
    BOOLEAN negative = (bits >> 31) != 0;
    if (exponent >= 127)
    {
        return negative ? MINLONG : MAXLONG; // |x| >= 1.0
    }
    ULONG mantissa = (bits & 0x7FFFFF) | 0x800000; // 24 bits
    LONG shift = (LONG)exponent - 119;             // value * 2^31 = mantissa * 2^(exponent - 119)
    ULONG magnitude;
    if (shift >= 0)
    {
        magnitude = mantissa << shift; // shift <= 7: fits in 31 bits
    }
    else if (shift > -32)
    {
        magnitude = mantissa >> (-shift);
    }
    else
    {
        magnitude = 0;
    }
    return negative ? -(LONG)magnitude : (LONG)magnitude;
}

// The reverse: a fixed-point sample to an IEEE single's bits.
static ULONG Q31ToFloatBits(LONG q)
{
    if (q == 0)
    {
        return 0;
    }
    ULONG sign = q < 0 ? 0x80000000u : 0;
    ULONGLONG magnitude = q < 0 ? (ULONGLONG)(-(LONGLONG)q) : (ULONGLONG)q; // up to 2^31
    ULONG top = 0; // highest set bit
    for (ULONGLONG m = magnitude; m > 1; m >>= 1)
    {
        top++;
    }
    ULONG exponent = top + 96; // value = magnitude / 2^31, so exponent = top - 31 + 127
    ULONG mantissa = top <= 23 ? (ULONG)(magnitude << (23 - top)) : (ULONG)(magnitude >> (top - 23));
    return sign | (exponent << 23) | (mantissa & 0x7FFFFF);
}

static LONG ReadSample(_In_ const BYTE* p, _In_ const LAYOUT* layout)
{
    switch (layout->BytesPerSample)
    {
    case 1:
        return ((LONG)p[0] - 128) << 24;
    case 2:
        return (LONG)(SHORT)(p[0] | (p[1] << 8)) << 16;
    case 3:
        return (LONG)(((ULONG)p[0] << 8) | ((ULONG)p[1] << 16) | ((ULONG)p[2] << 24));
    default:
    {
        ULONG v = (ULONG)p[0] | ((ULONG)p[1] << 8) | ((ULONG)p[2] << 16) | ((ULONG)p[3] << 24);
        return layout->Float ? FloatBitsToQ31(v) : (LONG)v;
    }
    }
}

static VOID WriteSample(_Out_ BYTE* p, _In_ LONG q, _In_ const LAYOUT* layout)
{
    switch (layout->BytesPerSample)
    {
    case 1:
        p[0] = (BYTE)((q >> 24) + 128);
        break;
    case 2:
        p[0] = (BYTE)(q >> 16);
        p[1] = (BYTE)(q >> 24);
        break;
    case 3:
        p[0] = (BYTE)(q >> 8);
        p[1] = (BYTE)(q >> 16);
        p[2] = (BYTE)(q >> 24);
        break;
    default:
    {
        ULONG v = layout->Float ? Q31ToFloatBits(q) : (ULONG)q;
        p[0] = (BYTE)v;
        p[1] = (BYTE)(v >> 8);
        p[2] = (BYTE)(v >> 16);
        p[3] = (BYTE)(v >> 24);
        break;
    }
    }
}

// ---- The ring --------------------------------------------------------------------------

VOID LoopbackWrite(_In_reads_bytes_(bytes) const BYTE* data, _In_ ULONG bytes, _In_ const WAVEFORMATEXTENSIBLE* format)
{
    LAYOUT layout;
    if (g_Ring == NULL || !LayoutOf(format, &layout))
    {
        return;
    }
    ULONG frameBytes = layout.Channels * layout.BytesPerSample;
    ULONG frames = bytes / frameBytes;

    KIRQL irql;
    KeAcquireSpinLock(&g_Lock, &irql);
    for (ULONG i = 0; i < frames; i++)
    {
        const BYTE* p = data + (SIZE_T)i * frameBytes;
        FRAME f;
        f.Left = ReadSample(p, &layout);
        f.Right = layout.Channels > 1 ? ReadSample(p + layout.BytesPerSample, &layout) : f.Left;
        if (g_Fill == MAX_FILL_FRAMES)
        {
            // Full: the oldest frame goes.
            g_Read = (g_Read + 1) % CAPACITY_FRAMES;
            g_Fill--;
        }
        g_Ring[(g_Read + g_Fill) % CAPACITY_FRAMES] = f;
        g_Fill++;
    }
    KeReleaseSpinLock(&g_Lock, irql);
}

VOID LoopbackRead(_Out_writes_bytes_(bytes) BYTE* data, _In_ ULONG bytes, _In_ const WAVEFORMATEXTENSIBLE* format)
{
    LAYOUT layout;
    if (g_Ring == NULL || !LayoutOf(format, &layout))
    {
        RtlZeroMemory(data, bytes);
        return;
    }
    ULONG frameBytes = layout.Channels * layout.BytesPerSample;
    ULONG frames = bytes / frameBytes;

    KIRQL irql;
    KeAcquireSpinLock(&g_Lock, &irql);
    for (ULONG i = 0; i < frames; i++)
    {
        FRAME f = { 0, 0 };
        if (g_Fill > 0)
        {
            f = g_Ring[g_Read];
            g_Read = (g_Read + 1) % CAPACITY_FRAMES;
            g_Fill--;
        }
        BYTE* p = data + (SIZE_T)i * frameBytes;
        if (layout.Channels == 1)
        {
            WriteSample(p, (LONG)(((LONGLONG)f.Left + f.Right) / 2), &layout);
        }
        else
        {
            WriteSample(p, f.Left, &layout);
            WriteSample(p + layout.BytesPerSample, f.Right, &layout);
            for (ULONG c = 2; c < layout.Channels; c++)
            {
                WriteSample(p + c * layout.BytesPerSample, 0, &layout);
            }
        }
    }
    KeReleaseSpinLock(&g_Lock, irql);

    // A partial frame at the end (shouldn't happen) is silence.
    if (frames * frameBytes < bytes)
    {
        RtlZeroMemory(data + frames * frameBytes, bytes - frames * frameBytes);
    }
}
