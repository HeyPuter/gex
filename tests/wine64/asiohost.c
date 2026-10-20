/* An ASIO host for the gex audio (ASIO) driver (Wine's dlls/gexasio): it
 * registers the driver (DllRegisterServer), finds it the way hosts do
 * (HKLM\Software\ASIO\<name>\CLSID), creates it by that class id and runs
 * SECS (default 10) seconds of bufferSwitch with two outputs, and the inputs
 * when the device has capture. Outputs carry silence with an impulse every
 * half second; with the player's loopback (the page's ?audioloop=1,
 * run-node's GEX_AUDIO_LOOP=1) the inputs bring it back, and the round
 * trip is timed in frames: from the bufferSwitch that wrote it to the one
 * whose input holds it.
 *
 *   asiohost.exe [SECS [BUFFER]]
 *
 * Prints "asio driver NAME", "asio rate R buffer B inputs I outputs O
 * latency in LI out LO", "asio switches N want W", and with inputs
 * "asio rt frames MIN MED MAX (N) ms MED". Exit 0 when the switch count is
 * within 10% of SECS * rate / buffer, the time info's sample positions
 * advance by one buffer each, and, with inputs, the impulses came back. */
#include <windows.h>
#include <objbase.h>
#include <math.h>
#include <stdio.h>
#include <stdlib.h>

typedef struct { ULONG hi, lo; } asio64;
typedef struct { LONG isInput, channel; void *buf[2]; } bufinfo;
typedef struct {
	LONG reserved[4];
	double speed;
	asio64 systemTime, samplePosition;
	double sampleRate;
	ULONG flags;
	char reserved2[12];
	double tcSpeed;
	asio64 tcSamples;
	ULONG tcFlags;
	char future[64];
} asiotime;
typedef struct {
	void (*bufferSwitch)(LONG index, LONG direct);
	void (*rateChanged)(double rate);
	LONG (*message)(LONG selector, LONG value, void *msg, double *opt);
	void *(*bufferSwitchTimeInfo)(asiotime *t, LONG index, LONG direct);
} callbacks;
typedef struct iasio iasio;
struct iasio_vtbl {
	HRESULT (STDMETHODCALLTYPE *QueryInterface)(iasio *, REFIID, void **);
	ULONG (STDMETHODCALLTYPE *AddRef)(iasio *);
	ULONG (STDMETHODCALLTYPE *Release)(iasio *);
	LONG (STDMETHODCALLTYPE *init)(iasio *, void *);
	void (STDMETHODCALLTYPE *getDriverName)(iasio *, char *);
	LONG (STDMETHODCALLTYPE *getDriverVersion)(iasio *);
	void (STDMETHODCALLTYPE *getErrorMessage)(iasio *, char *);
	LONG (STDMETHODCALLTYPE *start)(iasio *);
	LONG (STDMETHODCALLTYPE *stop)(iasio *);
	LONG (STDMETHODCALLTYPE *getChannels)(iasio *, LONG *, LONG *);
	LONG (STDMETHODCALLTYPE *getLatencies)(iasio *, LONG *, LONG *);
	LONG (STDMETHODCALLTYPE *getBufferSize)(iasio *, LONG *, LONG *, LONG *, LONG *);
	LONG (STDMETHODCALLTYPE *canSampleRate)(iasio *, double);
	LONG (STDMETHODCALLTYPE *getSampleRate)(iasio *, double *);
	LONG (STDMETHODCALLTYPE *setSampleRate)(iasio *, double);
	LONG (STDMETHODCALLTYPE *getClockSources)(iasio *, void *, LONG *);
	LONG (STDMETHODCALLTYPE *setClockSource)(iasio *, LONG);
	LONG (STDMETHODCALLTYPE *getSamplePosition)(iasio *, asio64 *, asio64 *);
	LONG (STDMETHODCALLTYPE *getChannelInfo)(iasio *, void *);
	LONG (STDMETHODCALLTYPE *createBuffers)(iasio *, bufinfo *, LONG, LONG, callbacks *);
	LONG (STDMETHODCALLTYPE *disposeBuffers)(iasio *);
	LONG (STDMETHODCALLTYPE *controlPanel)(iasio *);
	LONG (STDMETHODCALLTYPE *future)(iasio *, LONG, void *);
	LONG (STDMETHODCALLTYPE *outputReady)(iasio *);
};
struct iasio { const struct iasio_vtbl *v; };

#define MAXCH 4
#define PULSES 64
static bufinfo bi[MAXCH];
static int nbi, nin, frames;
static double rate;
static volatile LONG switches, bad_pos;
static ULONGLONG last_pos = ~0ull;
static long long sample; /* frames handed to the host so far */
static long long sent[PULSES], back[PULSES];
static int nsent, nback;

static void fill(LONG index)
{
	long long every = (long long)rate / 2;
	for (int c = 0; c < nbi; c++) {
		float *b = bi[c].buf[index];
		if (bi[c].isInput) {
			if (c == 0 || bi[c - 1].isInput == 0)
				for (int i = 0; i < frames && nback < nsent; i++)
					if (fabsf(b[i]) > 0.5f) { back[nback] = sample + i - sent[nback]; nback++; break; }
			continue;
		}
		for (int i = 0; i < frames; i++) b[i] = 0;
		long long at = ((sample + every - 1) / every) * every;
		if (sample > 0 && at < sample + frames && nsent < PULSES && nsent == nback) {
			b[at - sample] = 0.9f;
			if (bi[c].channel == 1) sent[nsent++] = at;
		}
	}
	sample += frames;
	InterlockedIncrement(&switches);
}

