/* _com_ptr_t and _com_IIID, as much of them as programs built for MinGW
 * use (gexcc-win's compatibility headers: Wine's include tree has no COM
 * support classes). No exceptions: a failed QueryInterface leaves it null. */
#pragma once
#include <objbase.h>
#include <cstddef>

template <typename T, const IID *piid> class _com_IIID
{
public:
    typedef T Interface;
    static const IID &GetIID() { return *piid; }
};

template <typename IIID> class _com_ptr_t
{
public:
    typedef IIID ThisIIID;
    typedef typename IIID::Interface Interface;

    _com_ptr_t() noexcept : p(nullptr) {}
    _com_ptr_t(std::nullptr_t) noexcept : p(nullptr) {}
    _com_ptr_t(Interface *q) noexcept : p(q) { if (p) p->AddRef(); }
    _com_ptr_t(Interface *q, bool addref) noexcept : p(q) { if (p && addref) p->AddRef(); }
    template <typename Q> _com_ptr_t(Q *q) noexcept : p(nullptr) { Query(q); }
    _com_ptr_t(const _com_ptr_t &o) noexcept : p(o.p) { if (p) p->AddRef(); }
    template <typename O> _com_ptr_t(const _com_ptr_t<O> &o) noexcept : p(nullptr) { Query(o.GetInterfacePtr()); }
    _com_ptr_t(const CLSID &clsid, IUnknown *outer = nullptr, DWORD ctx = CLSCTX_ALL) noexcept : p(nullptr)
    { CreateInstance(clsid, outer, ctx); }
    ~_com_ptr_t() { Release(); }

    _com_ptr_t &operator=(Interface *q) noexcept { if (q) q->AddRef(); Release(); p = q; return *this; }
    template <typename Q> _com_ptr_t &operator=(Q *q) noexcept { Release(); Query(q); return *this; }
    _com_ptr_t &operator=(const _com_ptr_t &o) noexcept { return *this = o.p; }
    template <typename O> _com_ptr_t &operator=(const _com_ptr_t<O> &o) noexcept { Release(); Query(o.GetInterfacePtr()); return *this; }
    _com_ptr_t &operator=(std::nullptr_t) noexcept { Release(); return *this; }

    HRESULT CreateInstance(const CLSID &clsid, IUnknown *outer = nullptr, DWORD ctx = CLSCTX_ALL) noexcept
    {
        Release();
        return CoCreateInstance(clsid, outer, ctx, IIID::GetIID(), reinterpret_cast<void **>(&p));
    }
    void Attach(Interface *q) noexcept { Release(); p = q; }
    void Attach(Interface *q, bool addref) noexcept { if (q && addref) q->AddRef(); Release(); p = q; }
    Interface *Detach() noexcept { Interface *t = p; p = nullptr; return t; }
    void Release() noexcept { if (p) { Interface *t = p; p = nullptr; t->Release(); } }
    void AddRef() noexcept { if (p) p->AddRef(); }
    Interface *GetInterfacePtr() const noexcept { return p; }
    Interface *&GetInterfacePtr() noexcept { return p; }
    template <typename Q> HRESULT QueryInterface(const IID &iid, Q *&q) const noexcept
    { return p ? p->QueryInterface(iid, reinterpret_cast<void **>(&q)) : E_POINTER; }
    template <typename Q> HRESULT QueryInterface(const IID &iid, Q **q) const noexcept
    { return p ? p->QueryInterface(iid, reinterpret_cast<void **>(q)) : E_POINTER; }

    operator Interface *() const noexcept { return p; }
    Interface &operator*() const noexcept { return *p; }
    Interface **operator&() noexcept { Release(); return &p; }
    Interface *operator->() const noexcept { return p; }
    bool operator!() const noexcept { return !p; }

private:
    Interface *p;
    template <typename Q> void Query(Q *q) noexcept
    {
        if (q && q->QueryInterface(IIID::GetIID(), reinterpret_cast<void **>(&p)) != S_OK) p = nullptr;
    }
};
