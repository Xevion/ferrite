// SPDX-License-Identifier: GPL-2.0-or-later
/*
 * Replaces init/init_task.c: the task_struct the main stack runs as.
 * Adapted from Genode's lx_emul start.c (Stefan Kalkowski, Genode Labs).
 */

#include <linux/cred.h>
#include <linux/sched/task.h>
#include <linux/sched.h>
#include <linux/sched/signal.h>

static struct cred init_cred_storage;
static struct signal_struct init_signals;
static struct sighand_struct init_sighand;

struct task_struct init_task = {
	.__state	= TASK_RUNNING,
	.usage		= REFCOUNT_INIT(2),
	.flags		= PF_KTHREAD,
	.prio		= MAX_PRIO - 20,
	.static_prio	= MAX_PRIO - 20,
	.normal_prio	= MAX_PRIO - 20,
	.policy		= SCHED_NORMAL,
	.cpus_ptr	= &init_task.cpus_mask,
	.cpus_mask	= CPU_MASK_ALL,
	.nr_cpus_allowed = 1,
	.tasks		= LIST_HEAD_INIT(init_task.tasks),
	.real_parent	= &init_task,
	.parent		= &init_task,
	.children	= LIST_HEAD_INIT(init_task.children),
	.sibling	= LIST_HEAD_INIT(init_task.sibling),
	.group_leader	= &init_task,
	.comm		= "swapper",
	.pending	= {
		.list	= LIST_HEAD_INIT(init_task.pending.list),
	},
	.real_cred	= &init_cred_storage,
	.cred		= &init_cred_storage,
	.signal		= &init_signals,
	.sighand	= &init_sighand,
};
EXPORT_SYMBOL(init_task);
