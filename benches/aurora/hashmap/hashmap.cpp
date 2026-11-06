// AURORA: hash maps, the C++ baseline of src/mod.code for the `fast`
// kernels: std::unordered_map (a node per entry, chained buckets) with
// Codira's unkeyed hash functions (the MurmurHash3 finalizer for integers,
// FNV-1a 64 for strings). The standard library has no seeded hasher
// (libstdc++'s std::hash<int64_t> is the identity), so there is no C++
// variant of the `secure` kernels. Kernel names as in hashmap.rs.
#include <string_view>
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

static void report_ns(uint64_t ns, int64_t check) {
    std::printf("@@ ns=%llu check=%llu\n", (unsigned long long)ns, (unsigned long long)check);
    peak_rss();
}

template <class K> K make(int64_t k);
template <> int64_t make<int64_t>(int64_t k) { return k; }
template <> std::string make<std::string>(int64_t k) { return "key-" + std::to_string(k); }
static int64_t weight(int64_t k) { return k; }
static int64_t weight(const std::string& k) { return (int64_t)k.size(); }

template <class K, class H>
static void run(const std::string& op, size_t n, bool ints) {
    Rng r;
    std::vector<int64_t> hits(n), miss(n);
    for (auto& k : hits) k = (int64_t)((r.next() >> 2) * 2);
    for (auto& k : miss) k = (int64_t)((r.next() >> 2) * 2 + 1);
    std::vector<K> keys, misses;
    for (auto k : hits) keys.push_back(make<K>(k));
    for (auto k : miss) misses.push_back(make<K>(k));
    auto value = [&](size_t i) { return ints ? hits[i] ^ 5 : (int64_t)i; };
    int reps = n <= 1000 ? 1000 : n <= 100000 ? 10 : 1;
    using Map = std::unordered_map<K, int64_t, H>;
    auto fill = [&](Map& m) { for (size_t i = 0; i < keys.size(); i++) m.insert_or_assign(keys[i], value(i)); };
    Map m;
    if (op != "insert") fill(m);
    uint64_t ns = 0;
    int64_t check = 0;
    if (op == "insert") {
        for (int rep = 0; rep < reps; rep++) {
            auto t = now();
            m = Map();
            fill(m);
            ns += elapsed_ns(t);
            check += (int64_t)m.size();
        }
    } else if (op == "hit") {
        auto t = now();
        for (int rep = 0; rep < reps; rep++)
            for (auto& k : keys) { auto it = m.find(k); if (it != m.end()) check += it->second & 1; }
        ns = elapsed_ns(t);
    } else if (op == "miss") {
        auto t = now();
        for (int rep = 0; rep < reps; rep++)
            for (auto& k : misses) if (m.contains(k)) check++;
        ns = elapsed_ns(t);
    } else if (op == "iter") {
        auto t = now();
        for (int rep = 0; rep < reps; rep++)
            for (auto& [k, v] : m) check += ints ? (weight(k) ^ v) & 7 : (weight(k) + v) & 7;
        ns = elapsed_ns(t);
    } else if (op == "remove") {
        for (int rep = 0; rep < reps; rep++) {
            if (rep > 0) fill(m);
            auto t = now();
            for (auto& k : keys) {
                auto it = m.find(k);
                if (it != m.end()) { check += it->second & 1; m.erase(it); }
            }
            ns += elapsed_ns(t);
        }
        check += (int64_t)m.size();
    } else unknown(op);
    report_ns(ns, check);
}

int main(int argc, char** argv) {
    if (argc < 2) unknown("");
    std::string k = argv[1];
    std::vector<std::string> p;
    for (std::string_view rest = k;;) {
        size_t at = rest.find('_');
        p.emplace_back(rest.substr(0, at));
        if (at == std::string_view::npos) break;
        rest.remove_prefix(at + 1);
    }
    if (p.size() == 3 && p[0] == "small" && p[2] == "fast") {
        const int64_t keys[5] = {17, 4242, -9, 123456789, 77};
        int64_t check = 0;
        if (p[1] == "build") {
            auto t = now();
            for (int i = 0; i < 200000; i++) {
                std::unordered_map<int64_t, int64_t, IntHash> m;
                for (int64_t key : keys) m.insert_or_assign(key, key ^ 5);
                check += (int64_t)m.size();
            }
            report_ns(elapsed_ns(t), check);
        } else {
            std::unordered_map<int64_t, int64_t, IntHash> m;
            for (int64_t key : keys) m.insert_or_assign(key, key ^ 5);
            int64_t count = 1000000;
            keep(count);
            auto t = now();
            for (int64_t i = 0; i < count; i++) {
                auto it = m.find(keys[(i % 8) % 5] + (i & 8));
                if (it != m.end()) check += it->second & 3;
            }
            report_ns(elapsed_ns(t), check);
        }
    } else if (p.size() == 2 && p[0] == "churn" && p[1] == "fast") {
        Rng r;
        std::vector<int64_t> ring(65536);
        std::unordered_map<int64_t, int64_t, IntHash> m;
        for (auto& slot : ring) { int64_t key = (int64_t)(r.next() >> 1); slot = key; m.insert_or_assign(key, key); }
        auto t = now();
        int64_t found = 0;
        for (size_t step = 0; step < 2000000; step++) {
            size_t slot = step & 65535;
            m.erase(ring[slot]);
            int64_t key = (int64_t)(r.next() >> 1);
            ring[slot] = key;
            m.insert_or_assign(key, key);
            auto it = m.find(ring[(step * 7) & 65535]);
            if (it != m.end()) found += (it->second & 1) + 1;
        }
        report_ns(elapsed_ns(t), found * 1000000 + (int64_t)m.size());
    } else if (p.size() == 4 && p[2] == "fast") {
        size_t n = p[3] == "1k" ? 1000 : p[3] == "100k" ? 100000 : p[3] == "1m" ? 1000000 : 0;
        if (!n) unknown(k);
        if (p[1] == "i64") run<int64_t, IntHash>(p[0], n, true);
        else if (p[1] == "string") run<std::string, StrHash>(p[0], n, false);
        else unknown(k);
    } else unknown(k);
}
