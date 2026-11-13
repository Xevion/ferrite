// SPDX-License-Identifier: GPL-2.0-or-later
/*
 * Replaces kernel/exit.c: a kernel thread ends by leaving the scheduler.
 * Adapted from Genode's lx_emul shadow (Stefan Kalkowski, Genode Labs).
 */

#include <linux/completion.h>
#include <linux/kernel.h>
#include <linux/kthread.h>
#include <linux/rcuwait.h>
#include <linux/sched.h>
#include <linux/sched/task.h>

#include <lx/task.h>

int rcuwait_wake_up(struct rcuwait *w)
{
	struct task_struct *task = rcu_dereference(w->task);

	return task ? wake_up_process(task) : 0;
}
EXPORT_SYMBOL_GPL(rcuwait_wake_up);

void __noreturn do_exit(long code)
{
	struct task_struct *tsk = current;

	tsk->exit_code = code;
	if (tsk->flags & PF_KTHREAD) {
		struct kthread *kthread = tsk_is_kthread(tsk);

		/* What kthread_stop() returns. */
		if (kthread)
			kthread_do_exit(kthread, code);
	}
	tsk->flags |= PF_EXITING | PF_NOFREEZE;
	set_special_state(TASK_DEAD);
	list_del_init(&tsk->tasks);

	/* kthread_stop() waits on this, holding a reference of its own. */
	if (tsk->vfork_done)
		complete(tsk->vfork_done);
	put_task_struct(tsk);
	lx_task_exit();
}
