#!/usr/bin/env bash
# Shared cross-language anchor: cognitive 7 / cyclomatic 4.
# Shell has no labelled `continue`, so the flat `else` supplies the 7th
# cognitive point the SonarSource original gets from a labelled jump.
sum_of_primes() {
  local max=$1
  local total=0
  for ((i = 2; i <= max; i++)); do
    for ((j = 2; j < i; j++)); do
      if [ $((i % j)) -eq 0 ]; then
        total=$total
      else
        total=$((total + i))
      fi
    done
  done
  echo "$total"
}

sum_of_primes 10
