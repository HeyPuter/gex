/* Media through winegstreamer (its Unix side over FFmpeg): Media
 * Foundation's source reader and media session, a DirectShow filter graph
 * and the WM sync reader, each decoding a whole file.
 *
 *   mediatest sr FILE [video]   source reader: every sample (NV12 and PCM),
 *                               the counts, the first video sample's latency
 *                               and the video frame rate
 *   mediatest ms FILE           media session: source -> sample grabbers
 *                               (RGB32 video, PCM audio), clock ignored
 *   mediatest ds FILE           DirectShow: source -> parser -> grabbers -> null
 *                               renderers, no clock
 *   mediatest wm FILE           WM sync reader: every output sample
 *
 * Prints "PASS MODE ..." or "FAIL MODE ...", and exits 0 when it passes.
 * gex-nt-cc: ARCH-w64-mingw32-gcc -municode
 * gex-nt-libs: -lole32 -loleaut32 -lmf -lmfplat -lmfreadwrite -lmfuuid -lstrmiids -luuid -lwinmm */
#define COBJMACROS
#define INITGUID
#include <windows.h>
#include <stdio.h>
#include <mfapi.h>
#include <mfidl.h>
#include <mfreadwrite.h>
#include <mferror.h>
#include <dshow.h>
#include <qedit.h>
#include <wmsdk.h>

static const GUID MY_GUID_NULL;
static HRESULT get_size(IMFMediaType *t, UINT32 *w, UINT32 *h)
{
    UINT64 v = 0;
    HRESULT hr = IMFMediaType_GetUINT64(t, &MF_MT_FRAME_SIZE, &v);
    *w = (UINT32)(v >> 32);
    *h = (UINT32)v;
    return hr;
}

DEFINE_GUID(MY_CLSID_SampleGrabber, 0xc1f400a0, 0x3f08, 0x11d3, 0x9f, 0x0b, 0x00, 0x60, 0x08, 0x03, 0x9e, 0x37);
DEFINE_GUID(MY_CLSID_NullRenderer, 0xc1f400a4, 0x3f08, 0x11d3, 0x9f, 0x0b, 0x00, 0x60, 0x08, 0x03, 0x9e, 0x37);
DEFINE_GUID(MY_IID_ISampleGrabber, 0x6b652fff, 0x11fe, 0x4fce, 0x92, 0xad, 0x02, 0x66, 0xb5, 0xd7, 0xc7, 0x8f);
DEFINE_GUID(MY_IID_ISampleGrabberCB, 0x0579154a, 0x2b53, 0x4994, 0xb0, 0xd0, 0xe7, 0x73, 0x14, 0x8e, 0xff, 0x85);

static const char *mode;
static int fail(const char *what, HRESULT hr)
{
    printf("FAIL %s %s hr %#lx\n", mode, what, (unsigned long)hr);
    fflush(stdout);
    return 1;
}

static double now(void)
{
    LARGE_INTEGER c, f;
    QueryPerformanceCounter(&c);
    QueryPerformanceFrequency(&f);
    return (double)c.QuadPart / f.QuadPart;
}

/* the mean of a frame's middle row of bytes (0: an empty picture) */
static unsigned mean_row(const BYTE *p, DWORD len, DWORD row)
{
    DWORD i, n = row && len >= row ? row : (len < 4096 ? len : 4096);
    const BYTE *r = p + (len / 2 / (row ? row : 1)) * (row ? row : 1);
    unsigned long long s = 0;
    if (r + n > p + len) r = p;
    for (i = 0; i < n; ++i) s += r[i];
    return n ? (unsigned)(s / n) : 0;
}

