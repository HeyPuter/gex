// C++ on gexcc: exceptions (through frames, rethrow, std types), RTTI,
// iostreams, containers, std::thread, static constructors.
#include <iostream>
#include <map>
#include <memory>
#include <mutex>
#include <sstream>
#include <stdexcept>
#include <string>
#include <thread>
#include <typeinfo>
#include <vector>

struct Base { virtual ~Base() = default; virtual std::string name() const { return "base"; } };
struct Derived : Base { std::string name() const override { return "derived"; } };
struct Noisy { std::string n; explicit Noisy(std::string s) : n(std::move(s)) {} ~Noisy() { std::cout << "~" << n << "\n"; } };

static std::vector<int> ctor_order;
struct Init { Init() { ctor_order.push_back(1); } } init_obj;

static int depth(int n) {
    Noisy guard("frame" + std::to_string(n));
    if (n == 0) throw std::runtime_error("bottom");
    return depth(n - 1) + 1;
}

int main() {
    try { depth(3); } catch (const std::exception &e) { std::cout << "caught " << e.what() << "\n"; }
    try { try { throw 42; } catch (int v) { std::cout << "int " << v << "\n"; throw; } }
    catch (...) { std::cout << "rethrown\n"; }
    try { std::vector<int> v; v.at(5); } catch (const std::out_of_range &) { std::cout << "out_of_range\n"; }
    std::unique_ptr<Base> b = std::make_unique<Derived>();
    std::cout << b->name() << " " << (dynamic_cast<Derived *>(b.get()) != nullptr) << " " << (typeid(*b) == typeid(Derived)) << "\n";
    std::map<std::string, int> m{{"a", 1}, {"b", 2}};
    std::ostringstream os; for (auto &[k, v] : m) os << k << "=" << v << ";";
    std::cout << os.str() << " ctors " << ctor_order.size() << "\n";
    std::mutex mu; long total = 0; std::vector<std::thread> ts;
    for (int i = 0; i < 4; i++) ts.emplace_back([&, i] { for (int j = 0; j < 1000; j++) { std::lock_guard<std::mutex> l(mu); total += i; } });
    for (auto &t : ts) t.join();
    std::cout << "threads " << total << "\n";
    try { std::thread([] {}).join(); throw std::logic_error("after thread"); } catch (const std::logic_error &e) { std::cout << e.what() << "\n"; }
    return 0;
}
