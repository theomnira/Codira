// AURORA: Result and `?`, the C++ baseline of src/mod.code on
// std::expected: the same parser, the same error types (an AnyError with a
// message, a context list and a type name), the same check values. C++ has
// no `?`; each frame tests and returns by hand.
#include <expected>
#include <variant>

#include "../common/bench.hpp"

using namespace bench;

struct ParseError {
    enum Kind { Empty, InvalidDigit, Overflow } kind;
    size_t index = 0;
    std::string message() const {
        switch (kind) {
            case Empty: return "no digits";
            case InvalidDigit: return "invalid digit at byte " + std::to_string(index);
            default: return "number too large";
        }
    }
};

static std::expected<int64_t, ParseError> parse_i64(const std::vector<uint8_t>& bytes, size_t start, size_t end) {
    size_t i = start;
    bool negative = false;
    if (i < end && (bytes[i] == '+' || bytes[i] == '-')) {
        negative = bytes[i] == '-';
        i++;
    }
    if (i >= end) return std::unexpected(ParseError{ParseError::Empty});
    const int64_t min = INT64_MIN;
    int64_t value = 0;
    while (i < end) {
        uint8_t b = bytes[i];
        if (b < '0' || b > '9') return std::unexpected(ParseError{ParseError::InvalidDigit, i});
        int64_t digit = b - '0';
        if (value < (min + digit) / 10) return std::unexpected(ParseError{ParseError::Overflow});
        value = value * 10 - digit;
        i++;
    }
    if (negative) return value;
    if (value == min) return std::unexpected(ParseError{ParseError::Overflow});
    return -value;
}

struct Problem { ParseError parse; };

static bool is_space(uint8_t b) { return b == 32 || (b >= 9 && b <= 13); }

// std.text.ascii's next_word: false when only whitespace is left.
static bool next_word(const std::vector<uint8_t>& bytes, size_t from, size_t& start, size_t& end) {
    size_t n = bytes.size();
    start = from;
    while (start < n && is_space(bytes[start])) start++;
    if (start >= n) return false;
    end = start;
    while (end < n && !is_space(bytes[end])) end++;
    return true;
}

static std::expected<int64_t, Problem> sum_words_of(const std::vector<uint8_t>& bytes) {
    int64_t total = 0;
    size_t at = 0, start, end;
    while (next_word(bytes, at, start, end)) {
        auto v = parse_i64(bytes, start, end);
        if (!v) return std::unexpected(Problem{v.error()});
        total = (int64_t)((uint64_t)total + (uint64_t)*v);
        at = end;
    }
    return total;
}

struct AnyError {
    std::string cause;
    std::vector<std::string> contexts;
    const char* kind;
    static AnyError from(const ParseError& e) { return AnyError{e.message(), {}, "ParseError"}; }
};

static std::expected<int64_t, Problem> wrapped(const std::vector<uint8_t>& b, size_t end) {
    auto v = parse_i64(b, 0, end);
    if (!v) return std::unexpected(Problem{v.error()});
    return *v + 1;
}

static std::expected<int64_t, AnyError> converted(const std::vector<uint8_t>& b, size_t end) {
    auto v = parse_i64(b, 0, end);
    if (!v) return std::unexpected(AnyError::from(v.error()));
    return *v + 1;
}

static std::expected<int64_t, AnyError> with_context(const std::vector<uint8_t>& b, size_t end) {
    auto v = parse_i64(b, 0, end);
    if (!v) {
        AnyError e = AnyError::from(v.error());
        e.contexts.push_back("parsing the answer");
        return std::unexpected(std::move(e));
    }
    return *v + 1;
}

struct Bad { int64_t odd; };
using R = std::expected<int64_t, Bad>;
#define TRY(v, call) auto v##_r = (call); if (!v##_r) return std::unexpected(v##_r.error()); int64_t v = *v##_r
static R level0(int64_t x) { if ((x & 15) == 0) return std::unexpected(Bad{x}); return (int64_t)((uint64_t)x * 3); }
static R level1(int64_t x) { TRY(v, level0(x)); return (int64_t)((uint64_t)v + 1); }
static R level2(int64_t x) { TRY(v, level1(x)); return v ^ 5; }
static R level3(int64_t x) { TRY(v, level2(x)); return (int64_t)((uint64_t)v + 7); }
static R level4(int64_t x) { TRY(v, level3(x)); return v ^ 9; }
static R level5(int64_t x) { TRY(v, level4(x)); return (int64_t)((uint64_t)v + 11); }
static R level6(int64_t x) { TRY(v, level5(x)); return v ^ 13; }
static R level7(int64_t x) { TRY(v, level6(x)); return (int64_t)((uint64_t)v + 15); }

static void run_wrapped(size_t end) {
    std::vector<uint8_t> bytes{'4', '2', 'x'};
    auto start = now();
    uint64_t check = 0;
    for (int i = 0; i < 10000000; i++) {
        keep(end);
        bytes[1] = (uint8_t)('0' + i % 10);
        auto r = wrapped(bytes, end);
        if (r) check += (uint64_t)*r; else check += 1;
    }
    report(start, check);
}

int main(int argc, char** argv) {
    if (argc < 2) unknown("");
    std::string k = argv[1];
    std::vector<uint8_t> bytes{'4', '2', 'x'};
    if (k == "wrapped_ok") run_wrapped(2);
    else if (k == "wrapped_err") run_wrapped(3);
    else if (k == "converted_ok" || k == "converted_err") {
        size_t end = k == "converted_ok" ? 2 : 3;
        keep(end);
        auto start = now();
        uint64_t check = 0;
        for (int i = 0; i < 1000000; i++) {
            bytes[1] = (uint8_t)('0' + i % 10);
            auto r = converted(bytes, end);
            if (r) check += (uint64_t)*r; else check += r.error().cause.size();
        }
        report(start, check);
    } else if (k == "context_ok" || k == "context_err") {
        size_t end = k == "context_ok" ? 2 : 3;
        keep(end);
        auto start = now();
        uint64_t check = 0;
        for (int i = 0; i < 1000000; i++) {
            bytes[1] = (uint8_t)('0' + i % 10);
            auto r = with_context(bytes, end);
            if (r) check += (uint64_t)*r;
            else check += r.error().cause.size() + r.error().contexts.size() + 1;
        }
        report(start, check);
    } else if (k == "sum_words") {
        std::vector<uint8_t> text;
        uint64_t x = 88172645463325252ull;
        for (int i = 0; i < 1000000; i++) {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            std::string w = std::to_string((int64_t)(x % 1000000) - 500000) + " ";
            text.insert(text.end(), w.begin(), w.end());
        }
        auto start = now();
        auto r = sum_words_of(text);
        report(start, r ? (uint64_t)*r : 1);
    } else if (k == "propagate_depth8") {
        auto start = now();
        uint64_t check = 0;
        for (int64_t i = 0; i < 10000000; i++) {
            int64_t x = i;
            keep(x);
            auto r = level7(x);
            if (r) check += (uint64_t)*r; else check += (uint64_t)r.error().odd >> 4;
        }
        report(start, check);
    } else unknown(k);
}