/* ---- source reader ---- */
static int run_sr(const WCHAR *file, int video_only)
{
    IMFSourceReader *reader;
    IMFMediaType *type;
    HRESULT hr;
    DWORD vframes = 0, asamples = 0, eos = 0, want = 0;
    ULONGLONG abytes = 0;
    unsigned luma = 0;
    UINT32 w = 0, h = 0;
    double t0, t1, topen = now(), tfirst = 0;
    int have_v = 0, have_a = 0;

    if (FAILED(hr = MFCreateSourceReaderFromURL(file, NULL, &reader))) return fail("MFCreateSourceReaderFromURL", hr);
    IMFSourceReader_SetStreamSelection(reader, MF_SOURCE_READER_ALL_STREAMS, FALSE);
    if (SUCCEEDED(IMFSourceReader_SetStreamSelection(reader, MF_SOURCE_READER_FIRST_VIDEO_STREAM, TRUE)))
    {
        MFCreateMediaType(&type);
        IMFMediaType_SetGUID(type, &MF_MT_MAJOR_TYPE, &MFMediaType_Video);
        IMFMediaType_SetGUID(type, &MF_MT_SUBTYPE, &MFVideoFormat_NV12);
        if (FAILED(hr = IMFSourceReader_SetCurrentMediaType(reader, MF_SOURCE_READER_FIRST_VIDEO_STREAM, NULL, type)))
            return fail("SetCurrentMediaType(video NV12)", hr);
        IMFMediaType_Release(type);
        if (SUCCEEDED(IMFSourceReader_GetCurrentMediaType(reader, MF_SOURCE_READER_FIRST_VIDEO_STREAM, &type)))
        {
            get_size(type, &w, &h);
            IMFMediaType_Release(type);
        }
        have_v = 1; want |= 1;
    }
    if (!video_only && SUCCEEDED(IMFSourceReader_SetStreamSelection(reader, MF_SOURCE_READER_FIRST_AUDIO_STREAM, TRUE)))
    {
        MFCreateMediaType(&type);
        IMFMediaType_SetGUID(type, &MF_MT_MAJOR_TYPE, &MFMediaType_Audio);
        IMFMediaType_SetGUID(type, &MF_MT_SUBTYPE, &MFAudioFormat_PCM);
        if (FAILED(hr = IMFSourceReader_SetCurrentMediaType(reader, MF_SOURCE_READER_FIRST_AUDIO_STREAM, NULL, type)))
            return fail("SetCurrentMediaType(audio PCM)", hr);
        IMFMediaType_Release(type);
        have_a = 1; want |= 2;
    }
    if (!want) return fail("no streams", E_FAIL);

    t0 = now();
    while (eos != want)
    {
        DWORD index, flags;
        LONGLONG ts;
        IMFSample *sample = NULL;

        if (FAILED(hr = IMFSourceReader_ReadSample(reader, MF_SOURCE_READER_ANY_STREAM, 0, &index, &flags, &ts, &sample)))
            return fail("ReadSample", hr);
        if (sample)
        {
            IMFMediaBuffer *buf;
            DWORD len;
            BYTE *p;
            IMFMediaType *cur;
            GUID major = MY_GUID_NULL;

            IMFSourceReader_GetCurrentMediaType(reader, index, &cur);
            IMFMediaType_GetGUID(cur, &MF_MT_MAJOR_TYPE, &major);
            IMFMediaType_Release(cur);
            IMFSample_ConvertToContiguousBuffer(sample, &buf);
            IMFMediaBuffer_GetCurrentLength(buf, &len);
            if (IsEqualGUID(&major, &MFMediaType_Video))
            {
                if (!vframes && SUCCEEDED(IMFMediaBuffer_Lock(buf, &p, NULL, NULL)))
                {
                    luma = mean_row(p, w * h < len ? w * h : len, w);
                    IMFMediaBuffer_Unlock(buf);
                }
                if (!vframes++) tfirst = now();
            }
            else { ++asamples; abytes += len; }
            IMFMediaBuffer_Release(buf);
            IMFSample_Release(sample);
        }
        if (flags & MF_SOURCE_READERF_ENDOFSTREAM)
        {
            IMFMediaType *cur;
            GUID major = MY_GUID_NULL;
            IMFSourceReader_GetCurrentMediaType(reader, index, &cur);
            IMFMediaType_GetGUID(cur, &MF_MT_MAJOR_TYPE, &major);
            IMFMediaType_Release(cur);
            eos |= IsEqualGUID(&major, &MFMediaType_Video) ? 1 : 2;
        }
        if (flags & MF_SOURCE_READERF_ERROR) return fail("stream error", E_FAIL);
    }
    t1 = now();
    IMFSourceReader_Release(reader);
    if (vframes) printf("first video sample %.0f ms after MFCreateSourceReaderFromURL\n", (tfirst - topen) * 1000);
    if ((have_v && (!vframes || !luma)) || (have_a && !abytes))
    {
        printf("FAIL sr video %lu frames (luma %u) audio %lu samples %llu bytes\n", vframes, luma, asamples, abytes);
        return 1;
    }
    printf("PASS sr video %ux%u %lu frames (first frame mid-row mean %u) audio %lu samples %llu bytes, %.2f s, %.1f video fps\n",
            w, h, vframes, luma, asamples, abytes, t1 - t0, vframes / (t1 - t0));
    return 0;
}

