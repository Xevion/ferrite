// SPDX-License-Identifier: GPL-2.0-or-later
/*
 * Replaces kernel/fork.c: a kernel thread is a task_struct plus a shim task.
 * Adapted from Genode's lx_emul shadow (Stefan Kalkowski, Genode Labs).
 *
 * Each task holds one reference to its own task_struct, dropped by do_exit.
 * Live tasks are linked on init_task.tasks, which pid lookup walks.
 */

#include <linux/cred.h>
#include <linux/sched/task.h>
#include <linux/kthread.h>
#include <linux/list.h>
#include <linux/refcount.h>
#include <linux/sched/signal.h>
#include <linux/sched/task.h>
#include <linux/slab.h>

#include <lx/task.h>

pid_t kernel_thread(int (*fn)(void *), void *arg, const char *name, unsigned long flags)
{
	static pid_t next_pid = 1;
	struct task_struct *task;
	struct cred *cred;
	struct signal_struct *signal;
	struct sighand_struct *sighand;

	cred = kzalloc(sizeof(*cred), GFP_KERNEL);
	signal = kzalloc(sizeof(*signal), GFP_KERNEL);
	sighand = kzalloc(sizeof(*sighand), GFP_KERNEL);
	task = kzalloc(sizeof(*task), GFP_KERNEL);
	if (!cred || !signal || !sighand || !task)
		goto fail;

	task->__state = TASK_RUNNING;
	refcount_set(&task->usage, 1);
	task->flags = PF_KTHREAD;
	task->prio = task->static_prio = task->normal_prio = MAX_PRIO - 20;
	task->policy = SCHED_NORMAL;
	task->cpus_ptr = &task->cpus_mask;
	task->cpus_mask = CPU_MASK_ALL;
	task->nr_cpus_allowed = 1;
	INIT_LIST_HEAD(&task->tasks);
	task->real_parent = current;
	task->parent = current;
	INIT_LIST_HEAD(&task->children);
	INIT_LIST_HEAD(&task->sibling);
	task->group_leader = task;
	task->pid = task->tgid = next_pid++;
	INIT_LIST_HEAD(&task->pending.list);
	task->cred = task->real_cred = cred;
	task->signal = signal;
	task->sighand = sighand;
	strscpy_pad(task->comm, name ? name : "kthread", sizeof(task->comm));

	if (!set_kthread_struct(task))
		goto fail;
	list_add_tail(&task->tasks, &init_task.tasks);

	if (lx_task_create(task, fn, arg, task->comm, task->prio)) {
		list_del(&task->tasks);
		free_kthread_struct(task);
		goto fail;
	}
	return task->pid;

fail:
	kfree(task);
	kfree(sighand);
	kfree(signal);
	kfree(cred);
	return -ENOMEM;
}

void __put_task_struct(struct task_struct *tsk)
{
	WARN_ON(refcount_read(&tsk->usage));
	WARN_ON(tsk == current);

	free_kthread_struct(tsk);
	kfree(tsk->sighand);
	kfree(tsk->signal);
	kfree(tsk->cred);
	kfree(tsk);
}
EXPORT_SYMBOL_GPL(__put_task_struct);

void __put_task_struct_rcu_cb(struct rcu_head *rhp)
{
	__put_task_struct(container_of(rhp, struct task_struct, rcu));
}
EXPORT_SYMBOL_GPL(__put_task_struct_rcu_cb);
