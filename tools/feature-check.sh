#!/bin/bash
# Build the kernel for a matrix of Cargo feature sets — nothing on, each
# feature alone, and everything-but-one — so a feature gate that quietly
# stops compiling gets caught. (Features that imply others pull them in
# through Cargo, so "alone" means "with its own dependencies".)
#
# usage: tools/feature-check.sh            # from anywhere in the repo
set -u
cd "$(dirname "$0")/../kernel"

FEATURES=(heap frame-alloc paging storage keyboard serial-input pit-100hz apic
          usermode-demo scheduler shell elf-loader net)

sets=("")                       # no features at all
for f in "${FEATURES[@]}"; do sets+=("$f"); done
for skip in "${FEATURES[@]}"; do
    list=()
    for f in "${FEATURES[@]}"; do [ "$f" != "$skip" ] && list+=("$f"); done
    sets+=("$(IFS=,; echo "${list[*]}")")
done

fail=0
for set in "${sets[@]}"; do
    label="${set:-<none>}"
    if out=$(cargo -Zjson-target-spec build --no-default-features --features "$set" 2>&1); then
        echo "ok    $label"
    else
        echo "FAIL  $label"
        echo "$out" | grep -E "^error" -A6 | head -12 | sed 's/^/        /'
        fail=1
    fi
done
[ $fail -eq 0 ] && echo "feature-check: all ${#sets[@]} combinations build"
exit $fail
