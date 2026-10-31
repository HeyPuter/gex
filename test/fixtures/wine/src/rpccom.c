/* RPC and COM through rpcrt4 (nt-x64.sh's rpccom case): an RPC client call
 * into services.exe (sechost's svcctl client: NdrClientCall2 and the
 * server's call_server_func), a cross-apartment IDispatch call through
 * oleaut32's proxy and stub (the stubless proxy thunks, the [call_as]
 * Invoke proxy, the stub calling this program's object), and DispCallFunc
 * on x86-64 code, on a Wine export and through a vtable, with integer,
 * floating-point and 64-bit arguments and a double result.
 *
 * x86_64-w64-mingw32-gcc -O1 -o rpccom.exe rpccom.c -lole32 -loleaut32 -ladvapi32 -luuid
 * gex-nt-arch: x64
 * gex-nt-libs: -lole32 -loleaut32 -ladvapi32 -luuid
 */
#define COBJMACROS
#include <windows.h>
#include <ole2.h>
#include <oleauto.h>
#include <stdio.h>

static int failures;

static void ok( int cond, const char *what, long long a, long long b )
{
    if (cond) printf( "ok   %s\n", what );
    else
    {
        failures++;
        printf( "FAIL %s (%lld, %lld)\n", what, a, b );
    }
}

/* ---- svcctl ---- */

static void test_svcctl(void)
{
    SC_HANDLE scm, svc;
    SERVICE_STATUS status;
    QUERY_SERVICE_CONFIGW *config;
    DWORD needed = 0, count = 0, resume = 0;
    BYTE *buf;
    BOOL r;

    scm = OpenSCManagerW( NULL, NULL, SC_MANAGER_CONNECT | SC_MANAGER_ENUMERATE_SERVICE );
    ok( scm != NULL, "svcctl: OpenSCManagerW", GetLastError(), 0 );
    if (!scm) return;

    r = EnumServicesStatusExW( scm, SC_ENUM_PROCESS_INFO, SERVICE_WIN32 | SERVICE_DRIVER, SERVICE_STATE_ALL,
                               NULL, 0, &needed, &count, &resume, NULL );
    ok( !r && GetLastError() == ERROR_MORE_DATA && needed > 0, "svcctl: EnumServicesStatusExW size", GetLastError(), needed );
    buf = HeapAlloc( GetProcessHeap(), 0, needed );
    resume = 0;
    r = EnumServicesStatusExW( scm, SC_ENUM_PROCESS_INFO, SERVICE_WIN32 | SERVICE_DRIVER, SERVICE_STATE_ALL,
                               buf, needed, &needed, &count, &resume, NULL );
    ok( r && count > 0, "svcctl: EnumServicesStatusExW", GetLastError(), count );
    HeapFree( GetProcessHeap(), 0, buf );

    svc = OpenServiceW( scm, L"PlugPlay", SERVICE_QUERY_STATUS | SERVICE_QUERY_CONFIG );
    ok( svc != NULL, "svcctl: OpenServiceW PlugPlay", GetLastError(), 0 );
    if (svc)
    {
        r = QueryServiceStatus( svc, &status );
        ok( r && status.dwServiceType != 0, "svcctl: QueryServiceStatus", GetLastError(), status.dwServiceType );
        needed = 0;
        QueryServiceConfigW( svc, NULL, 0, &needed );
        config = HeapAlloc( GetProcessHeap(), 0, needed );
        r = QueryServiceConfigW( svc, config, needed, &needed );
        ok( r && config->lpBinaryPathName && wcsstr( config->lpBinaryPathName, L"plugplay" ) != NULL,
            "svcctl: QueryServiceConfigW", GetLastError(), 0 );
        HeapFree( GetProcessHeap(), 0, config );
        CloseServiceHandle( svc );
    }
    svc = OpenServiceW( scm, L"no such service", SERVICE_QUERY_STATUS );
    ok( !svc && GetLastError() == ERROR_SERVICE_DOES_NOT_EXIST, "svcctl: OpenServiceW error", GetLastError(), 0 );
    CloseServiceHandle( scm );
}

/* ---- an IDispatch object ---- */

struct disp
{
    IDispatch IDispatch_iface;
    LONG ref;
    DWORD thread;  /* the thread its methods ran on */
};

static struct disp *impl( IDispatch *iface ) { return CONTAINING_RECORD( iface, struct disp, IDispatch_iface ); }

static HRESULT WINAPI disp_QueryInterface( IDispatch *iface, REFIID riid, void **out )
{
    if (IsEqualIID( riid, &IID_IUnknown ) || IsEqualIID( riid, &IID_IDispatch ))
    {
        *out = iface;
        IDispatch_AddRef( iface );
        return S_OK;
    }
    *out = NULL;
    return E_NOINTERFACE;
}

