// Copyright (c) 2026 Omnira CJSC
// Author: Tunjay Akbarli
// Date: November 6, 2026
//
// Functionality:
//   - The Go programs of the task benchmarks, printing the same
//     `name nanoseconds check` lines as the Codira host. The thread count
//     comes from GOMAXPROCS.
package main

import (
	"fmt"
	"sync"
	"time"
)

func mix(i int64) int64 {
	x := i*2654435761 + 1
	x ^= x >> 13
	x *= 1274126177
	return x ^ (x >> 16)
}

func burn(seed, iterations int64) int64 {
	x := seed*2 + 1
	for i := int64(0); i < iterations; i++ {
		x ^= x << 13
		x ^= x >> 7
		x ^= x << 17
	}
	return x
}

// n goroutines, each writing its index into a slot, joined by a WaitGroup.
func spawnJoin(n int64) int64 {
	slots := make([]int64, n)
	var wg sync.WaitGroup
	wg.Add(int(n))
	for i := int64(0); i < n; i++ {
		go func(i int64) {
			slots[i] = i
			wg.Done()
		}(i)
	}
	wg.Wait()
	var total int64
	for _, x := range slots {
		total += x
	}
	return total
}

// 64 goroutines each fill a chunk of the result slice.
func fanOutMap(n, chunks int64) int64 {
	all := make([]int64, n)
	var wg sync.WaitGroup
	wg.Add(int(chunks))
	for c := int64(0); c < chunks; c++ {
		go func(lo, hi int64) {
			for i := lo; i < hi; i++ {
				all[i] = mix(i)
			}
			wg.Done()
		}(n*c/chunks, n*(c+1)/chunks)
	}
	wg.Wait()
	var check int64
	for i, x := range all {
		check ^= x + int64(i)
	}
	return check
}

func pipeline(n int64) int64 {
	ch := make(chan int64, 64)
	go func() {
		for i := int64(0); i < n; i++ {
			ch <- i
		}
	}()
	var total int64
	for i := int64(0); i < n; i++ {
		total += <-ch
	}
	return total
}

func compute(tasks, iterations int64) int64 {
	results := make([]int64, tasks)
	var wg sync.WaitGroup
	wg.Add(int(tasks))
	for t := int64(0); t < tasks; t++ {
		go func(t int64) {
			results[t] = burn(t, iterations)
			wg.Done()
		}(t)
	}
	wg.Wait()
	var check int64
	for _, x := range results {
		check += x
	}
	return check
}

func timeIt(name string, f func() int64) {
	f()
	start := time.Now()
	check := f()
	fmt.Printf("%s %d %d\n", name, time.Since(start).Nanoseconds(), check)
}

func main() {
	timeIt("spawn_join_100k", func() int64 { return spawnJoin(100_000) })
	timeIt("fan_out_map_1m", func() int64 { return fanOutMap(1_000_000, 64) })
	timeIt("pipeline_1m", func() int64 { return pipeline(1_000_000) })
	timeIt("compute_64x4m", func() int64 { return compute(64, 4_000_000) })
}
