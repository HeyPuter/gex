/* What MSVC's and MinGW's headers give C++ that Wine's do not (gexcc-win
 * force-includes it in C++): the secure CRT functions' array templates.
 * (math.h first: libc++'s lround and lrint call the C runtime's, gexcc-win's
 * -D__builtin_lround=lround and the rest, declared there.) */
#pragma once
#include <math.h>
#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <wchar.h>

extern "C++" {
template <size_t N> inline errno_t strcpy_s(char (&d)[N], const char *s) { return strcpy_s(d, N, s); }
template <size_t N> inline errno_t strcat_s(char (&d)[N], const char *s) { return strcat_s(d, N, s); }
template <size_t N> inline errno_t strncpy_s(char (&d)[N], const char *s, size_t n) { return strncpy_s(d, N, s, n); }
template <size_t N> inline errno_t strncat_s(char (&d)[N], const char *s, size_t n) { return strncat_s(d, N, s, n); }
template <size_t N> inline errno_t wcscpy_s(wchar_t (&d)[N], const wchar_t *s) { return wcscpy_s(d, N, s); }
template <size_t N> inline errno_t wcsncpy_s(wchar_t (&d)[N], const wchar_t *s, size_t n) { return wcsncpy_s(d, N, s, n); }
template <size_t N> inline errno_t wcsncat_s(wchar_t (&d)[N], const wchar_t *s, size_t n) { return wcsncat_s(d, N, s, n); }
template <size_t N> inline int sprintf_s(char (&b)[N], const char *f, ...)
{ va_list a; va_start(a, f); int r = vsprintf_s(b, N, f, a); va_end(a); return r; }
template <size_t N> inline int swprintf_s(wchar_t (&b)[N], const wchar_t *f, ...)
{ va_list a; va_start(a, f); int r = vswprintf_s(b, N, f, a); va_end(a); return r; }
template <size_t N> inline int _snprintf_s(char (&b)[N], size_t n, const char *f, ...)
{ va_list a; va_start(a, f); int r = _vsnprintf_s(b, N, n, f, a); va_end(a); return r; }
template <size_t N> inline int _snwprintf_s(wchar_t (&b)[N], size_t n, const wchar_t *f, ...)
{ va_list a; va_start(a, f); int r = _vsnwprintf_s(b, N, n, f, a); va_end(a); return r; }
template <size_t N> inline int vsprintf_s(char (&b)[N], const char *f, va_list a) { return vsprintf_s(b, N, f, a); }
template <size_t N> inline int vswprintf_s(wchar_t (&b)[N], const wchar_t *f, va_list a) { return vswprintf_s(b, N, f, a); }
template <size_t N> inline errno_t _itoa_s(int v, char (&b)[N], int r) { return _itoa_s(v, b, N, r); }
template <size_t N> inline errno_t _itow_s(int v, wchar_t (&b)[N], int r) { return _itow_s(v, b, N, r); }
template <size_t N> inline errno_t _ltow_s(long v, wchar_t (&b)[N], int r) { return _ltow_s(v, b, N, r); }
template <size_t N> inline errno_t _ultow_s(unsigned long v, wchar_t (&b)[N], int r) { return _ultow_s(v, b, N, r); }
template <size_t N> inline errno_t _wcslwr_s(wchar_t (&b)[N]) { return _wcslwr_s(b, N); }
template <size_t N> inline errno_t _wcsupr_s(wchar_t (&b)[N]) { return _wcsupr_s(b, N); }
template <size_t N> inline errno_t _strlwr_s(char (&b)[N]) { return _strlwr_s(b, N); }
template <size_t N> inline errno_t _strupr_s(char (&b)[N]) { return _strupr_s(b, N); }
template <size_t N> inline errno_t mbstowcs_s(size_t *r, wchar_t (&d)[N], const char *s, size_t n) { return mbstowcs_s(r, d, N, s, n); }
template <size_t N> inline errno_t wcstombs_s(size_t *r, char (&d)[N], const wchar_t *s, size_t n) { return wcstombs_s(r, d, N, s, n); }

}
