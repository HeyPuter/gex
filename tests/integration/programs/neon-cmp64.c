/* cmhi/cmhs on 64-bit lanes: wasm has no i64x2.gt_u, so gex biases the
   operands and uses the signed comparison. The values here straddle the
   sign bit, which is exactly where a signed comparison would give the
   wrong answer if the bias were missing. */
#include <arm_neon.h>
#include <stdio.h>
int main(void) {
    uint64_t av[] = {1, 0xffffffffffffffffULL, 0x8000000000000000ULL, 5};
    uint64_t bv[] = {2, 1, 0x7fffffffffffffffULL, 5};
    for (int i = 0; i < 4; i += 2) {
        uint64x2_t a = vld1q_u64(&av[i]), b = vld1q_u64(&bv[i]);
        uint64x2_t hi = vcgtq_u64(a, b), hs = vcgeq_u64(a, b);
        printf("hi %llx %llx  hs %llx %llx\n",
               (unsigned long long)vgetq_lane_u64(hi, 0),
               (unsigned long long)vgetq_lane_u64(hi, 1),
               (unsigned long long)vgetq_lane_u64(hs, 0),
               (unsigned long long)vgetq_lane_u64(hs, 1));
    }
    return 0;
}
