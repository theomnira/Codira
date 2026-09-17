/* Calls a Codira library from C.
 *
 * The library is loaded at runtime rather than linked against, so this needs
 * no import library and no build-time dependency on the Codira toolchain --
 * only the function signatures, which are the contract `@export("C")`
 * declared on the other side.
 *
 * Build and run (see ../README.md):
 *     cc -o main main.c            (POSIX: add -ldl)
 *     ./main ../codira/target/mod.codiralib
 */

#include <stdio.h>
#include <stdint.h>
#include <math.h>

#if defined(_WIN32)
#include <windows.h>
typedef HMODULE library_t;
static library_t library_open(const char *path) { return LoadLibraryA(path); }
static void *library_symbol(library_t lib, const char *name) {
    return (void *)GetProcAddress(lib, name);
}
#else
#include <dlfcn.h>
typedef void *library_t;
static library_t library_open(const char *path) { return dlopen(path, RTLD_NOW); }
static void *library_symbol(library_t lib, const char *name) {
    return dlsym(lib, name);
}
#endif

/* The exported Codira API, as C sees it. */
typedef int64_t (*add_fn)(int64_t, int64_t);
typedef double (*scale_fn)(double, double);
typedef double (*binary_double_fn)(double, double);
typedef double (*ternary_double_fn)(double, double, double);
typedef int64_t (*divmod_fn)(int64_t, int64_t);

static int failures = 0;

static void check_i64(const char *label, int64_t actual, int64_t expected) {
    int ok = actual == expected;
    failures += !ok;
    printf("  %s %s = %lld", ok ? "ok  " : "FAIL", label, (long long)actual);
    if (!ok) printf(" (expected %lld)", (long long)expected);
    printf("\n");
}

static void check_f64(const char *label, double actual, double expected) {
    /* An exact comparison is right here: every value below is exactly
     * representable, so any difference would be a real ABI defect (a
     * truncated argument, a float/double mix-up) rather than rounding. */
    int ok = actual == expected;
    failures += !ok;
    printf("  %s %s = %g", ok ? "ok  " : "FAIL", label, actual);
    if (!ok) printf(" (expected %g)", expected);
    printf("\n");
}

int main(int argc, char **argv) {
    if (argc != 2) {
        fprintf(stderr, "usage: %s <path to .codiralib>\n", argv[0]);
        return 2;
    }

    library_t lib = library_open(argv[1]);
    if (!lib) {
        fprintf(stderr, "could not load %s\n", argv[1]);
        return 1;
    }

    add_fn codira_add = (add_fn)library_symbol(lib, "codira_add");
    scale_fn codira_scale = (scale_fn)library_symbol(lib, "codira_scale");
    binary_double_fn codira_sum_of_squares =
        (binary_double_fn)library_symbol(lib, "codira_sum_of_squares");
    binary_double_fn codira_length_squared =
        (binary_double_fn)library_symbol(lib, "codira_length_squared");
    ternary_double_fn codira_scaled_length_squared =
        (ternary_double_fn)library_symbol(lib, "codira_scaled_length_squared");
    divmod_fn codira_div = (divmod_fn)library_symbol(lib, "codira_div");
    divmod_fn codira_mod = (divmod_fn)library_symbol(lib, "codira_mod");

    if (!codira_add || !codira_scale || !codira_sum_of_squares ||
        !codira_length_squared || !codira_scaled_length_squared ||
        !codira_div || !codira_mod) {
        fprintf(stderr, "a symbol was missing from %s\n", argv[1]);
        return 1;
    }

    check_i64("codira_add(20, 22)", codira_add(20, 22), 42);
    check_f64("codira_scale(1.5, 4.0)", codira_scale(1.5, 4.0), 6.0);
    check_f64("codira_sum_of_squares(3, 4)", codira_sum_of_squares(3.0, 4.0), 25.0);
    check_f64("codira_length_squared(3, 4)", codira_length_squared(3.0, 4.0), 25.0);
    check_f64("codira_scaled_length_squared(3, 4, 2)",
              codira_scaled_length_squared(3.0, 4.0, 2.0), 100.0);
    check_i64("codira_div(47, 5)", codira_div(47, 5), 9);
    check_i64("codira_mod(47, 5)", codira_mod(47, 5), 2);

    return failures ? 1 : 0;
}
