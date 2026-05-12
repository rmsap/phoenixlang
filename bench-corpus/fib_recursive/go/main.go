// fib_recursive workload — Go counterpart.
//
// Naive recursive Fibonacci, no memoization. Mirrors the Phoenix
// version exactly: same input (35), same recursion shape. Reports
// the result to stdout for cross-implementation byte-for-byte
// comparison.

package main

import "fmt"

func fib(n int64) int64 {
	if n < 2 {
		return n
	}
	return fib(n-1) + fib(n-2)
}

func main() {
	fmt.Println(fib(35))
}
