// sort_ints workload — Go counterpart.
//
// Builds 100k pseudo-random integers via the *same* LCG constants as
// the Phoenix version (a=1664525, c=1013904223, m=2^31-1) seeded at
// 1, then sorts with `slices.Sort`. Output is first / middle / last
// elements plus the length, byte-for-byte equal to the Phoenix
// program's stdout when both LCGs walk the same sequence.

package main

import (
	"fmt"
	"slices"
)

func lcgNext(state int64) int64 {
	return (state*1664525 + 1013904223) % 2147483647
}

func main() {
	const n = 100000
	xs := make([]int64, 0, n)
	s := int64(1)
	for i := 0; i < n; i++ {
		s = lcgNext(s)
		xs = append(xs, s)
	}
	slices.Sort(xs)
	fmt.Println(xs[0])
	fmt.Println(xs[n/2])
	fmt.Println(xs[n-1])
	fmt.Println(len(xs))
}
