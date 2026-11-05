/* C++ exceptions in a gexcc-win program: an indirect call (a function
 * pointer, a virtual call) made inside a catch block reaches its target,
 * a rethrow and a nested try inside a catch, a std::string thrown. */
#include <cstdio>
#include <stdexcept>
#include <string>

static int failures;
static void check(bool ok, const char *what)
{
    std::printf("%s %s\n", ok ? "ok" : "FAIL", what);
    if (!ok) failures++;
}

static int seen;
static void note(int v) { seen = v; }
static void (*volatile note_ptr)(int) = note;

struct Base { virtual ~Base() {} virtual int value() const { return 1; } };
struct Derived : Base { int value() const override { return 7; } };
static Base *volatile obj;

static void thrower(int k)
{
    if (k == 1) throw std::runtime_error("one");
    if (k == 2) throw std::string("two");
    if (k == 3) throw 3;
}

int main()
{
    Derived d; obj = &d;
    try { thrower(1); } catch (const std::exception &e) { note_ptr(41); }
    check(seen == 41, "a function pointer called in a catch block");
    int v = 0;
    try { thrower(3); } catch (int) { v = obj->value(); }
    check(v == 7, "a virtual call in a catch block");
    std::string s;
    try {
        try { thrower(2); } catch (...) { note_ptr(5); throw; }
    } catch (const std::string &e) { s = e; }
    check(seen == 5 && s == "two", "a rethrow from a catch block");
    int inner = 0;
    try { thrower(1); } catch (const std::runtime_error &) {
        try { thrower(3); } catch (int k) { inner = k; note_ptr(9); }
    }
    check(inner == 3 && seen == 9, "a try nested in a catch block");
    std::printf("%d failures\n", failures);
    return failures != 0;
}
