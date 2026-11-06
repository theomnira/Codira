// AURORA: formatting, the C++ baseline of src/mod.code (the same values, the
// same text, the same check values): std::to_string, std::to_chars and
// std::format.
#include <charconv>
#include <format>

#include "../common/bench.hpp"

using namespace bench;

static double value(int64_t i) { return (double)((i * 7919) % 1000003) / 997.0 - 300.0; }
static int64_t integer(int64_t i) { return (i * 7919) % 1000003 - 500000; }
static uint64_t check(const std::string& s) { return s.size() + (unsigned char)s.back(); }

// The shortest round-trip text with a `.0` on whole numbers, as Codira's
// `to_string` and Rust's `{:?}` write it (no value here needs an exponent).
static std::string shortest(double x) {
    char buf[64];
    auto r = std::to_chars(buf, buf + sizeof buf, x);
    std::string s(buf, r.ptr);
    if (s.find_first_of(".en") == std::string::npos) s += ".0";
    return s;
}

template <class F>
static void run(F one) {
    auto start = now();
    uint64_t sum = 0;
    for (int64_t i = 0; i < 1000000; i++) {
        int64_t v = i;
        keep(v);
        sum += check(one(v));
    }
    report(start, sum);
}

int main(int argc, char** argv) {
    if (argc < 2) unknown("");
    std::string k = argv[1];
    if (k == "int_display") run([](int64_t i) { return std::to_string(integer(i)); });
    else if (k == "int_zero_8") run([](int64_t i) { return std::format("{:08}", integer(i)); });
    else if (k == "float_display") run([](int64_t i) { return shortest(value(i)); });
    else if (k == "float_right_10_3") run([](int64_t i) { return std::format("{:>10.3f}", value(i)); });
    else if (k == "debug_struct")
        run([](int64_t i) { return "Point { x: " + std::to_string(i) + ", y: " + shortest(value(i)) + " }"; });
    else unknown(k);
}
