/* An audio device for a 32-bit Wine program (test/wow64/audio): winmm through
 * mmdevapi to wineoss.drv, a pure Unix library in 32-bit Wine's loader, against
 * gex-kernel's /dev/dsp (a null output). Prints "audio ok N" when a device
 * is there and a 16-bit stereo stream opens on it. */
#include <windows.h>
#include <mmsystem.h>
#include <stdio.h>
int main(void)
{
    WAVEFORMATEX f = { WAVE_FORMAT_PCM, 2, 44100, 44100 * 4, 4, 16, 0 };
    HWAVEOUT h;
    UINT n = waveOutGetNumDevs();
    MMRESULT r = n ? waveOutOpen(&h, WAVE_MAPPER, &f, 0, 0, CALLBACK_NULL) : MMSYSERR_NODRIVER;
    printf("waveOutGetNumDevs %u, waveOutOpen %u\n", n, r);
    if (!r) waveOutClose(h);
    if (n && !r) printf("audio ok %u\n", n);
    return 0;
}
