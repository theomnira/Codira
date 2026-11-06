# Task benchmarks

Codira's `group`/`scope` scheduler against Rust (rayon, crossbeam-channel)
and Go (goroutines, channels) on four workloads, each printing a check value
the others must match:

| workload | what it measures |
|---|---|
| `spawn_join_100k` | 100,000 tasks that return their index, joined, summed |
| `fan_out_map_1m` | a map over 1,000,000 items in 64 tasks, fanned back into one array |
| `pipeline_1m` | 1,000,000 values from a producer task to a consumer over a channel of capacity 64 |
| `compute_64x4m` | 64 CPU-bound tasks (4M xorshift steps each): the parallel speedup |

```
python3 examples/fabric_bench/bench.py --codira <release codira> --work <scratch dir>
```

builds everything under `--work`, runs the programs alternately with 1 and 4
workers (`CODIRA_TASK_WORKERS`, `RAYON_NUM_THREADS`, `GOMAXPROCS`), and
prints the medians. The results are in `agents/USING_CODIRA.md`
("Structured concurrency", "Measured").
