/* Microsoft::WRL::ComPtr, as much of it as programs built for MinGW use
 * (gexcc-win's compatibility headers: Wine's include tree has no WRL). */
#pragma once
#include <unknwn.h>
#include <cstddef>
#include <utility>
#include <type_traits>

namespace Microsoft { namespace WRL {

template <typename C> class ComPtrRef
{
    C *c;
public:
    typedef typename C::InterfaceType T;
    ComPtrRef(C *c) noexcept : c(c) {}
    operator T **() noexcept { return c->ReleaseAndGetAddressOf(); }
    operator void **() noexcept { return reinterpret_cast<void **>(c->ReleaseAndGetAddressOf()); }
    template <typename I = T, typename = typename std::enable_if<!std::is_same<I, IUnknown>::value>::type>
    operator IUnknown **() noexcept { return reinterpret_cast<IUnknown **>(c->ReleaseAndGetAddressOf()); }
    operator C *() noexcept { c->Reset(); return c; }
    T *operator*() noexcept { return c->Get(); }
};

template <typename T> class ComPtr
{
public:
    typedef T InterfaceType;
    ComPtr() noexcept : p(nullptr) {}
    ComPtr(std::nullptr_t) noexcept : p(nullptr) {}
    template <typename U> ComPtr(U *q) noexcept : p(q) { if (p) p->AddRef(); }
    ComPtr(const ComPtr &o) noexcept : p(o.p) { if (p) p->AddRef(); }
    template <typename U> ComPtr(const ComPtr<U> &o) noexcept : p(o.Get()) { if (p) p->AddRef(); }
    ComPtr(ComPtr &&o) noexcept : p(o.p) { o.p = nullptr; }
    template <typename U> ComPtr(ComPtr<U> &&o) noexcept : p(o.Detach()) {}
    ~ComPtr() { InternalRelease(); }
    ComPtr &operator=(std::nullptr_t) noexcept { InternalRelease(); return *this; }
    ComPtr &operator=(T *q) noexcept { if (p != q) { ComPtr(q).Swap(*this); } return *this; }
    ComPtr &operator=(const ComPtr &o) noexcept { if (p != o.p) { ComPtr(o).Swap(*this); } return *this; }
    template <typename U> ComPtr &operator=(const ComPtr<U> &o) noexcept { ComPtr(o).Swap(*this); return *this; }
    ComPtr &operator=(ComPtr &&o) noexcept { ComPtr(std::move(o)).Swap(*this); return *this; }
    template <typename U> ComPtr &operator=(ComPtr<U> &&o) noexcept { ComPtr(std::move(o)).Swap(*this); return *this; }
    void Swap(ComPtr &o) noexcept { T *t = p; p = o.p; o.p = t; }
    void Swap(ComPtr &&o) noexcept { T *t = p; p = o.p; o.p = t; }
    operator bool() const noexcept { return p != nullptr; }
    T *Get() const noexcept { return p; }
    T *operator->() const noexcept { return p; }
    T *const *GetAddressOf() const noexcept { return &p; }
    T **GetAddressOf() noexcept { return &p; }
    T **ReleaseAndGetAddressOf() noexcept { InternalRelease(); return &p; }
    ComPtrRef<ComPtr> operator&() noexcept { return ComPtrRef<ComPtr>(this); }
    template <typename U> HRESULT As(ComPtrRef<ComPtr<U>> o) const noexcept { return As(static_cast<ComPtr<U> *>(o)); }
    T *Detach() noexcept { T *t = p; p = nullptr; return t; }
    void Attach(T *q) noexcept { if (p) p->Release(); p = q; }
    unsigned long Reset() noexcept { return InternalRelease(); }
    template <typename U> HRESULT As(ComPtr<U> *o) const noexcept
    { return p->QueryInterface(__uuidof(U), reinterpret_cast<void **>(o->ReleaseAndGetAddressOf())); }
    template <typename U> HRESULT As(U **o) const noexcept
    { return p->QueryInterface(__uuidof(U), reinterpret_cast<void **>(o)); }
    HRESULT AsIID(REFIID riid, ComPtr<IUnknown> *o) const noexcept
    { return p->QueryInterface(riid, reinterpret_cast<void **>(o->ReleaseAndGetAddressOf())); }
    HRESULT CopyTo(T **o) const noexcept { if (p) p->AddRef(); *o = p; return S_OK; }
    template <typename U> HRESULT CopyTo(U **o) const noexcept
    { return p->QueryInterface(__uuidof(U), reinterpret_cast<void **>(o)); }
protected:
    T *p;
    template <typename U> friend class ComPtr;
    unsigned long InternalRelease() noexcept { unsigned long r = 0; T *t = p; if (t) { p = nullptr; r = t->Release(); } return r; }
};

template <typename T, typename U> bool operator==(const ComPtr<T> &a, const ComPtr<U> &b) noexcept { return a.Get() == b.Get(); }
template <typename T> bool operator==(const ComPtr<T> &a, std::nullptr_t) noexcept { return a.Get() == nullptr; }
template <typename T, typename U> bool operator!=(const ComPtr<T> &a, const ComPtr<U> &b) noexcept { return a.Get() != b.Get(); }
template <typename T> bool operator!=(const ComPtr<T> &a, std::nullptr_t) noexcept { return a.Get() != nullptr; }

} } /* namespace Microsoft::WRL */