static ULONG WINAPI disp_AddRef( IDispatch *iface ) { return InterlockedIncrement( &impl( iface )->ref ); }
static ULONG WINAPI disp_Release( IDispatch *iface ) { return InterlockedDecrement( &impl( iface )->ref ); }

static HRESULT WINAPI disp_GetTypeInfoCount( IDispatch *iface, UINT *count )
{
    impl( iface )->thread = GetCurrentThreadId();
    *count = 7;
    return S_OK;
}

static HRESULT WINAPI disp_GetTypeInfo( IDispatch *iface, UINT index, LCID lcid, ITypeInfo **info )
{
    return E_NOTIMPL;
}

static HRESULT WINAPI disp_GetIDsOfNames( IDispatch *iface, REFIID riid, LPOLESTR *names, UINT count, LCID lcid,
                                          DISPID *ids )
{
    UINT i;
    for (i = 0; i < count; i++) ids[i] = wcscmp( names[i], L"Sum" ) ? DISPID_UNKNOWN : 42;
    return ids[0] == 42 ? S_OK : DISP_E_UNKNOWNNAME;
}

/* DISPID 42: Sum(a, b) = a + b (VT_I4 arguments, in reverse order) */
static HRESULT WINAPI disp_Invoke( IDispatch *iface, DISPID id, REFIID riid, LCID lcid, WORD flags,
                                   DISPPARAMS *params, VARIANT *result, EXCEPINFO *excep, UINT *argerr )
{
    impl( iface )->thread = GetCurrentThreadId();
    if (id != 42) return DISP_E_MEMBERNOTFOUND;
    if (params->cArgs != 2 || V_VT( &params->rgvarg[0] ) != VT_I4 || V_VT( &params->rgvarg[1] ) != VT_I4)
        return DISP_E_BADPARAMCOUNT;
    V_VT( result ) = VT_I4;
    V_I4( result ) = V_I4( &params->rgvarg[0] ) + V_I4( &params->rgvarg[1] );
    return S_OK;
}

static IDispatchVtbl disp_vtbl =
{
    disp_QueryInterface, disp_AddRef, disp_Release,
    disp_GetTypeInfoCount, disp_GetTypeInfo, disp_GetIDsOfNames, disp_Invoke,
};

/* ---- cross-apartment calls ---- */

struct call_args { IStream *stream; struct disp *obj; HANDLE done; };

static DWORD WINAPI mta_thread( void *arg )
{
    struct call_args *a = arg;
    IDispatch *proxy = NULL;
    VARIANT args[2], result;
    DISPPARAMS params = { args, NULL, 2, 0 };
    LPOLESTR name = (LPOLESTR)L"Sum";
    DISPID id = 0;
    UINT count = 0;
    HRESULT hr;

    CoInitializeEx( NULL, COINIT_MULTITHREADED );
    hr = CoGetInterfaceAndReleaseStream( a->stream, &IID_IDispatch, (void **)&proxy );
    ok( hr == S_OK && proxy && proxy != &a->obj->IDispatch_iface, "com: unmarshal a proxy in another apartment", hr, 0 );
    if (hr == S_OK)
    {
        hr = IDispatch_GetTypeInfoCount( proxy, &count );
        ok( hr == S_OK && count == 7, "com: GetTypeInfoCount through the proxy", hr, count );
        ok( a->obj->thread != GetCurrentThreadId(), "com: the call ran in the object's apartment", a->obj->thread, 0 );
        hr = IDispatch_GetIDsOfNames( proxy, &IID_NULL, &name, 1, 0, &id );
        ok( hr == S_OK && id == 42, "com: GetIDsOfNames through the proxy", hr, id );
        V_VT( &args[0] ) = VT_I4; V_I4( &args[0] ) = 40;
        V_VT( &args[1] ) = VT_I4; V_I4( &args[1] ) = 2;
        VariantInit( &result );
        hr = IDispatch_Invoke( proxy, 42, &IID_NULL, 0, DISPATCH_METHOD, &params, &result, NULL, NULL );
        ok( hr == S_OK && V_VT( &result ) == VT_I4 && V_I4( &result ) == 42, "com: Invoke through the proxy", hr, V_I4( &result ) );
        hr = IDispatch_Invoke( proxy, 7, &IID_NULL, 0, DISPATCH_METHOD, &params, &result, NULL, NULL );
        ok( hr == DISP_E_MEMBERNOTFOUND, "com: Invoke's failure through the proxy", hr, 0 );
        IDispatch_Release( proxy );
    }
    CoUninitialize();
    SetEvent( a->done );
    return 0;
}

