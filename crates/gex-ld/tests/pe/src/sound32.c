#define INITGUID
#define COBJMACROS
#include <dsound.h>
#include <windows.h>
int entry(void) {
    IDirectSound8 *device = 0;
    IDirectSoundBuffer *buffer = 0;
    if (FAILED(DirectSoundCreate8(0, &device, 0))) {
        return 1;
    }
    if (FAILED(IDirectSound8_SetCooperativeLevel(device, GetDesktopWindow(), DSSCL_PRIORITY))) {
        return 2;
    }
    WAVEFORMATEX format = { WAVE_FORMAT_PCM, 2, 44100, 176400, 4, 16, 0 };
    DSBUFFERDESC desc = { 0 };
    desc.dwSize = sizeof(desc);
    desc.dwFlags = DSBCAPS_CTRLVOLUME | DSBCAPS_CTRLPAN | DSBCAPS_CTRLFREQUENCY | DSBCAPS_CTRLPOSITIONNOTIFY;
    desc.dwBufferBytes = 176400;
    desc.lpwfxFormat = &format;
    if (FAILED(IDirectSound8_CreateSoundBuffer(device, &desc, &buffer, 0))) {
        return 3;
    }
    void *a, *b;
    DWORD na, nb;
    if (FAILED(IDirectSoundBuffer_Lock(buffer, 0, 0, &a, &na, &b, &nb, DSBLOCK_ENTIREBUFFER)) || na != 176400 || nb) {
        return 4;
    }
    for (unsigned i = 0; i < 44100; i++) {
        short value = (i % 100) < 50 ? 8192 : -8192;
        ((short *)a)[i * 2] = value;
        ((short *)a)[i * 2 + 1] = -value;
    }
    if (FAILED(IDirectSoundBuffer_Unlock(buffer, a, na, b, nb))) {
        return 5;
    }
    if (FAILED(IDirectSoundBuffer_Lock(buffer, 176396, 8, &a, &na, &b, &nb, 0)) || na != 4 || nb != 4) {
        return 6;
    }
    if (FAILED(IDirectSoundBuffer_Unlock(buffer, a, na, b, nb))) {
        return 7;
    }
    if (FAILED(IDirectSoundBuffer_SetVolume(buffer, -600)) || FAILED(IDirectSoundBuffer_SetPan(buffer, 0)) ||
        FAILED(IDirectSoundBuffer_SetFrequency(buffer, 44100))) {
        return 8;
    }
    IDirectSoundNotify *notify = 0;
    if (FAILED(IDirectSoundBuffer_QueryInterface(buffer, &IID_IDirectSoundNotify, (void **)&notify))) {
        return 18;
    }
    HANDLE event = CreateEventW(0, FALSE, FALSE, 0);
    DSBPOSITIONNOTIFY notification = { 17640, event };
    if (!event || FAILED(IDirectSoundNotify_SetNotificationPositions(notify, 1, &notification))) {
        return 19;
    }
    if (FAILED(IDirectSoundBuffer_Play(buffer, 0, 0, DSBPLAY_LOOPING))) {
        return 9;
    }
    if (WaitForSingleObject(event, 2000) != WAIT_OBJECT_0) {
        return 20;
    }
    DWORD state, pos, write;
    IDirectSoundBuffer_GetStatus(buffer, &state);
    if (!(state & DSBSTATUS_PLAYING)) {
        return 10;
    }
    Sleep(250);
    if (FAILED(IDirectSoundBuffer_GetCurrentPosition(buffer, &pos, &write)) || pos >= 176400 || pos % 4 || write >= 176400) {
        return 11;
    }
    if (FAILED(IDirectSoundBuffer_Stop(buffer))) {
        return 12;
    }
    IDirectSoundBuffer_GetStatus(buffer, &state);
    if (state & DSBSTATUS_PLAYING) {
        return 13;
    }
    if (FAILED(IDirectSoundBuffer_SetCurrentPosition(buffer, 0))) {
        return 14;
    }
    IDirectSoundBuffer *copy = 0;
    if (FAILED(IDirectSound8_DuplicateSoundBuffer(device, buffer, &copy))) {
        return 15;
    }
    IDirectSoundNotify_Release(notify);
    IDirectSoundBuffer_Release(buffer);
    if (FAILED(IDirectSoundBuffer_Lock(copy, 0, 4, &a, &na, &b, &nb, 0)) || ((short *)a)[0] != 8192) {
        return 16;
    }
    if (FAILED(IDirectSoundBuffer_Unlock(copy, a, na, b, nb))) {
        return 17;
    }
    if (FAILED(IDirectSoundBuffer_QueryInterface(copy, &IID_IDirectSoundNotify, (void **)&notify))) {
        return 21;
    }
    notification.dwOffset = DSBPN_OFFSETSTOP;
    if (FAILED(IDirectSoundNotify_SetNotificationPositions(notify, 1, &notification))) {
        return 22;
    }
    ResetEvent(event);
    if (FAILED(IDirectSoundBuffer_SetCurrentPosition(copy, 176000)) || FAILED(IDirectSoundBuffer_Play(copy, 0, 0, 0)) ||
        WaitForSingleObject(event, 2000) != WAIT_OBJECT_0) {
        return 23;
    }
    IDirectSoundBuffer_GetStatus(copy, &state);
    if (state & DSBSTATUS_PLAYING) {
        return 24;
    }
    IDirectSoundBuffer_Release(copy);
    IDirectSoundNotify_Release(notify);
    CloseHandle(event);
    IDirectSound8_Release(device);
    OutputDebugStringA("PASS: DirectSound PCM lock/wrap, notification, play, cursor, stop, duplicate and release\n");
    return 0;
}
