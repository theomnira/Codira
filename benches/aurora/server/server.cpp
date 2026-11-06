// AURORA: a server-style workload, the C++ baseline of src/mod.code
// (std::unordered_map with Codira's integer hash, std::string, std::format).
// The same requests, the same work per request, the same check values.
#include <charconv>
#include <format>
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

struct Session { int64_t uid, hits; std::vector<int64_t> items; std::string last; };

static std::vector<std::string_view> split(std::string_view s, char sep) {
    std::vector<std::string_view> out;
    for (;;) {
        size_t at = s.find(sep);
        if (at == std::string_view::npos) { out.push_back(s); return out; }
        out.push_back(s.substr(0, at));
        s.remove_prefix(at + 1);
    }
}
static int64_t parse(std::string_view s) {
    int64_t v = 0;
    if (std::from_chars(s.data(), s.data() + s.size(), v).ec != std::errc()) std::abort();
    return v;
}

static void serve(size_t requests, size_t live, uint64_t users) {
    Rng r;
    std::unordered_map<int64_t, Session, IntHash> sessions;
    std::vector<int64_t> ring(live, -1);
    size_t ring_at = 0;
    std::vector<uint64_t> lat(requests);
    uint64_t check = 0;
    auto start = now();
    for (size_t n = 0; n < requests; n++) {
        int64_t uid = (int64_t)(r.next() % users);
        int64_t item = (int64_t)(r.next() % 1000);
        std::string line = std::format("GET /users/{}/items/{} HTTP/1.1", uid, item);
        auto t0 = now();

        auto words = split(line, ' ');
        auto parts = split(words[1], '/');
        int64_t user = parse(parts[2]), thing = parse(parts[4]);
        auto it = sessions.find(user);
        if (it == sessions.end()) {
            int64_t old = ring[ring_at];
            if (old >= 0) sessions.erase(old);
            ring[ring_at] = user;
            ring_at = (ring_at + 1) % live;
            it = sessions.emplace(user, Session{user, 0, {}, {}}).first;
        }
        Session& s = it->second;
        s.hits++;
        if (s.items.size() >= 16) s.items = std::vector<int64_t>();
        s.items.push_back(thing);
        std::string body = std::format("{{\"user\":{},\"item\":{},\"hits\":{},\"recent\":{}}}", user, thing,
                                       s.hits, s.items.size());
        std::string response = std::format(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n", body.size()) + body;
        check += response.size();
        s.last = std::move(response);

        lat[n] = elapsed_ns(t0);
    }
    uint64_t ns = elapsed_ns(start);
    std::printf("@@ ns=%llu check=%llu\n", (unsigned long long)ns,
                (unsigned long long)(check * 1000 + sessions.size()));
    report_latencies(lat);
    peak_rss();
}

int main(int argc, char** argv) {
    if (argc < 2) unknown("");
    std::string k = argv[1];
    if (k == "requests") serve(200000, 20000, 50000);
    else if (k == "long_run") serve(1000000, 50000, 200000);
    else unknown(k);
}
