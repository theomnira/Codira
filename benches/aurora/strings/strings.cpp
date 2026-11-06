// AURORA: strings, the C++ baseline of src/mod.code (same work, same check
// values) on std::string and std::string_view. Built by scripts/bench.py.
#include <charconv>
#include <string_view>

#include "../common/bench.hpp"

using namespace bench;

static std::string repeat(std::string_view s, size_t n) {
    std::string out;
    out.reserve(s.size() * n);
    for (size_t i = 0; i < n; i++) out += s;
    return out;
}

// Rust's str::lines: split at '\n' (dropping a '\r' before it), no empty
// last line.
static std::vector<std::string_view> lines(std::string_view text) {
    std::vector<std::string_view> out;
    size_t at = 0;
    while (at < text.size()) {
        size_t end = text.find('\n', at);
        if (end == std::string_view::npos) end = text.size();
        std::string_view line = text.substr(at, end - at);
        if (!line.empty() && line.back() == '\r') line.remove_suffix(1);
        out.push_back(line);
        at = end + 1;
    }
    return out;
}

int main(int argc, char** argv) {
    if (argc < 2) unknown("");
    std::string k = argv[1];
    if (k == "append_short") {
        auto start = now();
        std::string words;
        const char* volatile word = "word ";
        for (int i = 0; i < 1000000; i++) words += word;
        report(start, words.size());
    } else if (k == "append_numbers") {
        auto start = now();
        std::string numbers;
        for (long long i = 0; i < 1000000; i++) {
            numbers += std::to_string(i);
            numbers += '\n';
        }
        report(start, numbers.size());
    } else if (k == "concat") {
        std::vector<std::string> names;
        for (int i = 0; i < 64; i++) names.push_back("name" + std::to_string(i));
        auto start = now();
        uint64_t check = 0;
        for (size_t i = 0; i < 1000000; i++) {
            std::string t = names[i & 63] + "/" + names[(i * 7) & 63];
            check += t.size() + (unsigned char)t.back();
        }
        report(start, check);
    } else if (k == "split_lines") {
        std::string text = repeat("the quick brown fox jumps over the lazy dog 012345\n", 205600);
        keep(text);
        auto start = now();
        auto ls = lines(text);
        report(start, ls.size());
    } else if (k == "lines_lazy") {
        std::string text = repeat("the quick brown fox jumps over the lazy dog 012345\n", 205600);
        keep(text);
        auto start = now();
        uint64_t line_bytes = 0;
        std::string_view rest = text;
        while (!rest.empty()) {
            size_t end = rest.find('\n');
            std::string_view line = rest.substr(0, end);
            if (!line.empty() && line.back() == '\r') line.remove_suffix(1);
            line_bytes += line.size();
            if (end == std::string_view::npos) break;
            rest.remove_prefix(end + 1);
        }
        report(start, line_bytes);
    } else if (k == "split_lazy_count") {
        std::string text = repeat("the,quick,brown,fox,jumps,over,the,lazy,dog,012345\n", 205600);
        keep(text);
        auto start = now();
        uint64_t count = 0;
        std::string_view rest = text;
        for (;;) {
            size_t at = rest.find(',');
            std::string_view field = rest.substr(0, at);
            if (field.size() > 3) count++;
            if (at == std::string_view::npos) break;
            rest.remove_prefix(at + 1);
        }
        report(start, count);
    } else if (k == "split_fields") {
        std::string text = repeat("alpha,beta,gamma,delta,", 250000);
        keep(text);
        auto start = now();
        std::vector<std::string_view> parts;
        std::string_view rest = text;
        for (;;) {
            size_t at = rest.find(',');
            if (at == std::string_view::npos) { parts.push_back(rest); break; }
            parts.push_back(rest.substr(0, at));
            rest.remove_prefix(at + 1);
        }
        uint64_t total = 0;
        for (auto p : parts) total += p.size();
        report(start, parts.size() * 1000000ull + total);
    } else if (k == "find_10mb") {
        std::string hay = repeat("ab", 5000000) + "needle";
        keep(hay);
        auto start = now();
        size_t at = hay.find("needle");
        report(start, at);
    } else if (k == "parse_i64") {
        std::string numbers;
        for (long long i = 0; i < 1000000; i++) {
            numbers += std::to_string(i);
            numbers += '\n';
        }
        auto fields = lines(numbers);
        auto start = now();
        int64_t sum = 0;
        for (auto f : fields) {
            int64_t v = 0;
            auto r = std::from_chars(f.data(), f.data() + f.size(), v);
            if (r.ec != std::errc()) std::abort();
            sum += v;
        }
        report(start, (uint64_t)sum);
    } else {
        unknown(k);
    }
}
