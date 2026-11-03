/* Game controllers through Wine's HID bus (winebus over SDL over the page's
 * /dev/input): XInput, DirectInput 8 and winmm's joystick calls.
 *
 *   padtest none       no controller anywhere: XInput's slot 0 is not
 *                      connected, DirectInput lists no game controller and
 *                      winmm's joystick 1 has nothing. Exit 0 when so.
 *   padtest wait SECS  a controller arrives within SECS (the page connects
 *                      one, an Xbox 360 pad with A held and the left stick
 *                      half right): XInput sees it connected, then A and
 *                      the stick; DirectInput lists one game controller
 *                      with its first button down; winmm's joystick 1 has
 *                      its first button down. Prints "ok ..." lines and
 *                      "N failures"; exit N.
 * gex-nt-libs: -lxinput1_3 -ldinput8 -ldxguid -lwinmm -lole32
 */
#define DIRECTINPUT_VERSION 0x0800
#include <windows.h>
#include <xinput.h>
#include <dinput.h>
#include <mmsystem.h>
#include <stdio.h>
#include <stdlib.h>

static int failures;
#define CHECK(c, ...) do { if (c) printf("ok "); else { printf("FAIL "); failures++; } printf(__VA_ARGS__); printf("\n"); } while (0)

static int controllers;
static GUID first;

static BOOL CALLBACK count_device(const DIDEVICEINSTANCEA *d, void *ctx)
{
    if (!controllers++) first = d->guidInstance;
    printf("  dinput: %s\n", d->tszProductName);
    return DIENUM_CONTINUE;
}

static int dinput_count(IDirectInput8A *di)
{
    controllers = 0;
    IDirectInput8_EnumDevices(di, DI8DEVCLASS_GAMECTRL, count_device, NULL, DIEDFL_ATTACHEDONLY);
    return controllers;
}

/* the first game controller's state, its first button and X axis */
static int dinput_state(IDirectInput8A *di, BYTE *button0, LONG *x)
{
    IDirectInputDevice8A *dev;
    DIJOYSTATE2 st;
    HRESULT hr;

    if (FAILED(hr = IDirectInput8_CreateDevice(di, &first, &dev, NULL))) return hr;
    if (FAILED(hr = IDirectInputDevice8_SetDataFormat(dev, &c_dfDIJoystick2))) goto done;
    if (FAILED(hr = IDirectInputDevice8_Acquire(dev))) goto done;
    IDirectInputDevice8_Poll(dev);
    if (SUCCEEDED(hr = IDirectInputDevice8_GetDeviceState(dev, sizeof(st), &st)))
    {
        *button0 = st.rgbButtons[0];
        *x = st.lX;
    }
    IDirectInputDevice8_Unacquire(dev);
done:
    IDirectInputDevice8_Release(dev);
    return hr;
}

int main(int argc, char **argv)
{
    IDirectInput8A *di;
    XINPUT_STATE xs;
    JOYINFOEX ji = {sizeof(ji), JOY_RETURNALL};
    DWORD r, start = GetTickCount();
    HRESULT hr;

    setvbuf(stdout, NULL, _IONBF, 0);
    hr = DirectInput8Create(GetModuleHandleA(NULL), DIRECTINPUT_VERSION, &IID_IDirectInput8A, (void **)&di, NULL);
    CHECK(SUCCEEDED(hr), "DirectInput8Create (%#lx)", hr);
    if (FAILED(hr)) return 1;

    if (argc > 1 && !strcmp(argv[1], "none"))
    {
        r = XInputGetState(0, &xs);
        CHECK(r == ERROR_DEVICE_NOT_CONNECTED, "XInput slot 0 not connected (%lu)", r);
        CHECK(!dinput_count(di), "DirectInput lists no game controller (%d)", controllers);
        r = joyGetPosEx(JOYSTICKID1, &ji);
        CHECK(r != JOYERR_NOERROR, "winmm's joystick 1 has nothing (%lu)", r);
        printf("%d failures\n", failures);
        return failures;
    }
    if (argc < 3 || strcmp(argv[1], "wait"))
    {
        printf("usage: padtest none | padtest wait SECS\n");
        return 2;
    }

    /* the controller's arrival, as a game sees it */
    DWORD limit = atoi(argv[2]) * 1000;
    while ((r = XInputGetState(0, &xs)) != ERROR_SUCCESS && GetTickCount() - start < limit) Sleep(100);
    CHECK(r == ERROR_SUCCESS, "XInput slot 0 connected after %lu ms (%lu)", GetTickCount() - start, r);
    if (r != ERROR_SUCCESS)
    {
        printf("%d failures\n", failures);
        return failures;
    }
    /* (the button and the stick are two events: both, or 10 s) */
    for (DWORD t = GetTickCount(); !((xs.Gamepad.wButtons & XINPUT_GAMEPAD_A) && xs.Gamepad.sThumbLX > 12000) && GetTickCount() - t < 10000; Sleep(50))
        XInputGetState(0, &xs);
    CHECK(xs.Gamepad.wButtons & XINPUT_GAMEPAD_A, "XInput: A held (buttons %#x)", xs.Gamepad.wButtons);
    CHECK(xs.Gamepad.sThumbLX > 12000 && xs.Gamepad.sThumbLX < 20000, "XInput: the left stick half right (%d)", xs.Gamepad.sThumbLX);

    int n = 0;
    for (DWORD t = GetTickCount(); !(n = dinput_count(di)) && GetTickCount() - t < 10000; Sleep(200)) {}
    CHECK(n == 1, "DirectInput lists one game controller (%d)", n);
    if (n)
    {
        BYTE b0 = 0;
        LONG x = 0;
        for (DWORD t = GetTickCount(); GetTickCount() - t < 10000; Sleep(100))
            if (SUCCEEDED(hr = dinput_state(di, &b0, &x)) && (b0 & 0x80)) break;
        CHECK(SUCCEEDED(hr) && (b0 & 0x80), "DirectInput: its first button down (%#lx, %#x)", hr, b0);
        CHECK(x > 40000 && x < 56000, "DirectInput: X half right (%ld of 0..65535)", x);
    }

    for (DWORD t = GetTickCount(); GetTickCount() - t < 10000; Sleep(200))
        if ((r = joyGetPosEx(JOYSTICKID1, &ji)) == JOYERR_NOERROR && (ji.dwButtons & 1)) break;
    CHECK(r == JOYERR_NOERROR && (ji.dwButtons & 1), "winmm: joystick 1's first button down (%lu, buttons %#lx)", r, ji.dwButtons);

    IDirectInput8_Release(di);
    printf("%d failures\n", failures);
    return failures;
}