/* ---- media session ---- */
struct grabber
{
    IMFSampleGrabberSinkCallback iface;
    LONG ref;
    const char *name;
    DWORD samples;
    ULONGLONG bytes;
    unsigned first_mean;
};

static HRESULT WINAPI gcb_QueryInterface(IMFSampleGrabberSinkCallback *iface, REFIID riid, void **out)
{
    if (IsEqualIID(riid, &IID_IMFSampleGrabberSinkCallback) || IsEqualIID(riid, &IID_IMFClockStateSink)
            || IsEqualIID(riid, &IID_IUnknown))
    {
        *out = iface;
        IMFSampleGrabberSinkCallback_AddRef(iface);
        return S_OK;
    }
    *out = NULL;
    return E_NOINTERFACE;
}
static ULONG WINAPI gcb_AddRef(IMFSampleGrabberSinkCallback *iface) { return InterlockedIncrement(&((struct grabber *)iface)->ref); }
static ULONG WINAPI gcb_Release(IMFSampleGrabberSinkCallback *iface) { return InterlockedDecrement(&((struct grabber *)iface)->ref); }
static HRESULT WINAPI gcb_OnClockStart(IMFSampleGrabberSinkCallback *i, MFTIME t, LONGLONG o) { return S_OK; }
static HRESULT WINAPI gcb_OnClockStop(IMFSampleGrabberSinkCallback *i, MFTIME t) { return S_OK; }
static HRESULT WINAPI gcb_OnClockPause(IMFSampleGrabberSinkCallback *i, MFTIME t) { return S_OK; }
static HRESULT WINAPI gcb_OnClockRestart(IMFSampleGrabberSinkCallback *i, MFTIME t) { return S_OK; }
static HRESULT WINAPI gcb_OnClockSetRate(IMFSampleGrabberSinkCallback *i, MFTIME t, float r) { return S_OK; }
static HRESULT WINAPI gcb_OnSetPresentationClock(IMFSampleGrabberSinkCallback *i, IMFPresentationClock *c) { return S_OK; }
static HRESULT WINAPI gcb_OnProcessSample(IMFSampleGrabberSinkCallback *iface, REFGUID major, DWORD flags,
        LONGLONG time, LONGLONG duration, const BYTE *buffer, DWORD size)
{
    struct grabber *g = (struct grabber *)iface;
    if (!g->samples) g->first_mean = mean_row(buffer, size, 0);
    ++g->samples;
    g->bytes += size;
    return S_OK;
}
static HRESULT WINAPI gcb_OnShutdown(IMFSampleGrabberSinkCallback *iface) { return S_OK; }
static IMFSampleGrabberSinkCallbackVtbl gcb_vtbl =
{
    gcb_QueryInterface, gcb_AddRef, gcb_Release, gcb_OnClockStart, gcb_OnClockStop, gcb_OnClockPause,
    gcb_OnClockRestart, gcb_OnClockSetRate, gcb_OnSetPresentationClock, gcb_OnProcessSample, gcb_OnShutdown,
};

