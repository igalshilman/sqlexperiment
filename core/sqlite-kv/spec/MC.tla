---- MODULE MC ----
EXTENDS Protocol
CONSTANTS a, b
\* The two processes are interchangeable.
Symmetry == Permutations({a, b})
====
