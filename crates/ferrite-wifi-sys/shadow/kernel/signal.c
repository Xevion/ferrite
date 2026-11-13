// SPDX-License-Identifier: GPL-2.0-only
/*
 * Replaces kernel/signal.c: kernel threads take no signals, so none is ever pending.
 */

#include <linux/sched/signal.h>

void ignore_signals(struct task_struct *t)
{
	int i;

	for (i = 0; i < _NSIG; ++i)
		t->sighand->action[i].sa.sa_handler = SIG_IGN;
}