static int run_ms(const WCHAR *file)
{
    IMFSourceResolver *resolver;
    IMFMediaSource *source;
    IMFPresentationDescriptor *pd;
    IMFMediaSession *session;
    IMFTopology *topo;
    MF_OBJECT_TYPE otype;
    IUnknown *obj;
    DWORD count, i;
    HRESULT hr;
    struct grabber grabbers[2] = {{{&gcb_vtbl}, 1, "video"}, {{&gcb_vtbl}, 1, "audio"}};
    int streams = 0, ended = 0;
    double t0, t1;
    PROPVARIANT var;

    if (FAILED(hr = MFCreateSourceResolver(&resolver))) return fail("MFCreateSourceResolver", hr);
    if (FAILED(hr = IMFSourceResolver_CreateObjectFromURL(resolver, file, MF_RESOLUTION_MEDIASOURCE, NULL, &otype, &obj)))
        return fail("CreateObjectFromURL", hr);
    IUnknown_QueryInterface(obj, &IID_IMFMediaSource, (void **)&source);
    IUnknown_Release(obj);
    if (FAILED(hr = IMFMediaSource_CreatePresentationDescriptor(source, &pd))) return fail("CreatePresentationDescriptor", hr);
    if (FAILED(hr = MFCreateMediaSession(NULL, &session))) return fail("MFCreateMediaSession", hr);
    MFCreateTopology(&topo);
    IMFPresentationDescriptor_GetStreamDescriptorCount(pd, &count);
    for (i = 0; i < count; ++i)
    {
        IMFStreamDescriptor *sd;
        IMFMediaTypeHandler *th;
        IMFTopologyNode *src, *out;
        IMFMediaType *type;
        IMFActivate *act;
        BOOL selected;
        GUID major;
        struct grabber *g;

        IMFPresentationDescriptor_GetStreamDescriptorByIndex(pd, i, &selected, &sd);
        IMFStreamDescriptor_GetMediaTypeHandler(sd, &th);
        IMFMediaTypeHandler_GetMajorType(th, &major);
        IMFMediaTypeHandler_Release(th);
        if (!selected || !(IsEqualGUID(&major, &MFMediaType_Video) || IsEqualGUID(&major, &MFMediaType_Audio)))
        {
            IMFStreamDescriptor_Release(sd);
            continue;
        }
        g = &grabbers[IsEqualGUID(&major, &MFMediaType_Video) ? 0 : 1];
        MFCreateMediaType(&type);
        IMFMediaType_SetGUID(type, &MF_MT_MAJOR_TYPE, &major);
        IMFMediaType_SetGUID(type, &MF_MT_SUBTYPE, IsEqualGUID(&major, &MFMediaType_Video) ? &MFVideoFormat_RGB32 : &MFAudioFormat_PCM);
        if (FAILED(hr = MFCreateSampleGrabberSinkActivate(type, &g->iface, &act))) return fail("MFCreateSampleGrabberSinkActivate", hr);
        IMFActivate_SetUINT32(act, &MF_SAMPLEGRABBERSINK_IGNORE_CLOCK, TRUE);
        IMFMediaType_Release(type);
        MFCreateTopologyNode(MF_TOPOLOGY_SOURCESTREAM_NODE, &src);
        IMFTopologyNode_SetUnknown(src, &MF_TOPONODE_SOURCE, (IUnknown *)source);
        IMFTopologyNode_SetUnknown(src, &MF_TOPONODE_PRESENTATION_DESCRIPTOR, (IUnknown *)pd);
        IMFTopologyNode_SetUnknown(src, &MF_TOPONODE_STREAM_DESCRIPTOR, (IUnknown *)sd);
        MFCreateTopologyNode(MF_TOPOLOGY_OUTPUT_NODE, &out);
        IMFTopologyNode_SetObject(out, (IUnknown *)act);
        IMFTopology_AddNode(topo, src);
        IMFTopology_AddNode(topo, out);
        IMFTopologyNode_ConnectOutput(src, 0, out, 0);
        IMFTopologyNode_Release(src);
        IMFTopologyNode_Release(out);
        IMFStreamDescriptor_Release(sd);
        ++streams;
    }
    if (!streams) return fail("no streams", E_FAIL);
    if (FAILED(hr = IMFMediaSession_SetTopology(session, 0, topo))) return fail("SetTopology", hr);
    t0 = now();
    while (!ended)
    {
        IMFMediaEvent *ev;
        MediaEventType et;
        HRESULT status;

        if (FAILED(hr = IMFMediaSession_GetEvent(session, 0, &ev))) return fail("GetEvent", hr);
        IMFMediaEvent_GetType(ev, &et);
        IMFMediaEvent_GetStatus(ev, &status);
        if (FAILED(status))
        {
            printf("  event %lu status %#lx\n", (unsigned long)et, (unsigned long)status);
            return fail("session event", status);
        }
        if (et == MESessionTopologyStatus)
        {
            UINT32 ts = 0;
            IMFMediaEvent_GetUINT32(ev, &MF_EVENT_TOPOLOGY_STATUS, &ts);
            if (ts == MF_TOPOSTATUS_READY)
            {
                PropVariantInit(&var);
                if (FAILED(hr = IMFMediaSession_Start(session, &MY_GUID_NULL, &var))) return fail("Start", hr);
            }
        }
        else if (et == MESessionEnded) ended = 1;
        IMFMediaEvent_Release(ev);
    }
    t1 = now();
    IMFMediaSession_Close(session);
    IMFMediaSession_Shutdown(session);
    IMFMediaSource_Shutdown(source);
    printf("%s ms video %lu samples %llu bytes (first mean %u) audio %lu samples %llu bytes, %.2f s\n",
            grabbers[0].samples && (streams < 2 || grabbers[1].samples) ? "PASS" : "FAIL",
            grabbers[0].samples, grabbers[0].bytes, grabbers[0].first_mean, grabbers[1].samples, grabbers[1].bytes, t1 - t0);
    return !(grabbers[0].samples && (streams < 2 || grabbers[1].samples));
}