static void bufferSwitch(LONG index, LONG direct) { (void)direct; fill(index); }
static void rateChanged(double r) { (void)r; }
static LONG message(LONG selector, LONG value, void *msg, double *opt)
{
	(void)msg; (void)opt;
	switch (selector) {
	case 1: return value == 7 || value == 2; /* selector supported: time info, engine version */
	case 2: return 2;                        /* engine version */
	case 7: return 1;                        /* supports time info */
	default: return 0;
	}
}
static void *bufferSwitchTimeInfo(asiotime *t, LONG index, LONG direct)
{
	ULONGLONG pos = ((ULONGLONG)t->samplePosition.hi << 32) | t->samplePosition.lo;
	if (last_pos != ~0ull && pos != last_pos + frames) InterlockedIncrement(&bad_pos);
	last_pos = pos;
	(void)direct;
	fill(index);
	return 0;
}

static int cmp(const void *a, const void *b)
{
	long long x = *(const long long *)a, y = *(const long long *)b;
	return x < y ? -1 : x > y;
}

int main(int argc, char **argv)
{
	setvbuf(stdout, NULL, _IONBF, 0);
	int secs = argc > 1 ? atoi(argv[1]) : 10;
	HMODULE m = LoadLibraryA("gexasio.dll");
	if (!m) { printf("asio no gexasio.dll (%lu)\n", GetLastError()); return 1; }
	HRESULT (WINAPI *reg)(void) = (void *)GetProcAddress(m, "DllRegisterServer");
	if (!reg || reg() != S_OK) { printf("asio DllRegisterServer failed\n"); return 1; }
	WCHAR s[64]; DWORD sz = sizeof s;
	if (RegGetValueW(HKEY_LOCAL_MACHINE, L"Software\\ASIO\\gex audio", L"CLSID", RRF_RT_REG_SZ, NULL, s, &sz)) {
		printf("asio no HKLM\\Software\\ASIO\\gex audio\n"); return 1;
	}
	CLSID clsid;
	if (CLSIDFromString(s, &clsid) != S_OK) { printf("asio bad CLSID\n"); return 1; }
	CoInitialize(NULL);
	iasio *a = NULL;
	HRESULT hr = CoCreateInstance(&clsid, NULL, CLSCTX_INPROC_SERVER, &clsid, (void **)&a);
	if (hr != S_OK || !a) { printf("asio CoCreateInstance %08lx\n", (unsigned long)hr); return 1; }
	if (!a->v->init(a, GetDesktopWindow())) { char e[128]; a->v->getErrorMessage(a, e); printf("asio init failed: %s\n", e); return 1; }
	char name[64] = {0};
	a->v->getDriverName(a, name);
	printf("asio driver %s\n", name);
	LONG ins = 0, outs = 0, mn, mx, pref, gran;
	a->v->getChannels(a, &ins, &outs);
	a->v->getBufferSize(a, &mn, &mx, &pref, &gran);
	a->v->getSampleRate(a, &rate);
	frames = argc > 2 ? atoi(argv[2]) : pref;
	nin = ins > 2 ? 2 : ins;
	for (int i = 0; i < nin; i++) { bi[nbi].isInput = 1; bi[nbi++].channel = i; }
	for (int i = 0; i < 2 && i < outs; i++) { bi[nbi].isInput = 0; bi[nbi++].channel = i; }
	static callbacks cb = { bufferSwitch, rateChanged, message, bufferSwitchTimeInfo };
	LONG r = a->v->createBuffers(a, bi, nbi, frames, &cb);
	if (r) { printf("asio createBuffers %ld\n", (long)r); return 1; }
	LONG li = 0, lo = 0;
	a->v->getLatencies(a, &li, &lo);
	printf("asio rate %.0f buffer %d (min %ld max %ld) inputs %ld outputs %ld latency in %ld out %ld\n",
	       rate, frames, (long)mn, (long)mx, (long)ins, (long)outs, (long)li, (long)lo);
	fflush(stdout);
	if ((r = a->v->start(a))) { printf("asio start %ld\n", (long)r); return 1; }
	Sleep(secs * 1000);
	asio64 sp, ts;
	LONG pr = a->v->getSamplePosition(a, &sp, &ts);
	a->v->stop(a);
	long want = (long)(secs * rate / frames);
	printf("asio switches %ld want %ld, position %s, time info %s\n", (long)switches, want,
	       pr ? "failed" : "ok", bad_pos ? "skips" : "steady");
	int ok = labs(switches - want) <= want / 10 && !bad_pos && !pr;
	if (nin) {
		if (!nback) { printf("asio no impulse came back (%d sent)\n", nsent); ok = 0; }
		else {
			qsort(back, nback, sizeof back[0], cmp);
			printf("asio rt frames %lld %lld %lld (%d) ms %.1f\n", back[0], back[nback / 2], back[nback - 1], nback,
			       back[nback / 2] * 1000.0 / rate);
		}
	}
	a->v->disposeBuffers(a);
	a->v->Release(a);
	CoUninitialize();
	return ok ? 0 : 3;
}
