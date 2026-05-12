// hash_map_churn workload — Go counterpart.
//
// Inserts 100k key/value pairs (k → k*7), then probes 0..200000 with
// `m[k]` reads — half hit, half miss. Sum the hit values; print the
// final length + the sum.
//
// Go's `map` is mutable + amortized O(1) per insert, vs Phoenix's
// immutable `Map` whose every `set` allocates and copies. The
// comparison numbers therefore stack the immutability cost on the
// Phoenix column; the published page calls this out.

package main

import "fmt"

func main() {
	const n = 100000
	m := make(map[int]int, n)
	for i := 0; i < n; i++ {
		m[i] = i * 7
	}
	fmt.Println(len(m))

	sum := 0
	for j := 0; j < 2*n; j++ {
		// Map zero-value on miss matches Phoenix's `unwrapOr(0)`.
		sum += m[j]
	}
	fmt.Println(sum)
}