/* ---- DirectShow ---- */
struct dscb
{
    ISampleGrabberCB iface;
    DWORD samples;
    ULONGLONG bytes;
    unsigned first_mean;
};
static HRESULT WINAPI dcb_QueryInterface(ISampleGrabberCB *iface, REFIID riid, void **out)
{
    if (IsEqualIID(riid, &MY_IID_ISampleGrabberCB) || IsEqualIID(riid, &IID_IUnknown)) { *out = iface; return S_OK; }
    *out = NULL;
    return E_NOINTERFACE;
}
static ULONG WINAPI dcb_AddRef(ISampleGrabberCB *iface) { return 2; }
static ULONG WINAPI dcb_Release(ISampleGrabberCB *iface) { return 1; }
static HRESULT WINAPI dcb_SampleCB(ISampleGrabberCB *iface, double t, IMediaSample *s)
{
    struct dscb *c = (struct dscb *)iface;
    BYTE *p;
    long n = IMediaSample_GetActualDataLength(s);
    if (!c->samples && SUCCEEDED(IMediaSample_GetPointer(s, &p))) c->first_mean = mean_row(p, n, 0);
    ++c->samples;
    c->bytes += n;
    return S_OK;
}
static HRESULT WINAPI dcb_BufferCB(ISampleGrabberCB *iface, double t, BYTE *b, long n) { return S_OK; }
static ISampleGrabberCBVtbl dcb_vtbl = { dcb_QueryInterface, dcb_AddRef, dcb_Release, dcb_SampleCB, dcb_BufferCB };

static IPin *find_pin(IBaseFilter *f, PIN_DIRECTION want, BOOL unconnected)
{
    IEnumPins *e;
    IPin *pin;
    IBaseFilter_EnumPins(f, &e);
    while (IEnumPins_Next(e, 1, &pin, NULL) == S_OK)
    {
        PIN_DIRECTION dir;
        IPin *peer = NULL;
        IPin_QueryDirection(pin, &dir);
        if (dir == want && (!unconnected || IPin_ConnectedTo(pin, &peer) == VFW_E_NOT_CONNECTED))
        {
            IEnumPins_Release(e);
            return pin;
        }
        if (peer) IPin_Release(peer);
        IPin_Release(pin);
    }
    IEnumPins_Release(e);
    return NULL;
}

