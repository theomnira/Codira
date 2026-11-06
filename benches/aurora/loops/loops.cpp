// AURORA: numeric loops and calls, the C++ baseline of src/mod.code (the
// same arithmetic; unsigned where Codira wraps; unchecked operator[]).
#include "../common/bench.hpp"

using namespace bench;

static const size_t N = 10000000;

static std::vector<int64_t> ramp() {
    std::vector<int64_t> v(N);
    for (size_t i = 0; i < N; i++) v[i] = (int64_t)i * 3 - 7;
    keep(v);
    return v;
}

struct Acc { int64_t total; };

[[gnu::noinline]] static void store_into(const std::vector<int64_t>& xs, Acc* acc) {
    for (int64_t x : xs) acc->total = (int64_t)((uint64_t)acc->total + (uint64_t)x);
}

[[gnu::noinline]] static void copy_into(const std::vector<int64_t>& xs, std::vector<int64_t>& ys) {
    size_t k = 0;
    for (int64_t x : xs) ys[k++] = x + 1;
}

static int64_t fib(int64_t n) { return n < 2 ? n : fib(n - 1) + fib(n - 2); }

int main(int argc, char** argv) {
    if (argc < 2) unknown("");
    std::string k = argv[1];
    if (k == "sum_for" || k == "sum_indexed") {
        auto xs = ramp();
        auto start = now();
        uint64_t t = 0;
        for (size_t i = 0; i < xs.size(); i++) t += (uint64_t)xs[i];
        report(start, t);
    } else if (k == "store_field") {
        auto xs = ramp();
        Acc* acc = new Acc{0};
        auto start = now();
        store_into(xs, acc);
        report(start, (uint64_t)acc->total);
        delete acc;
    } else if (k == "copy_plus_one") {
        auto xs = ramp();
        std::vector<int64_t> ys(N);
        auto start = now();
        copy_into(xs, ys);
        report(start, (uint64_t)(ys[7] + ys[N - 1]));
    } else if (k == "fib_32") {
        int64_t n = 32;
        keep(n);
        auto start = now();
        report(start, (uint64_t)fib(n));
    } else if (k == "sieve_20m") {
        size_t n = 20000000;
        keep(n);
        auto start = now();
        std::vector<uint8_t> composite(n, 0);  // a byte per entry, as the others
        uint64_t count = 0;
        for (size_t i = 2; i < n; i++) {
            if (!composite[i]) {
                count++;
                for (size_t j = i * i; j < n; j += i) composite[j] = 1;
            }
        }
        report(start, count);
    } else if (k == "mandelbrot") {
        int rows = 400;
        keep(rows);
        auto start = now();
        uint64_t inside = 0, steps = 0;
        for (int py = 0; py < rows; py++) {
            for (int px = 0; px < 600; px++) {
                double cx = px / 200.0 - 2.0, cy = py / 200.0 - 1.0;
                double x = 0, y = 0;
                int k = 0;
                while (k < 200 && x * x + y * y <= 4.0) {
                    double t = x * x - y * y + cx;
                    y = 2.0 * x * y + cy;
                    x = t;
                    k++;
                }
                steps += k;
                if (k == 200) inside++;
            }
        }
        report(start, inside * 1000000000ull + steps);
    } else if (k == "matmul_200") {
        size_t n = 200;
        keep(n);
        std::vector<double> a(n * n), b(n * n), c(n * n, 0.0);
        for (size_t i = 0; i < n * n; i++) {
            a[i] = (double)(i % 17) * 0.25;
            b[i] = (double)(i % 13) * 0.5;
        }
        auto start = now();
        for (size_t i = 0; i < n; i++)
            for (size_t k = 0; k < n; k++) {
                double aik = a[i * n + k];
                for (size_t j = 0; j < n; j++) c[i * n + j] = c[i * n + j] + aik * b[k * n + j];
            }
        double s = 0;
        for (double v : c) s += v;
        report(start, (uint64_t)s);
    } else if (k == "collatz_1m") {
        uint64_t limit = 1000000;
        keep(limit);
        auto start = now();
        uint64_t best = 0, at = 0;
        for (uint64_t n = 1; n < limit; n++) {
            uint64_t x = n, steps = 0;
            while (x != 1) {
                x = (x & 1) == 0 ? x >> 1 : 3 * x + 1;
                steps++;
            }
            if (steps > best) { best = steps; at = n; }
        }
        report(start, at * 1000 + best);
    } else unknown(k);
}
