// AURORA: `@region` functions, the C++ baseline of src/mod.code: `new` and
// `delete` (unique_ptr) and std::vector for the `_heap` kernels;
// std::pmr::monotonic_buffer_resource, an arena released when the call
// returns, for the `_region` kernels.
#include <memory>
#include <memory_resource>

#include "../common/bench.hpp"

using namespace bench;

struct Cell { int64_t value; Cell* next; };

static int64_t list_sum_heap(int64_t n) {
    Cell* head = nullptr;
    for (int64_t i = 0; i < n; i++) head = new Cell{i, head};
    int64_t sum = 0;
    for (Cell* c = head; c; c = c->next) sum += c->value;
    while (head) { Cell* next = head->next; delete head; head = next; }
    return sum;
}

static int64_t list_sum_region(int64_t n) {
    std::pmr::monotonic_buffer_resource arena;
    std::pmr::polymorphic_allocator<Cell> alloc(&arena);
    Cell* head = nullptr;
    for (int64_t i = 0; i < n; i++) {
        Cell* c = alloc.allocate(1);
        *c = Cell{i, head};
        head = c;
    }
    int64_t sum = 0;
    for (Cell* c = head; c; c = c->next) sum += c->value;
    return sum;  // the arena is released here
}

static int64_t scratch_heap(size_t n, int64_t seed) {
    std::vector<int64_t> xs(n, seed), ys;
    ys.reserve(n);
    for (int64_t x : xs) ys.push_back(x * 3 + 1);
    int64_t s = 0;
    for (int64_t y : ys) s += y;
    return s + ys[0];
}

static int64_t scratch_region(size_t n, int64_t seed) {
    std::pmr::monotonic_buffer_resource arena;
    std::pmr::vector<int64_t> xs(n, seed, &arena), ys(&arena);
    ys.reserve(n);
    for (int64_t x : xs) ys.push_back(x * 3 + 1);
    int64_t s = 0;
    for (int64_t y : ys) s += y;
    return s + ys[0];
}

int main(int argc, char** argv) {
    if (argc < 2) unknown("");
    std::string k = argv[1];
    auto start = now();
    int64_t check = 0;
    if (k == "list_heap" || k == "list_region") {
        bool region = k == "list_region";
        for (int64_t i = 0; i < 100000; i++) {
            int64_t n = 64 + (i & 1);
            keep(n);
            check += region ? list_sum_region(n) : list_sum_heap(n);
        }
    } else if (k == "scratch_arrays_heap" || k == "scratch_arrays_region") {
        bool region = k == "scratch_arrays_region";
        for (int64_t i = 0; i < 50000; i++) {
            size_t n = 1024;
            keep(n);
            check += region ? scratch_region(n, i & 7) : scratch_heap(n, i & 7);
        }
    } else unknown(k);
    report(start, (uint64_t)check);
}
