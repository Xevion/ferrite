// SPDX-License-Identifier: GPL-2.0-or-later
/*
 * Replaces kernel/pid.c: one namespace, and tasks found by walking init_task.tasks.
 * Adapted from Genode's lx_emul shadow (Stefan Kalkowski, Genode Labs).
 */

#include <linux/bug.h>
#include <linux/export.h>
#include <linux/pid.h>
#include <linux/sched/task.h>
#include <linux/pid_namespace.h>
#include <linux/sched/task.h>

struct task_struct *find_task_by_pid_ns(pid_t nr, struct pid_namespace *ns)
{
	struct task_struct *task;

	list_for_each_entry(task, &init_task.tasks, tasks)
		if (task->pid == nr)
			return task;
	return NULL;
}

struct task_struct *find_task_by_vpid(pid_t vnr)
{
	return find_task_by_pid_ns(vnr, NULL);
}

/* No struct pid is ever allocated here: task pid links stay NULL, so a pid is only ever NULL. */
pid_t pid_vnr(struct pid *pid)
{
	return pid_nr(pid);
}
EXPORT_SYMBOL_GPL(pid_vnr);

void put_pid(struct pid *pid)
{
	WARN_ON_ONCE(pid);
}
EXPORT_SYMBOL_GPL(put_pid);

pid_t __task_pid_nr_ns(struct task_struct *task, enum pid_type type, struct pid_namespace *ns)
{
	return type == PIDTYPE_PID ? task->pid : task->tgid;
}
EXPORT_SYMBOL(__task_pid_nr_ns);
