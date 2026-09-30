// C++ exceptions through the x64 SEH unwinder (libgcc/libunwind on
// RtlUnwindEx), destructors during unwinding, rethrow, and threads.
// gex-nt-cc: ARCH-w64-mingw32-g++ -static
#include <cstdio>
#include <stdexcept>
#include <string>
#include <thread>
#include <vector>

static int dtors, failures;
struct Guard { ~Guard() { dtors++; } };
#define CHECK(name, cond) do { bool ok_ = (cond); std::printf("%s %s\n", ok_ ? "ok" : "FAIL", name); if (!ok_) failures++; } while (0)

[[gnu::noinline]] static void deep(int n) {
    Guard g;
    if (n == 0) throw std::runtime_error("bottom");
    deep(n - 1);
}

int main() {
    std::string what;
    try { deep(5); } catch (const std::exception &e) { what = e.what(); }
    CHECK("throw through frames", what == "bottom" && dtors == 6);
    int caught = 0;
    try {
        try { throw 42; } catch (int v) { caught = v; throw; }
    } catch (int v) { caught += v; }
    CHECK("rethrow", caught == 84);
    std::vector<std::thread> ts;
    int results[4] = {};
    for (int i = 0; i < 4; i++)
        ts.emplace_back([i, &results] {
            try { if (i >= 0) throw i * 10; } catch (int v) { results[i] = v + 1; }
        });
    for (auto &t : ts) t.join();
    CHECK("exceptions in threads", results[0] == 1 && results[1] == 11 && results[2] == 21 && results[3] == 31);
    std::printf("%d failures\n", failures);
    return failures;
}