static IBaseFilter *add_grabber(IGraphBuilder *graph, const GUID *major, struct dscb *cb, const WCHAR *name, IBaseFilter **null)
{
    IBaseFilter *f;
    ISampleGrabber *sg;
    AM_MEDIA_TYPE mt = {0};
    WCHAR nn[32];

    if (FAILED(CoCreateInstance(&MY_CLSID_SampleGrabber, NULL, CLSCTX_INPROC_SERVER, &IID_IBaseFilter, (void **)&f))) return NULL;
    IBaseFilter_QueryInterface(f, &MY_IID_ISampleGrabber, (void **)&sg);
    mt.majortype = *major;
    ISampleGrabber_SetMediaType(sg, &mt);
    ISampleGrabber_SetCallback(sg, &cb->iface, 0);
    ISampleGrabber_Release(sg);
    IGraphBuilder_AddFilter(graph, f, name);
    CoCreateInstance(&MY_CLSID_NullRenderer, NULL, CLSCTX_INPROC_SERVER, &IID_IBaseFilter, (void **)null);
    (void)nn;
    IGraphBuilder_AddFilter(graph, *null, name[0] == L'v' ? L"vnull" : L"anull");
    return f;
}

static int run_ds(const WCHAR *file)
{
    IGraphBuilder *graph;
    IBaseFilter *src, *vg, *ag, *vn, *an;
    IMediaControl *control;
    IMediaEvent *event;
    IMediaFilter *mf;
    IPin *pin, *in;
    HRESULT hr;
    long code = 0;
    struct dscb vcb = {{&dcb_vtbl}}, acb = {{&dcb_vtbl}};
    IEnumFilters *ef;
    IBaseFilter *f;
    double t0, t1;
    int audio = 0;

    if (FAILED(hr = CoCreateInstance(&CLSID_FilterGraph, NULL, CLSCTX_INPROC_SERVER, &IID_IGraphBuilder, (void **)&graph)))
        return fail("FilterGraph", hr);
    if (FAILED(hr = IGraphBuilder_AddSourceFilter(graph, file, L"source", &src))) return fail("AddSourceFilter", hr);
    if (!(vg = add_grabber(graph, &MEDIATYPE_Video, &vcb, L"vgrab", &vn))) return fail("SampleGrabber", E_FAIL);
    if (!(ag = add_grabber(graph, &MEDIATYPE_Audio, &acb, L"agrab", &an))) return fail("SampleGrabber", E_FAIL);
    pin = find_pin(src, PINDIR_OUTPUT, TRUE);
    in = find_pin(vg, PINDIR_INPUT, TRUE);
    if (FAILED(hr = IGraphBuilder_Connect(graph, pin, in))) return fail("Connect(source, video grabber)", hr);
    IPin_Release(pin); IPin_Release(in);
    pin = find_pin(vg, PINDIR_OUTPUT, TRUE); in = find_pin(vn, PINDIR_INPUT, TRUE);
    if (FAILED(hr = IGraphBuilder_Connect(graph, pin, in))) return fail("Connect(video grabber, null)", hr);
    IPin_Release(pin); IPin_Release(in);
    /* the splitter's other output: audio */
    IGraphBuilder_EnumFilters(graph, &ef);
    while (!audio && IEnumFilters_Next(ef, 1, &f, NULL) == S_OK)
    {
        if (f != src && f != vg && f != ag && f != vn && f != an && (pin = find_pin(f, PINDIR_OUTPUT, TRUE)))
        {
            in = find_pin(ag, PINDIR_INPUT, TRUE);
            if (SUCCEEDED(IGraphBuilder_Connect(graph, pin, in))) audio = 1;
            IPin_Release(in);
            IPin_Release(pin);
        }
        IBaseFilter_Release(f);
    }
    IEnumFilters_Release(ef);
    if (audio)
    {
        pin = find_pin(ag, PINDIR_OUTPUT, TRUE); in = find_pin(an, PINDIR_INPUT, TRUE);
        if (FAILED(hr = IGraphBuilder_Connect(graph, pin, in))) return fail("Connect(audio grabber, null)", hr);
        IPin_Release(pin); IPin_Release(in);
    }
    else IGraphBuilder_RemoveFilter(graph, ag), IGraphBuilder_RemoveFilter(graph, an);
    IGraphBuilder_QueryInterface(graph, &IID_IMediaFilter, (void **)&mf);
    IMediaFilter_SetSyncSource(mf, NULL);
    IGraphBuilder_QueryInterface(graph, &IID_IMediaControl, (void **)&control);
    IGraphBuilder_QueryInterface(graph, &IID_IMediaEvent, (void **)&event);
    t0 = now();
    if (FAILED(hr = IMediaControl_Run(control))) return fail("Run", hr);
    hr = IMediaEvent_WaitForCompletion(event, 60000, &code);
    t1 = now();
    IMediaControl_Stop(control);
    printf("%s ds video %lu samples %llu bytes (first mean %u) audio%s %lu samples %llu bytes, completion hr %#lx code %#lx, %.2f s\n",
            SUCCEEDED(hr) && code == EC_COMPLETE && vcb.samples && (!audio || acb.samples) ? "PASS" : "FAIL",
            vcb.samples, vcb.bytes, vcb.first_mean, audio ? "" : " (none)", acb.samples, acb.bytes,
            (unsigned long)hr, (unsigned long)code, t1 - t0);
    return !(SUCCEEDED(hr) && code == EC_COMPLETE && vcb.samples);
}

