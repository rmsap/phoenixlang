// alloc_walk_struct workload — Go counterpart.
//
// Mirrors the Phoenix deviation from phase-2.md's literal "1M alive
// concurrently" wording: allocates one Point per iteration, reads
// both fields into a running sum, lets each go out of scope. Go's
// escape analysis will likely stack-allocate the Point if it's small
// enough — so this is *not* a clean apples-to-apples allocator
// comparison until Phoenix has equivalent escape analysis. The
// published page documents the gap; the workload still exercises
// Go's call + struct-literal path against Phoenix's heap-alloc path.
//
// Output: Σ (i + (i + 1)) = Σ (2i + 1) for i = 0..n-1
//   = n*(n-1) + n = n²; for n = 1_000_000 that's 10¹² = 1,000,000,000,000.
// Must match the Phoenix counterpart byte-for-byte.

package main

import "fmt"

type Point struct {
	X int64
	Y int64
}

func main() {
	const n = 1_000_000
	acc := int64(0)
	for i := int64(0); i < n; i++ {
		// `new(Point)` would force heap allocation, but the
		// composite-literal form matches the Phoenix `Point(i, i+1)`
		// constructor most directly. Go's compiler decides where it
		// goes; the published page calls out the escape-analysis
		// asymmetry.
		p := Point{X: i, Y: i + 1}
		acc += p.X + p.Y
	}
	fmt.Println(acc)
}
