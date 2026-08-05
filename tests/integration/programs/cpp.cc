#include <cstdio>
#include <string>
#include <vector>
#include <map>
#include <memory>
#include <algorithm>
#include <stdexcept>
#include <sstream>
struct Node { int v; std::unique_ptr<Node> next; Node(int v):v(v){} };
static int depth(const Node* n){ return n? 1+depth(n->next.get()) : 0; }
static void thrower(int n){ if(n==0) throw std::runtime_error("bottom"); thrower(n-1); }
int main(){
    std::vector<int> v; for(int i=0;i<2000;i++) v.push_back((i*7919)%1000);
    std::sort(v.begin(), v.end());
    printf("sorted %zu first=%d last=%d\n", v.size(), v.front(), v.back());

    std::map<std::string,int> m;
    for(int i=0;i<200;i++){ std::ostringstream o; o<<"key"<<(i%37); m[o.str()]+=i; }
    printf("map size=%zu key0=%d\n", m.size(), m["key0"]);

    std::string s;
    for(int i=0;i<50;i++) s += std::to_string(i) + ",";
    printf("string len=%zu tail=%s\n", s.size(), s.c_str()+s.size()-8);

    auto head = std::make_unique<Node>(0);
    Node* cur = head.get();
    for(int i=1;i<100;i++){ cur->next = std::make_unique<Node>(i); cur = cur->next.get(); }
    printf("list depth=%d\n", depth(head.get()));

    try { thrower(20); } catch (const std::exception& e) { printf("caught: %s\n", e.what()); }
    try { v.at(99999); } catch (const std::out_of_range&) { printf("caught out_of_range\n"); }
    int nested=0;
    try { try { throw 42; } catch(int x){ nested=x; throw std::logic_error("rethrown"); } }
    catch(const std::logic_error& e){ printf("nested=%d then %s\n", nested, e.what()); }
    printf("done\n");
    return 0;
}
