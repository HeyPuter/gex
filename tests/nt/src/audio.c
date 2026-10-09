/* Audio output through winmm (waveOut over mmdevapi): a device exists, a
 * stream opens, and a buffer is played at its own rate (0.4 s of 44.1 kHz
 * stereo takes at least 0.3 s to come back done). Native Wine (W3) only,
 * where mmdevapi's OSS driver plays into gex's /dev/dsp (a null output):
 * GEX_NT_W3=1 runs it.
 * gex-nt-cc: clang --target=ARCH-w64-mingw32 -fuse-ld=lld */
#include <windows.h>
#include <mmsystem.h>
#include <stdio.h>
static int fails;
static void ok(int c, const char *what) { printf("%s %s\n", c ? "ok" : "FAIL", what); if (!c) fails++; }
int main(void) {
    WAVEFORMATEX f = {WAVE_FORMAT_PCM, 2, 44100, 44100 * 4, 4, 16, 0};
    HWAVEOUT wo = 0;
    HANDLE done = CreateEventA(0, FALSE, FALSE, 0);
    static short pcm[44100 * 2 * 4 / 10];
    WAVEHDR h = {(LPSTR)pcm, sizeof pcm};
    DWORD t0, waited;
    int i;
    for (i = 0; i < (int)(sizeof pcm / sizeof *pcm); i++) pcm[i] = (short)((i % 100) * 300 - 15000);
    ok(waveOutGetNumDevs() >= 1, "an output device");
    ok(waveOutOpen(&wo, WAVE_MAPPER, &f, (DWORD_PTR)done, 0, CALLBACK_EVENT) == MMSYSERR_NOERROR, "waveOutOpen 44.1 kHz stereo");
    if (!wo) { printf("%d failures\n", fails); return 1; }
    WaitForSingleObject(done, 1000); /* WOM_OPEN */
    ok(waveOutPrepareHeader(wo, &h, sizeof h) == MMSYSERR_NOERROR, "waveOutPrepareHeader");
    t0 = GetTickCount();
    ok(waveOutWrite(wo, &h, sizeof h) == MMSYSERR_NOERROR, "waveOutWrite");
    while (!(h.dwFlags & WHDR_DONE) && GetTickCount() - t0 < 10000) WaitForSingleObject(done, 100);
    waited = GetTickCount() - t0;
    ok(h.dwFlags & WHDR_DONE, "the buffer comes back done");
    ok(waited >= 300, "played at its rate, not at once");
    waveOutUnprepareHeader(wo, &h, sizeof h);
    ok(waveOutClose(wo) == MMSYSERR_NOERROR, "waveOutClose");
    printf("%d failures\n", fails);
    return fails != 0;
}
