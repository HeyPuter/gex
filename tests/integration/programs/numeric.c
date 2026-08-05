// Numeric kernels chosen so gcc -O3 emits fmla-by-element, fccmp and the
// scalar AdvSIMD forms: matmul and dot products index one operand's lane,
// and the min/max reductions compile to conditional compares.
#include <stdio.h>
#include <stdlib.h>
#include <math.h>
#include <string.h>

#define N 96
static float A[N][N], B[N][N], C[N][N];

static void matmul(void) {
    memset(C, 0, sizeof C);
    for (int i = 0; i < N; i++)
        for (int k = 0; k < N; k++) {
            float a = A[i][k];
            for (int j = 0; j < N; j++) C[i][j] += a * B[k][j];
        }
}

static double dotd(const double *x, const double *y, int n) {
    double s = 0;
    for (int i = 0; i < n; i++) s += x[i] * y[i];
    return s;
}

// Conditional float compares: gcc turns the chained test into fccmp.
static int classify(double x, double lo, double hi) {
    if (x > lo && x < hi) return 1;
    if (x <= lo || isnan(x)) return 2;
    return 3;
}

int main(void) {
    unsigned s = 12345;
    for (int i = 0; i < N; i++)
        for (int j = 0; j < N; j++) {
            s = s * 1103515245u + 12345u;
            A[i][j] = (float)((s >> 16) % 1000) / 32.0f;
            s = s * 1103515245u + 12345u;
            B[i][j] = (float)((s >> 16) % 1000) / 32.0f;
        }
    matmul();
    double sum = 0, mx = -1e30, mn = 1e30;
    for (int i = 0; i < N; i++)
        for (int j = 0; j < N; j++) {
            sum += C[i][j];
            if (C[i][j] > mx) mx = C[i][j];
            if (C[i][j] < mn) mn = C[i][j];
        }
    printf("matmul sum=%.4f max=%.4f min=%.4f\n", sum, mx, mn);

    static double x[1024], y[1024];
    for (int i = 0; i < 1024; i++) { x[i] = i * 0.5 - 100.0; y[i] = 1.0 / (i + 1); }
    printf("dot=%.9f\n", dotd(x, y, 1024));

    int c[4] = {0};
    for (int i = 0; i < 1000; i++) {
        double v = (i % 7 == 0) ? nan("") : (double)i - 500.0;
        c[classify(v, -200.0, 200.0)]++;
    }
    printf("classify %d %d %d\n", c[1], c[2], c[3]);

    // Scalar AdvSIMD: gcc uses cmge/abs on d registers for 64-bit lanes.
    long long acc = 0;
    for (int i = 0; i < 4096; i++) { long long v = (i * 37) % 501 - 250; acc += llabs(v); }
    printf("absacc=%lld\n", acc);
    return 0;
}
