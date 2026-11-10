// What libstdc++ accepts and libc++ needs told: strings and views of
// unsigned char (char_traits<unsigned char>), a deque of a type complete
// only later, and a non-constant narrowing in a braced initializer (a
// warning, as with GCC).
#include <cstdio>
#include <deque>
#include <string>
#include <string_view>

struct Later;
struct Holder {
	std::deque<Later> q;
};
struct Later {
	int v;
};

int main(int argc, char **)
{
	std::basic_string<unsigned char> s{'a', 'b', 'c'};
	std::basic_string_view<unsigned char> v(s);
	Holder h;
	h.q.push_back({7});
	long long off = argc + 41;
	unsigned long n{off};
	std::printf("bytes %zu %d deque %d narrow %lu\n", v.size(), v.compare(s), h.q.front().v, n);
	return 0;
}
