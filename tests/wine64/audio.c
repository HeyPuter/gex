/* An audio device for an x64 program on W64 (tests/wine64/nt-x64.sh, case
 * audio): winmm's waveOut and mmdevapi's endpoint, both through
 * wineoss.drv (a pure Unix library in W64's loader, link-w64.sh) to
 * gex-kernel's /dev/dsp. Without the driver mmdevapi found none ("No driver
 * from pulse,alsa,oss,coreaudio could be initialized") and an x64 game's
 * OpenAL device failed to open (OneShot). Plays 100 ms of a tone each way:
 * waveOut until the buffer is done, and an IAudioClient stream (shared
 * mode) until its padding drains. Prints "waveout ok" and "mmdevapi ok",
 * or what failed. */
#define COBJMACROS
#include <initguid.h>
#include <windows.h>
#include <mmsystem.h>
#include <mmdeviceapi.h>
#include <audioclient.h>
#include <stdio.h>

static void tone(short *s, unsigned frames, unsigned channels)
{
    unsigned i, c;
    for (i = 0; i < frames; i++)
        for (c = 0; c < channels; c++)
            s[i * channels + c] = (short)(((i / 50) & 1) ? 4000 : -4000);
}

static int waveout(void)
{
    WAVEFORMATEX f = { WAVE_FORMAT_PCM, 2, 44100, 44100 * 4, 4, 16, 0 };
    static short buf[4410 * 2];
    WAVEHDR h = { (char *)buf, sizeof(buf) };
    HWAVEOUT w;
    UINT n = waveOutGetNumDevs();
    MMRESULT r;
    int t;

    if (!n) { printf("waveOutGetNumDevs 0\n"); return 1; }
    if ((r = waveOutOpen(&w, WAVE_MAPPER, &f, 0, 0, CALLBACK_NULL))) { printf("waveOutOpen %u\n", r); return 1; }
    tone(buf, 4410, 2);
    if ((r = waveOutPrepareHeader(w, &h, sizeof(h)))) { printf("waveOutPrepareHeader %u\n", r); return 1; }
    if ((r = waveOutWrite(w, &h, sizeof(h)))) { printf("waveOutWrite %u\n", r); return 1; }
    for (t = 0; t < 500 && !(h.dwFlags & WHDR_DONE); t++) Sleep(10);
    if (!(h.dwFlags & WHDR_DONE)) { printf("waveOut: the buffer not done in 5 s\n"); return 1; }
    waveOutUnprepareHeader(w, &h, sizeof(h));
    waveOutClose(w);
    printf("waveout ok\n");
    return 0;
}

static int mmdevapi(void)
{
    IMMDeviceEnumerator *e;
    IMMDevice *d;
    IAudioClient *c;
    IAudioRenderClient *rc;
    WAVEFORMATEX *mix;
    UINT32 frames, pad = 0, want;
    BYTE *data;
    HRESULT hr;
    int t;

    CoInitializeEx(NULL, COINIT_MULTITHREADED);
    if ((hr = CoCreateInstance(&CLSID_MMDeviceEnumerator, NULL, CLSCTX_ALL, &IID_IMMDeviceEnumerator, (void **)&e)))
        { printf("MMDeviceEnumerator %08lx\n", hr); return 1; }
    if ((hr = IMMDeviceEnumerator_GetDefaultAudioEndpoint(e, eRender, eConsole, &d)))
        { printf("GetDefaultAudioEndpoint %08lx\n", hr); return 1; }
    if ((hr = IMMDevice_Activate(d, &IID_IAudioClient, CLSCTX_ALL, NULL, (void **)&c)))
        { printf("Activate %08lx\n", hr); return 1; }
    if ((hr = IAudioClient_GetMixFormat(c, &mix))) { printf("GetMixFormat %08lx\n", hr); return 1; }
    if ((hr = IAudioClient_Initialize(c, AUDCLNT_SHAREMODE_SHARED, 0, 2000000, 0, mix, NULL)))
        { printf("Initialize %08lx\n", hr); return 1; }
    if ((hr = IAudioClient_GetBufferSize(c, &frames))) { printf("GetBufferSize %08lx\n", hr); return 1; }
    if ((hr = IAudioClient_GetService(c, &IID_IAudioRenderClient, (void **)&rc))) { printf("GetService %08lx\n", hr); return 1; }
    want = mix->nSamplesPerSec / 10;
    if (want > frames) want = frames;
    if ((hr = IAudioRenderClient_GetBuffer(rc, want, &data))) { printf("GetBuffer %08lx\n", hr); return 1; }
    /* silence: the mix format is float or PCM, whichever the driver says */
    memset(data, 0, (size_t)want * mix->nBlockAlign);
    if ((hr = IAudioRenderClient_ReleaseBuffer(rc, want, 0))) { printf("ReleaseBuffer %08lx\n", hr); return 1; }
    if ((hr = IAudioClient_Start(c))) { printf("Start %08lx\n", hr); return 1; }
    for (t = 0; t < 500; t++) {
        if ((hr = IAudioClient_GetCurrentPadding(c, &pad))) { printf("GetCurrentPadding %08lx\n", hr); return 1; }
        if (!pad) break;
        Sleep(10);
    }
    if (pad) { printf("mmdevapi: %u frames still queued after 5 s\n", pad); return 1; }
    IAudioClient_Stop(c);
    CoTaskMemFree(mix);
    IAudioRenderClient_Release(rc);
    IAudioClient_Release(c);
    IMMDevice_Release(d);
    IMMDeviceEnumerator_Release(e);
    printf("mmdevapi ok\n");
    return 0;
}

int main(void)
{
    setvbuf(stdout, NULL, _IONBF, 0);
    return waveout() | mmdevapi();
}
