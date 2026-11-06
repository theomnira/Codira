// AURORA: task groups, the C++ baseline of src/mod.code: std::async with
// std::launch::async (a thread per task), joined through its future.
#include <future>

#include "../common/bench.hpp"

using namespace bench;

static int64_t fib(int64_t n) { return n < 2 ? n : fib(n - 1) + fib(n - 2); }

int main(int argc, char** argv) {
    if (argc < 2) unknown("");
    std::string k = argv[1];
    if (k == "fib_64") {
        auto start = now();
        int64_t check = 0;
        for (int round = 0; round < 10; round++) {
            std::vector<std::future<int64_t>> fs;
            for (int64_t i = 0; i < 64; i++) fs.push_back(std::async(std::launch::async, [i] { return fib(24 + (i & 1)); }));
            for (auto& f : fs) check += f.get();
        }
        report(start, (uint64_t)check);
    } else unknown(k);
}
