// AURORA: hash maps, the C++ baseline of src/mod.code: std::unordered_map
// (a node per entry, chained buckets) with Codira's hash functions (the
// MurmurHash3 finalizer for integers, FNV-1a 64 for strings). The standard
// library has no seeded hasher (libstdc++'s std::hash<int64_t> is the
// identity), so there is no C++ variant of the *_secure kernels.
#include <unordered_map>

#include "../common/bench.hpp"

using namespace bench;

static uint64_t mix64(uint64_t x) {
    x ^= x >> 33;
    x *= 0xFF51AFD7ED558CCDull;
    x ^= x >> 33;
    x *= 0xC4CEB9FE1A85EC53ull;
    return x ^ (x >> 33);
}
struct IntHash { size_t operator()(int64_t v) const { return mix64((uint64_t)v); } };
struct StrHash {
    size_t operator()(const std::string& s) const {
        uint64_t h = 0xCBF29CE484222325ull;
        for (unsigned char b : s) { h ^= b; h *= 0x100000001B3ull; }
        return h;
    }
};
using IntMap = std::unordered_map<int64_t, int64_t, IntHash>;
using StrMap = std::unordered_map<std::string, int64_t, StrHash>;

static std::vector<int64_t> int_keys() {
    Rng r;
    std::vector<int64_t> k;
    for (int i = 0; i < 1000000; i++) k.push_back((int64_t)(r.next() >> 1));
    return k;
}
static std::vector<std::string> string_keys() {
    Rng r;
    std::vector<std::string> k;
    for (int i = 0; i < 200000; i++) k.push_back("user:" + std::to_string(r.next() % 100000000));
    return k;
}
static IntMap build_ints(const std::vector<int64_t>& keys) {
    IntMap m;
    for (int64_t k : keys) m.insert_or_assign(k, k ^ 5);
    return m;
}
static StrMap build_strings(const std::vector<std::string>& keys) {
    StrMap m;
    int64_t i = 0;
    for (auto& k : keys) m.insert_or_assign(k, i++);
    return m;
}

int main(int argc, char** argv) {
    if (argc < 2) unknown("");
    std::string k = argv[1];
    if (k == "insert_i64") {
        auto keys = int_keys();
        auto start = now();
        auto m = build_ints(keys);
        report(start, m.size());
    } else if (k == "lookup_i64") {
        auto keys = int_keys();
        auto m = build_ints(keys);
        auto start = now();
        uint64_t found = 0;
        for (int64_t key : keys) {
            auto it = m.find(key);
            if (it != m.end()) found += (it->second & 1) + 1;
        }
        report(start, found);
    } else if (k == "insert_string") {
        auto keys = string_keys();
        auto start = now();
        auto m = build_strings(keys);
        report(start, m.size());
    } else if (k == "lookup_string") {
        auto keys = string_keys();
        auto m = build_strings(keys);
        auto start = now();
        uint64_t found = 0;
        for (int r = 0; r < 5; r++)
            for (auto& key : keys) {
                auto it = m.find(key);
                if (it != m.end()) found += (it->second & 7) + 1;
            }
        report(start, found);
    } else if (k == "churn_i64") {
        Rng r;
        std::vector<int64_t> ring(65536);
        IntMap m;
        for (auto& slot : ring) {
            int64_t key = (int64_t)(r.next() >> 1);
            slot = key;
            m.insert_or_assign(key, key);
        }
        auto start = now();
        uint64_t found = 0;
        for (size_t step = 0; step < 2000000; step++) {
            size_t slot = step & 65535;
            m.erase(ring[slot]);
            int64_t key = (int64_t)(r.next() >> 1);
            ring[slot] = key;
            m.insert_or_assign(key, key);
            auto it = m.find(ring[(step * 7) & 65535]);
            if (it != m.end()) found += (it->second & 1) + 1;
        }
        report(start, found * 1000000 + m.size());
    } else unknown(k);
}