/* ---- WM sync reader ---- */
static int run_wm(const WCHAR *file)
{
    HRESULT (WINAPI *create)(IUnknown *, DWORD, IWMSyncReader **);
    IWMSyncReader *reader;
    DWORD outputs, i, counts[8] = {0};
    ULONGLONG bytes[8] = {0};
    HRESULT hr;
    HMODULE m = LoadLibraryW(L"wmvcore.dll");

    if (!m || !(create = (void *)GetProcAddress(m, "WMCreateSyncReader"))) return fail("wmvcore", E_FAIL);
    if (FAILED(hr = create(NULL, 0, &reader))) return fail("WMCreateSyncReader", hr);
    if (FAILED(hr = IWMSyncReader_Open(reader, file))) return fail("Open", hr);
    IWMSyncReader_GetOutputCount(reader, &outputs);
    for (;;)
    {
        INSSBuffer *sample;
        QWORD pts, dur;
        DWORD flags, output;
        WORD stream;
        DWORD len;

        hr = IWMSyncReader_GetNextSample(reader, 0, &sample, &pts, &dur, &flags, &output, &stream);
        if (hr == (HRESULT)0xc00d0bcf) break; /* NS_E_NO_MORE_SAMPLES */
        if (FAILED(hr)) return fail("GetNextSample", hr);
        { BYTE *p; INSSBuffer_GetBufferAndLength(sample, &p, &len); }
        if (output < 8) { ++counts[output]; bytes[output] += len; }
        INSSBuffer_Release(sample);
    }
    IWMSyncReader_Close(reader);
    printf("%s wm %lu outputs:", counts[0] ? "PASS" : "FAIL", outputs);
    for (i = 0; i < outputs && i < 8; ++i) printf(" [%lu] %lu samples %llu bytes", i, counts[i], bytes[i]);
    printf("\n");
    return !counts[0];
}


int wmain(int argc, WCHAR **argv)
{
    char m[8];
    int r;
    HRESULT hr;

    if (argc < 3) { printf("usage: mediatest sr|ms|ds|wm FILE [video]\n"); return 2; }
    WideCharToMultiByte(CP_ACP, 0, argv[1], -1, m, sizeof(m), NULL, NULL);
    mode = m;
    CoInitializeEx(NULL, COINIT_MULTITHREADED);
    if (FAILED(hr = MFStartup(MF_VERSION, MFSTARTUP_FULL))) return fail("MFStartup", hr);
    printf("mediatest %s (%d-bit)\n", m, (int)sizeof(void *) * 8);
    fflush(stdout);
    if (!lstrcmpW(argv[1], L"sr")) r = run_sr(argv[2], argc > 3);
    else if (!lstrcmpW(argv[1], L"ms")) r = run_ms(argv[2]);
    else if (!lstrcmpW(argv[1], L"ds")) r = run_ds(argv[2]);
    else if (!lstrcmpW(argv[1], L"wm")) r = run_wm(argv[2]);
    else { printf("unknown mode\n"); r = 2; }
    fflush(stdout);
    MFShutdown();
    CoUninitialize();
    return r;
}
