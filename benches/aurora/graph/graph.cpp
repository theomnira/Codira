// AURORA: object graphs, the C++ baseline of src/mod.code: std::unique_ptr
// trees; graphs and lists of individually allocated nodes owned by a
// vector (or by the list walk that deletes them), edges as plain pointers.
// The same shapes, the same check values.
#include <memory>

#include "../common/bench.hpp"

using namespace bench;

struct Tree { std::unique_ptr<Tree> left, right; };

static std::unique_ptr<Tree> bottom_up(int64_t depth) {
    auto t = std::make_unique<Tree>();
    if (depth > 0) { t->left = bottom_up(depth - 1); t->right = bottom_up(depth - 1); }
    return t;
}
static int64_t count(const Tree* t) {
    return t->left && t->right ? 1 + count(t->left.get()) + count(t->right.get()) : 1;
}

struct Node { int64_t id, mark; std::vector<Node*> edges; };
struct Link { int64_t value; Link* prev; Link* next; };

int main(int argc, char** argv) {
    if (argc < 2) unknown("");
    std::string k = argv[1];
    if (k == "binary_trees") {
        int64_t max_depth = 16;
        keep(max_depth);
        auto start = now();
        auto long_lived = bottom_up(max_depth);
        int64_t check = 0;
        for (int64_t depth = 4; depth <= max_depth; depth += 2) {
            int64_t iterations = (int64_t)1 << (max_depth - depth + 4);
            for (int64_t i = 0; i < iterations; i++) check += count(bottom_up(depth).get());
        }
        check += count(long_lived.get());
        report(start, (uint64_t)check);
    } else if (k == "cyclic_graph") {
        Rng r;
        size_t n = 20000;
        keep(n);
        auto start = now();
        uint64_t check = 0;
        for (int64_t round = 0; round < 40; round++) {
            std::vector<std::unique_ptr<Node>> nodes;
            for (size_t i = 0; i < n; i++) nodes.push_back(std::make_unique<Node>(Node{(int64_t)i, -1, {}}));
            for (auto& node : nodes)
                for (int e = 0; e < 4; e++) node->edges.push_back(nodes[r.next() % n].get());
            std::vector<Node*> stack{nodes[0].get()};
            nodes[0]->mark = round;
            uint64_t visited = 0, sum = 0;
            while (!stack.empty()) {
                Node* top = stack.back();
                stack.pop_back();
                visited++;
                sum += top->id;
                for (Node* next : top->edges)
                    if (next->mark != round) { next->mark = round; stack.push_back(next); }
            }
            check += visited * 1000000 + sum % 1000000;
        }
        report(start, check);
    } else if (k == "linked_list") {
        int rounds = 10;
        keep(rounds);
        auto start = now();
        uint64_t check = 0;
        for (int round = 0; round < rounds; round++) {
            Link* head = new Link{0, nullptr, nullptr};
            Link* tail = head;
            for (int64_t i = 1; i < 1000000; i++) {
                Link* node = new Link{i, tail, nullptr};
                tail->next = node;
                tail = node;
            }
            uint64_t forward = 0, backward = 0;
            for (Link* at = head; at; at = at->next) forward += at->value;
            for (Link* at = tail; at; at = at->prev) backward += (uint64_t)at->value & 255;
            check += forward + backward;
            for (Link* at = head; at;) { Link* next = at->next; delete at; at = next; }
        }
        report(start, check);
    } else unknown(k);
}
