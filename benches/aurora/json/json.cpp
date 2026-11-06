// AURORA: JSON, the C++ baseline of src/mod.code on RapidJSON's DOM
// (rapidjson-dev 1.1): parsing with kParseFullPrecisionFlag (correctly
// rounded floats, as Codira and serde_json with float_roundtrip), objects
// in insertion order (RapidJSON keeps members in an array), compact and
// pretty printing with a two-space indent. Same kernels, same check values
// where RapidJSON writes the same text (see the README of the suite).
// `stream_*`: RapidJSON's SAX `Reader` (same flags) with a handler that
// adds to the sums, no tree. `typed_twitter` has no C++ variant (see
// suite.toml).
#include <fstream>
#include <sstream>

#include <rapidjson/document.h>
#include <rapidjson/prettywriter.h>
#include <rapidjson/reader.h>
#include <rapidjson/stringbuffer.h>
#include <rapidjson/writer.h>

#include "../common/bench.hpp"

using namespace bench;
using rapidjson::Document;
using rapidjson::Value;

static const int REPS = 5;

struct Sums { uint64_t nodes = 0, strings = 0, ints = 0, floats = 0; };

static uint64_t bits(double d) { uint64_t u; std::memcpy(&u, &d, 8); return u; }

static void walk(const Value& v, Sums& s) {
    s.nodes++;
    if (v.IsBool()) { if (v.GetBool()) s.ints++; }
    else if (v.IsNumber()) {
        // An integer in the text that fits an i64 is an integer; every
        // other number is a float (RapidJSON's IsInt64 is false for 1.0).
        if (v.IsInt64()) s.ints += (uint64_t)v.GetInt64();
        else if (v.IsUint64()) s.floats += bits((double)v.GetUint64());
        else s.floats += bits(v.GetDouble());
    } else if (v.IsString()) s.strings += v.GetStringLength();
    else if (v.IsArray()) { for (auto& item : v.GetArray()) walk(item, s); }
    else if (v.IsObject()) {
        for (auto& m : v.GetObject()) { s.strings += m.name.GetStringLength(); walk(m.value, s); }
    }
}

static std::string load(const std::string& name) {
    std::ifstream f("data/" + name + ".json", std::ios::binary);
    if (!f) { std::fprintf(stderr, "run from the directory holding data/\n"); std::exit(1); }
    std::stringstream ss;
    ss << f.rdbuf();
    return ss.str();
}

static void parse_into(Document& d, const std::string& text) {
    d.Parse<rapidjson::kParseFullPrecisionFlag>(text.data(), text.size());
    if (d.HasParseError()) { std::fprintf(stderr, "parse error\n"); std::exit(1); }
}

// The sums of `walk`, from the reader's events. A number is an integer
// when the text's integer fits an i64 (RapidJSON reports those through
// Int, Uint, Int64 or Uint64), a float otherwise.
struct StreamSums : rapidjson::BaseReaderHandler<rapidjson::UTF8<>, StreamSums> {
    Sums s;
    bool Null() { s.nodes++; return true; }
    bool Bool(bool b) { s.nodes++; if (b) s.ints++; return true; }
    bool Int(int i) { s.nodes++; s.ints += (uint64_t)(int64_t)i; return true; }
    bool Uint(unsigned u) { s.nodes++; s.ints += u; return true; }
    bool Int64(int64_t i) { s.nodes++; s.ints += (uint64_t)i; return true; }
    bool Uint64(uint64_t u) {
        s.nodes++;
        if (u <= (uint64_t)INT64_MAX) s.ints += u; else s.floats += bits((double)u);
        return true;
    }
    bool Double(double d) { s.nodes++; s.floats += bits(d); return true; }
    bool String(const char*, rapidjson::SizeType len, bool) { s.nodes++; s.strings += len; return true; }
    bool Key(const char*, rapidjson::SizeType len, bool) { s.strings += len; return true; }
    bool StartObject() { s.nodes++; return true; }
    bool EndObject(rapidjson::SizeType) { return true; }
    bool StartArray() { s.nodes++; return true; }
    bool EndArray(rapidjson::SizeType) { return true; }
};

int main(int argc, char** argv) {
    if (argc < 2) unknown("");
    std::string k = argv[1];
    auto at = k.find('_');
    if (at == std::string::npos) unknown(k);
    std::string phase = k.substr(0, at), input = k.substr(at + 1);
    if (input != "twitter" && input != "canada" && input != "deep") unknown(k);
    std::string text = load(input);
    if (phase == "parse") {
        auto start = now();
        Document d;
        for (int i = 0; i < REPS; i++) {
            Document fresh;  // a new tree each time, the old one freed
            parse_into(fresh, text);
            d.Swap(fresh);
        }
        uint64_t ns = elapsed_ns(start);
        Sums s;
        walk(d, s);
        uint64_t check = s.nodes * 1000003 + s.strings * 10007 + s.ints * 101 + s.floats;
        std::printf("@@ ns=%llu check=%llu\n", (unsigned long long)ns, (unsigned long long)check);
        peak_rss();
    } else if (phase == "print" || phase == "pretty") {
        Document d;
        parse_into(d, text);
        auto start = now();
        size_t len = 0;
        for (int i = 0; i < REPS; i++) {
            rapidjson::StringBuffer buf;
            if (phase == "pretty") {
                rapidjson::PrettyWriter<rapidjson::StringBuffer> w(buf);
                w.SetIndent(' ', 2);
                d.Accept(w);
            } else {
                rapidjson::Writer<rapidjson::StringBuffer> w(buf);
                d.Accept(w);
            }
            std::string out(buf.GetString(), buf.GetSize());  // a string, as the others return
            len = out.size();
        }
        report(start, len);
    } else if (phase == "stream") {
        auto start = now();
        uint64_t check = 0;
        for (int i = 0; i < REPS; i++) {
            StreamSums h;
            rapidjson::Reader reader;
            rapidjson::StringStream ms(text.c_str());
            if (reader.Parse<rapidjson::kParseFullPrecisionFlag>(ms, h).IsError()) {
                std::fprintf(stderr, "parse error\n");
                std::exit(1);
            }
            check = h.s.nodes * 1000003 + h.s.strings * 10007 + h.s.ints * 101 + h.s.floats;
        }
        report(start, check);
    } else unknown(k);
}
