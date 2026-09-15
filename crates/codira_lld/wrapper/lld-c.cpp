//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: August 7, 2026
//!
//! Functionality:
//! - A wrapper around LLD in Rust Programming Language.
//!   An Interface between C++ and Rust.

#include <lld/Common/Driver.h>

#include <cstdlib>
#include <cstring>
#include <mutex>
#include <string>
#include <vector>

LLD_HAS_DRIVER(coff)
LLD_HAS_DRIVER(elf)
LLD_HAS_DRIVER(mingw)
LLD_HAS_DRIVER(macho)
LLD_HAS_DRIVER(wasm)

const char *codira_alloc_str(const std::string &str) {
    size_t size = str.length();
    if (size > 0) {
        char *strPtr = reinterpret_cast<char *>(malloc(size + 1));
        memcpy(strPtr, str.c_str(), size + 1);
        return strPtr;
    }
    return nullptr;
}

// LLD is not thread safe: the drivers mutate global state. We only allow
// single threaded access to lldMain using a mutex.
std::mutex concurrencyMutex;

extern "C" {

enum LldFlavor {
    Elf = 0,
    Wasm = 1,
    MachO = 2,
    Coff = 3,
};

struct LldInvokeResult {
    bool success;
    const char *messages;
};

void codira_link_free_result(LldInvokeResult *result) {
    if (result->messages) {
        free(reinterpret_cast<void *>(const_cast<char *>(result->messages)));
    }
}
}

// lldMain dispatches on argv[0]; these program names select the right driver.
static const char *getProgramNameForFlavor(LldFlavor flavor) {
    switch (flavor) {
        case Wasm:
            return "wasm-ld";
        case MachO:
            return "ld64.lld";
        case Coff:
            return "lld-link";
        case Elf:
        default:
            return "ld.lld";
    }
}

extern "C" {

LldInvokeResult codira_lld_link(LldFlavor flavor, int argc, const char *const *argv) {
    LldInvokeResult result;

    // Construct stdout and stderr streams
    std::string outputString, errorString;
    llvm::raw_string_ostream outputStream(outputString);
    llvm::raw_string_ostream errorStream(errorString);

    // Copy arguments, prefixed by the program name that selects the driver.
    std::vector<const char *> args;
    args.reserve(static_cast<size_t>(argc) + 1);
    args.push_back(getProgramNameForFlavor(flavor));
    args.insert(args.end(), argv, argv + argc);

    // LLD is not thread-safe at all, so we guard parallel invocation with a mutex
    std::unique_lock<std::mutex> lock(concurrencyMutex);
    lld::Result linkResult = lld::lldMain(args, outputStream, errorStream, LLD_ALL_DRIVERS);
    result.success = linkResult.retCode == 0;

    std::string resultMessage = errorStream.str() + outputStream.str();
    result.messages = codira_alloc_str(resultMessage);
    return result;
}
}
