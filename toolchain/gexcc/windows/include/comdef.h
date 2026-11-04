/* _com_error and the COM support classes' header (gexcc-win's compatibility
 * headers): what MinGW programs include for _com_ptr_t. */
#pragma once
#include <comip.h>
#include <oleauto.h>

class _com_error
{
public:
    explicit _com_error(HRESULT hr) noexcept : hr(hr) {}
    HRESULT Error() const noexcept { return hr; }
    const TCHAR *ErrorMessage() const noexcept { return TEXT(""); }
private:
    HRESULT hr;
};
