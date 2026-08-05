/* Auto-vectorising loops, for the NEON path (PLAN §5.7).
 *
 * Compiled at -O3 these become NEON: lane-wise arithmetic, horizontal
 * reductions, shuffles and the load/store forms. The point is not the
 * arithmetic but that gcc's vectoriser picks the instructions, so the corpus
 * tracks what real compilers emit rather than what we thought to test.
 */
__attribute__((noinline)) static int isum(const int *a, int n){ int s=0; for(int i=0;i<n;i++) s+=a[i]; return s; }
__attribute__((noinline)) static float fsum(const float *a, int n){ float s=0; for(int i=0;i<n;i++) s+=a[i]*a[i]; return s; }
__attribute__((noinline)) static void saxpy(float *y,const float *x,float a,int n){ for(int i=0;i<n;i++) y[i]=a*x[i]+y[i]; }
__attribute__((noinline)) static int imax(const int *a,int n){ int m=a[0]; for(int i=1;i<n;i++) if(a[i]>m) m=a[i]; return m; }
__attribute__((noinline)) static int cnt(const unsigned char *a,int n){ int c=0; for(int i=0;i<n;i++) c+=__builtin_popcount(a[i]); return c; }
__attribute__((noinline)) static void rev(unsigned *d,const unsigned *s,int n){ for(int i=0;i<n;i++) d[i]=__builtin_bswap32(s[i]); }
int main(void){
    enum { N = 1024 };
    static int ia[N]; static float fa[N], fb[N]; static unsigned char ba[N]; static unsigned ua[N], ub[N];
    unsigned seed=7;
    for (int i=0;i<N;i++){ seed=seed*1103515245u+12345u; ia[i]=(int)(seed>>17)-16384; fa[i]=(float)((seed>>20)%1000)/7.0f; fb[i]=1.0f; ba[i]=(unsigned char)(seed>>8); ua[i]=seed; }
    printf("isum=%d\n", isum(ia,N));
    printf("fsum=%.4f\n", fsum(fa,N));
    saxpy(fb,fa,2.5f,N); printf("saxpy=%.4f %.4f\n", fb[0], fb[N-1]);
    printf("imax=%d\n", imax(ia,N));
    printf("popcount=%d\n", cnt(ba,N));
    rev(ub,ua,N); printf("bswap=%08x %08x\n", ub[0], ub[N-1]);
    double d=0; for(int i=0;i<N;i++) d += (double)fa[i]/(1.0+i);
    printf("dsum=%.8f\n", d);
    return 0;
}
