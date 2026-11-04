/* gexcc runtime: sigsetjmp's mask (include/setjmp.h). save: before the
 * setjmp, the mask into the buffer when asked; post: on the second return
 * (a siglongjmp), the mask back. */
#include <setjmp.h>
#include <signal.h>

void __gexcc_sigsetjmp_save(struct __jmp_buf_tag *b, int save)
{
	b->__fl = 0;
	if (save) {
		b->__fl = 1;
		pthread_sigmask(SIG_BLOCK, 0, (sigset_t *)b->__ss);
	}
}

int __gexcc_sigsetjmp_post(struct __jmp_buf_tag *b, int r)
{
	if (r && b->__fl)
		pthread_sigmask(SIG_SETMASK, (sigset_t *)b->__ss, 0);
	return r;
}
