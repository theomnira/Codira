// AURORA: producers and consumers, the C++ baseline of src/mod.code. The
// standard library has no channel; this is the usual bounded blocking
// queue: a ring buffer under a std::mutex with two std::condition_variables,
// and std::thread (started inside the timing, as Codira's tasks are).
#include <condition_variable>
#include <mutex>
#include <thread>

#include "../common/bench.hpp"

using namespace bench;

// A waiter is woken only when one is waiting: notifying on every operation
// with four producers contending makes a lock convoy (measured: 21 s for
// mpsc_4 instead of well under one).
class Channel {
    std::vector<int64_t> buf;
    size_t head = 0, len = 0, waiting_senders = 0, waiting_receivers = 0;
    std::mutex m;
    std::condition_variable not_empty, not_full;

public:
    explicit Channel(size_t cap) : buf(cap) {}
    void send(int64_t v) {
        std::unique_lock<std::mutex> l(m);
        while (len == buf.size()) {
            waiting_senders++;
            not_full.wait(l);
            waiting_senders--;
        }
        buf[(head + len) % buf.size()] = v;
        len++;
        bool wake = waiting_receivers > 0;
        l.unlock();
        if (wake) not_empty.notify_one();
    }
    int64_t recv() {
        std::unique_lock<std::mutex> l(m);
        while (len == 0) {
            waiting_receivers++;
            not_empty.wait(l);
            waiting_receivers--;
        }
        int64_t v = buf[head];
        head = (head + 1) % buf.size();
        len--;
        bool wake = waiting_senders > 0;
        l.unlock();
        if (wake) not_full.notify_one();
        return v;
    }
};

static void produce(Channel& out, int64_t first, int64_t count) {
    for (int64_t i = first; i < first + count; i++) out.send(i);
}

int main(int argc, char** argv) {
    if (argc < 2) unknown("");
    std::string k = argv[1];
    if (k == "spsc") {
        auto start = now();
        Channel ch(1024);
        std::thread t(produce, std::ref(ch), 0, 2000000);
        int64_t sum = 0;
        for (int i = 0; i < 2000000; i++) sum += ch.recv();
        t.join();
        report(start, (uint64_t)sum);
    } else if (k == "mpsc_4") {
        auto start = now();
        Channel ch(1024);
        std::vector<std::thread> ts;
        for (int64_t p = 0; p < 4; p++) ts.emplace_back(produce, std::ref(ch), p * 500000, 500000);
        int64_t sum = 0;
        for (int i = 0; i < 2000000; i++) sum += ch.recv();
        for (auto& t : ts) t.join();
        report(start, (uint64_t)sum);
    } else if (k == "fan_out_in") {
        auto start = now();
        Channel results(1024);
        std::vector<std::unique_ptr<Channel>> jobs;
        std::vector<std::thread> ts;
        for (int w = 0; w < 4; w++) {
            jobs.push_back(std::make_unique<Channel>(1024));
            Channel* j = jobs.back().get();
            ts.emplace_back([j, &results] {
                for (int i = 0; i < 50000; i++) {
                    int64_t x = j->recv();
                    results.send((int64_t)((uint64_t)x * (uint64_t)x) & 0xFFFF);
                }
            });
        }
        int64_t sum = 0, sent = 0, got = 0;
        while (got < 200000) {
            for (int b = 0; b < 256 && sent < 200000; b++, sent++) jobs[sent & 3]->send(sent);
            while (got < sent) { sum += results.recv(); got++; }
        }
        for (auto& t : ts) t.join();
        report(start, (uint64_t)sum);
    } else unknown(k);
}
