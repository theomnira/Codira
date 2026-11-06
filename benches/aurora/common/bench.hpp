// Copyright (c) 2026 Omnira CJSC
// Author: Tunjay Akbarli
// Date: November 6, 2026
//
// Functionality:
// - What every C++ baseline of the AURORA suite shares: the kernel name from
//   the command line, the monotonic clock, the `@@ ns=.. check=..` line the
//   runner reads (with the peak resident set from /proc/self/status), the
//   xorshift generator the Codira programs use, and latency percentiles.
#pragma once

#include <algorithm>
#include <chrono>
#include <cstdint>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <string>
#include <vector>

namespace bench {

using Clock = std::chrono::steady_clock;

inline Clock::time_point now() { return Clock::now(); }

inline uint64_t elapsed_ns(Clock::time_point start) {
    return std::chrono::duration_cast<std::chrono::nanoseconds>(Clock::now() - start).count();
}

inline void peak_rss() {
    FILE* f = std::fopen("/proc/self/status", "r");
    if (!f) return;
    char line[256];
    while (std::fgets(line, sizeof line, f)) {
        if (std::strncmp(line, "VmHWM:", 6) == 0) {
            std::printf("@@ rss_peak_kb=%lld\n", std::atoll(line + 6));
            break;
        }
    }
    std::fclose(f);
}

inline void report(Clock::time_point start, uint64_t check) {
    uint64_t ns = elapsed_ns(start);
    std::printf("@@ ns=%llu check=%llu\n", (unsigned long long)ns, (unsigned long long)check);
    peak_rss();
    std::fflush(stdout);
}

[[noreturn]] inline void unknown(const std::string& k) {
    std::fprintf(stderr, "unknown kernel `%s`\n", k.c_str());
    std::exit(2);
}

struct Rng {
    uint64_t s = 0x9E3779B97F4A7C15ull;
    uint64_t next() {
        uint64_t x = s;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        s = x;
        return x;
    }
};

inline void report_latencies(std::vector<uint64_t>& lat) {
    std::sort(lat.begin(), lat.end());
    auto at = [&](size_t q) { return lat[std::min(lat.size() * q / 1000, lat.size() - 1)]; };
    std::printf("@@ p50_ns=%llu p95_ns=%llu p99_ns=%llu p999_ns=%llu max_ns=%llu\n",
                (unsigned long long)at(500), (unsigned long long)at(950),
                (unsigned long long)at(990), (unsigned long long)at(999),
                (unsigned long long)lat.back());
}

// Keeps a value alive as far as the optimiser is concerned.
template <class T>
inline void keep(T const& v) {
    asm volatile("" : : "r,m"(v) : "memory");
}

}  // namespace bench