static void test_com(void)
{
    struct disp obj = { { &disp_vtbl }, 1 };
    struct call_args a = { NULL, &obj };
    HANDLE thread;
    HRESULT hr;
    MSG msg;

    CoInitializeEx( NULL, COINIT_APARTMENTTHREADED );
    hr = CoMarshalInterThreadInterfaceInStream( &IID_IDispatch, (IUnknown *)&obj.IDispatch_iface, &a.stream );
    ok( hr == S_OK, "com: CoMarshalInterThreadInterfaceInStream", hr, 0 );
    if (hr == S_OK)
    {
        a.done = CreateEventW( NULL, TRUE, FALSE, NULL );
        thread = CreateThread( NULL, 0, mta_thread, &a, 0, NULL );
        /* the calls come to this apartment as messages */
        while (MsgWaitForMultipleObjects( 1, &a.done, FALSE, 30000, QS_ALLINPUT ) == WAIT_OBJECT_0 + 1)
            while (PeekMessageW( &msg, NULL, 0, 0, PM_REMOVE )) DispatchMessageW( &msg );
        ok( WaitForSingleObject( a.done, 0 ) == WAIT_OBJECT_0, "com: the other apartment finished", 0, 0 );
        WaitForSingleObject( thread, 5000 );
        CloseHandle( thread );
        CloseHandle( a.done );
    }
    CoUninitialize();
}

/* ---- DispCallFunc ---- */

static double WINAPI mix( int a, double b, float c, LONGLONG d, int e )
{
    return a + b + c + (double)d + e;
}

static void test_dispcallfunc(void)
{
    struct disp obj = { { &disp_vtbl }, 1 };
    VARTYPE types[5] = { VT_I4, VT_R8, VT_R4, VT_I8, VT_I4 };
    VARIANT args[5], *pargs[5], result;
    HRESULT hr;
    int i;

    for (i = 0; i < 5; i++) { VariantInit( &args[i] ); pargs[i] = &args[i]; }
    V_VT( &args[0] ) = VT_I4; V_I4( &args[0] ) = 1;
    V_VT( &args[1] ) = VT_R8; V_R8( &args[1] ) = 0.5;
    V_VT( &args[2] ) = VT_R4; V_R4( &args[2] ) = 0.25f;
    V_VT( &args[3] ) = VT_I8; V_I8( &args[3] ) = 1000000000000LL;
    V_VT( &args[4] ) = VT_I4; V_I4( &args[4] ) = -3;
    VariantInit( &result );
    hr = DispCallFunc( NULL, (ULONG_PTR)mix, CC_STDCALL, VT_R8, 5, types, pargs, &result );
    ok( hr == S_OK && V_VT( &result ) == VT_R8 && V_R8( &result ) == 1000000000000.0 - 2 + 0.75,
        "dispcallfunc: five arguments, a double back", hr, (long long)V_R8( &result ) );

    /* a Wine export: kernel32's */
    hr = DispCallFunc( NULL, (ULONG_PTR)GetProcAddress( GetModuleHandleW( L"kernel32" ), "GetCurrentProcessId" ),
                       CC_STDCALL, VT_UI4, 0, NULL, NULL, &result );
    ok( hr == S_OK && V_UI4( &result ) == GetCurrentProcessId(), "dispcallfunc: a Wine export", hr, V_UI4( &result ) );
    types[0] = VT_BSTR;
    V_VT( &args[0] ) = VT_BSTR; V_BSTR( &args[0] ) = SysAllocString( L"twelve chars" );
    hr = DispCallFunc( NULL, (ULONG_PTR)GetProcAddress( GetModuleHandleW( L"kernel32" ), "lstrlenW" ),
                       CC_STDCALL, VT_I4, 1, types, pargs, &result );
    ok( hr == S_OK && V_I4( &result ) == 12, "dispcallfunc: a Wine export with an argument", hr, V_I4( &result ) );
    SysFreeString( V_BSTR( &args[0] ) );

    /* through the object's vtable: GetTypeInfoCount(&count) */
    {
        UINT count = 0;
        types[0] = VT_BYREF | VT_UI4;
        V_VT( &args[0] ) = VT_BYREF | VT_UI4; V_UINTREF( &args[0] ) = &count;
        hr = DispCallFunc( &obj.IDispatch_iface, offsetof( IDispatchVtbl, GetTypeInfoCount ), CC_STDCALL, VT_ERROR,
                           1, types, pargs, &result );
        ok( hr == S_OK && V_ERROR( &result ) == S_OK && count == 7, "dispcallfunc: through a vtable", hr, count );
    }
}

int main(void)
{
    setvbuf( stdout, NULL, _IONBF, 0 );
    test_svcctl();
    test_dispcallfunc();
    test_com();
    printf( "%d failures\n", failures );
    return failures != 0;
}
