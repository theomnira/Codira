// AURORA: arrays and pipelines, the C++ baseline of src/mod.code
// (std::vector, std::ranges views; the same inputs, the same check values).
#include <numeric>
#include <ranges>

#include "../common/bench.hpp"

using namespace bench;
namespace vw = std::views;

static const int64_t N = 10000000;

static std::vector<int64_t> iota() {
    std::vector<int64_t> v(N);
    std::iota(v.begin(), v.end(), 0);
    keep(v);
    return v;
}
static std::vector<int64_t> randoms() {
    Rng r;
    std::vector<int64_t> v;
    for (int64_t i = 0; i < N; i++) v.push_back((int64_t)(r.next() % 1000));
    keep(v);
    return v;
}
static std::vector<double> floats(uint64_t seed) {
    Rng r{seed};
    std::vector<double> v;
    for (int64_t i = 0; i < N; i++) v.push_back((double)(r.next() >> 11) / 9007199254740992.0);
    keep(v);
    return v;
}

template <class R>
static int64_t sum(R&& r) {
    int64_t s = 0;
    for (int64_t x : r) s += x;
    return s;
}

int main(int argc, char** argv) {
    if (argc < 2) unknown("");
    std::string k = argv[1];
    if (k == "push_10m") {
        auto start = now();
        std::vector<int64_t> xs;
        int64_t n = N;
        keep(n);
        for (int64_t i = 0; i < n; i++) xs.push_back(i);
        keep(xs);
        report(start, xs.size());
    } else if (k == "sum_for") {
        auto xs = iota();
        auto start = now();
        int64_t s = 0;
        for (int64_t x : xs) s += x;
        report(start, (uint64_t)s);
    } else if (k == "sort_1m") {
        Rng r;
        std::vector<int64_t> ys;
        for (int i = 0; i < 1000000; i++) ys.push_back((int64_t)(r.next() >> 1));
        auto start = now();
        std::sort(ys.begin(), ys.end());
        report(start, (uint64_t)ys[500000]);
    } else if (k == "map_filter_sum_eager") {
        auto xs = iota();
        auto start = now();
        std::vector<int64_t> a;
        a.reserve(xs.size());
        for (int64_t x : xs) a.push_back(x * 2);
        std::vector<int64_t> b;
        for (int64_t x : a) if (x % 3 == 0) b.push_back(x);
        keep(b);
        report(start, (uint64_t)sum(b));
    } else if (k == "map_filter_sum_lazy") {
        auto xs = iota();
        auto start = now();
        auto s = sum(xs | vw::transform([](int64_t x) { return x * 2; }) |
                     vw::filter([](int64_t x) { return x % 3 == 0; }));
        report(start, (uint64_t)s);
    } else if (k == "map_filter_sum_fold") {
        auto xs = iota();
        auto start = now();
        int64_t s = std::accumulate(xs.begin(), xs.end(), (int64_t)0,
                                    [](int64_t acc, int64_t x) { return (x * 2) % 3 == 0 ? acc + x * 2 : acc; });
        report(start, (uint64_t)s);
    } else if (k == "enumerate_filter_count") {
        auto rs = randoms();
        auto start = now();
        auto e = vw::enumerate(rs) | vw::filter([](auto p) { return ((int64_t)std::get<0>(p) + std::get<1>(p)) % 5 == 0; });
        uint64_t c = std::ranges::distance(e);
        report(start, c);
    } else if (k == "zip_fold_dot") {
        auto fs = floats(0x9E3779B97F4A7C15ull);
        auto gs = floats(0x2545F4914F6CDD1Dull);
        auto start = now();
        double c = 0.0;
        for (auto [a, b] : vw::zip(fs, gs)) c = c + a * b;
        report(start, (uint64_t)(c * 1000.0));
    } else if (k == "take_while_sum") {
        auto xs = iota();
        int64_t n = N;
        keep(n);
        auto start = now();
        auto d = sum(xs | vw::take_while([n](int64_t x) { return x < n - 1000; }));
        report(start, (uint64_t)d);
    } else if (k == "map_collect") {
        auto xs = iota();
        auto start = now();
        auto m = xs | vw::transform([](int64_t x) { return x * 3 + 1; });
        // The length is known, as for Rust's `collect` and Codira's: one
        // allocation. (The vector's iterator-pair constructor sees a
        // transform view's iterators as input iterators and would grow.)
        std::vector<int64_t> e;
        e.reserve(m.size());
        for (int64_t v : m) e.push_back(v);
        keep(e);
        report(start, (uint64_t)(e[e.size() / 2] + (int64_t)e.size()));
    } else if (k == "filter_collect") {
        auto rs = randoms();
        auto start = now();
        auto m = rs | vw::filter([](int64_t x) { return x % 3 == 0; });
        std::vector<int64_t> f(m.begin(), m.end());  // not sized: grows
        keep(f);
        report(start, (uint64_t)(f[f.size() / 2] + (int64_t)f.size()));
    } else if (k == "range_filter_map_sum") {
        int64_t n = N;
        keep(n);
        auto start = now();
        auto g = sum(vw::iota((int64_t)0, n) | vw::filter([](int64_t i) { return i % 3 == 0; }) |
                     vw::transform([](int64_t i) { return i ^ (i >> 3); }));
        report(start, (uint64_t)g);
    } else if (k == "chain_filter_count") {
        auto xs = iota();
        auto rs = randoms();
        auto start = now();
        uint64_t h = 0;
        for (int64_t x : xs) h += x % 7 == 1;
        for (int64_t x : rs) h += x % 7 == 1;
        report(start, h);
    } else unknown(k);
}
